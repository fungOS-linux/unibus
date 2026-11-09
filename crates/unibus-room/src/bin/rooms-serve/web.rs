//! The HTTP half of `rooms-serve`: one page, one scene document, one small
//! signaling relay. It knows nothing about Unibus; the binary owns membership
//! and hands this module a flat snapshot.

use std::collections::{HashMap, VecDeque};
use std::hash::{Hash, Hasher};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::Command as ProcessCommand;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use unibus_room::MAX_MESSAGE;

use crate::Command;

const PAGE: &str = include_str!("../../../web/rooms.html");
const MAX_LINE: u64 = 16 << 10;
const MAX_BODY: usize = 64 << 10;
const MAX_SIDE: u32 = 4096;
const PEER_TTL: Duration = Duration::from_secs(8);
const MAX_SIGNALS: usize = 128;

pub struct Paths {
    pub view: PathBuf,
    pub ui: PathBuf,
    pub renderer: PathBuf,
}

pub struct State {
    snapshot: Mutex<Value>,
    cache: Mutex<Option<(String, String)>>,
    peers: Mutex<HashMap<String, Peer>>,
    signals: Mutex<HashMap<String, VecDeque<Value>>>,
    calls: Mutex<HashMap<String, BrowserCall>>,
    temporary: AtomicU64,
}

struct Peer {
    name: String,
    in_call: bool,
    seen: Instant,
}

struct BrowserCall {
    call: Value,
    seen: Instant,
}

impl State {
    pub fn new() -> Self {
        Self {
            snapshot: Mutex::new(json!({})),
            cache: Mutex::new(None),
            peers: Mutex::new(HashMap::new()),
            signals: Mutex::new(HashMap::new()),
            calls: Mutex::new(HashMap::new()),
            temporary: AtomicU64::new(1),
        }
    }

    pub fn set_snapshot(&self, snapshot: Value) {
        if let Ok(mut current) = self.snapshot.lock() {
            *current = snapshot;
        }
    }

    /// Each browser owns its own call panel. The room still has one membership
    /// for this host, but two browser presenters must not inherit each other's
    /// microphone state.
    pub fn snapshot_for(&self, session: &str) -> Value {
        let mut snapshot = self
            .snapshot
            .lock()
            .map(|current| current.clone())
            .unwrap_or_else(|_| json!({}));
        if !snapshot.is_object() {
            snapshot = json!({});
        }
        if !session.is_empty() {
            snapshot["call"] = self.call_for(session);
        }
        snapshot
    }

    pub fn apply_call(&self, session: &str, patch: &Value) {
        let Some(patch) = patch.as_object() else {
            return;
        };
        let Ok(mut calls) = self.calls.lock() else {
            return;
        };
        let entry = calls
            .entry(session.to_owned())
            .or_insert_with(|| BrowserCall {
                call: idle_call(),
                seen: Instant::now(),
            });
        for (key, value) in patch {
            match key.as_str() {
                "state" => {
                    if let Some(state) = value.as_str()
                        && matches!(
                            state,
                            "idle" | "starting" | "connected" | "muted" | "failed"
                        )
                    {
                        entry.call["state"] = json!(state);
                        if state == "idle" {
                            entry.call = idle_call();
                        }
                    }
                }
                "muted" => {
                    if let Some(muted) = value.as_bool() {
                        entry.call["muted"] = json!(muted);
                        if is_live(&entry.call) {
                            entry.call["state"] = json!(if muted { "muted" } else { "connected" });
                        }
                    }
                }
                "mic_rms" => {
                    entry.call["mic_rms"] = json!(value.as_f64().unwrap_or(0.0).clamp(0.0, 1.0));
                }
                "peer_count" => {
                    entry.call["peer_count"] = json!(value.as_u64().unwrap_or(0).min(32));
                }
                "frame_count" => {
                    entry.call["frame_count"] =
                        json!(value.as_u64().unwrap_or(0).min(1_000_000_000));
                }
                "error" => entry.call["error"] = value.clone(),
                "route" => {
                    if let Some(route) = value.as_str() {
                        entry.call["route"] = json!(route);
                    }
                }
                "format" => {
                    if let Some(format) = value.as_str() {
                        entry.call["format"] = json!(format);
                    }
                }
                "frame_ms" => {
                    entry.call["frame_ms"] = json!(value.as_u64().unwrap_or(20).clamp(10, 2000));
                }
                _ => {}
            }
        }
        entry.seen = Instant::now();
        calls.retain(|_, entry| {
            is_live(&entry.call) || entry.seen.elapsed() < Duration::from_secs(30)
        });
    }

    fn call_for(&self, session: &str) -> Value {
        self.calls
            .lock()
            .ok()
            .and_then(|calls| calls.get(session).map(|entry| entry.call.clone()))
            .unwrap_or_else(idle_call)
    }

    /// The room's one membership reflects whether any browser presenter is
    /// publishing. Totals are for presence and the speaking indicator, not a
    /// second media path.
    pub fn aggregate_call(&self) -> Value {
        let Ok(calls) = self.calls.lock() else {
            return idle_call();
        };
        let mut live = calls
            .values()
            .filter(|entry| is_live(&entry.call))
            .collect::<Vec<_>>();
        if live.is_empty() {
            return idle_call();
        }
        live.sort_by_key(|entry| std::cmp::Reverse(entry.seen));
        let mut call = live[0].call.clone();
        let peers = live
            .iter()
            .map(|entry| entry.call["peer_count"].as_u64().unwrap_or(0))
            .sum::<u64>()
            .min(32);
        let rms = live
            .iter()
            .map(|entry| entry.call["mic_rms"].as_f64().unwrap_or(0.0))
            .fold(0.0_f64, f64::max);
        let any_unmuted = live.iter().any(|entry| entry.call["muted"] != true);
        call["peer_count"] = json!(peers);
        call["mic_rms"] = json!(rms);
        call["muted"] = json!(!any_unmuted);
        call["state"] = json!(if any_unmuted {
            if live.iter().any(|entry| entry.call["state"] == "starting") {
                "starting"
            } else {
                "connected"
            }
        } else {
            "muted"
        });
        call
    }

    /// A browser that disappeared mid-call must not leave the room looking
    /// busy. Mark it failed once its heartbeat is stale.
    pub fn expire_calls(&self) -> bool {
        let Ok(mut calls) = self.calls.lock() else {
            return false;
        };
        let mut changed = false;
        for entry in calls.values_mut() {
            if is_live(&entry.call) && entry.seen.elapsed() > Duration::from_secs(15) {
                entry.call["state"] = json!("failed");
                entry.call["muted"] = json!(false);
                entry.call["mic_rms"] = json!(0.0);
                entry.call["error"] = json!("browser stopped reporting the call");
                changed = true;
            }
        }
        changed
    }
}

fn idle_call() -> Value {
    json!({
        "state": "idle",
        "route": "browser-webrtc",
        "format": "Opus 48 kHz",
        "frame_ms": 20,
        "muted": false,
        "error": Value::Null,
        "peer_count": 0,
        "frame_count": 0,
        "mic_rms": 0.0
    })
}

fn is_live(call: &Value) -> bool {
    matches!(
        call["state"].as_str(),
        Some("starting" | "connected" | "muted")
    )
}

pub fn listen(
    bind: &str,
    state: Arc<State>,
    paths: Arc<Paths>,
    commands: Sender<Command>,
) -> Result<(), String> {
    let listener = TcpListener::bind(bind).map_err(|error| format!("bind {bind}: {error}"))?;
    eprintln!(
        "rooms-serve: http://{bind}/ (view {})",
        paths.view.display()
    );
    std::thread::spawn(move || {
        for connection in listener.incoming() {
            match connection {
                Ok(stream) => {
                    let state = Arc::clone(&state);
                    let paths = Arc::clone(&paths);
                    let commands = commands.clone();
                    // A stalled browser costs one thread, never the room loop.
                    std::thread::spawn(move || {
                        if let Err(error) = handle(stream, &state, &paths, &commands) {
                            eprintln!("rooms-serve: {error}");
                        }
                    });
                }
                Err(error) => eprintln!("rooms-serve: accept: {error}"),
            }
        }
    });
    Ok(())
}

struct Request {
    method: String,
    path: String,
    query: String,
    headers: String,
    body: Vec<u8>,
}

fn handle(
    mut stream: TcpStream,
    state: &State,
    paths: &Paths,
    commands: &Sender<Command>,
) -> Result<(), String> {
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .map_err(|error| format!("read timeout: {error}"))?;
    let _ = stream.set_nodelay(true);
    let request = read_request(&mut stream)?;

    match (request.method.as_str(), request.path.as_str()) {
        ("GET" | "HEAD", "/") | ("GET" | "HEAD", "/index.html") => respond(
            &mut stream,
            200,
            "text/html; charset=utf-8",
            PAGE,
            &[("cache-control", "no-store")],
        ),
        ("GET" | "HEAD", "/scene.json") => scene(&mut stream, state, paths, &request),
        ("GET" | "HEAD", "/api/state") => {
            let session = query_text(&request.query, "session");
            let snapshot = state.snapshot_for(&session);
            respond_json(&mut stream, 200, &snapshot.to_string())
        }
        ("POST", "/api/action") => {
            let body: Value = serde_json::from_slice(&request.body)
                .map_err(|error| format!("action body: {error}"))?;
            let id = body["id"]
                .as_str()
                .ok_or("action needs a string id")?
                .to_owned();
            if id.len() > 128 {
                return respond(&mut stream, 400, "text/plain", "action id too long", &[]);
            }
            let text = body["text"].as_str().map(str::to_owned);
            if text.as_ref().is_some_and(|text| text.len() > MAX_MESSAGE) {
                return respond(&mut stream, 400, "text/plain", "message too long", &[]);
            }
            let _ = commands.send(Command::Action { id, text });
            respond_json(&mut stream, 200, "{\"ok\":true}")
        }
        ("POST", "/api/call") => {
            let body: Value = serde_json::from_slice(&request.body)
                .map_err(|error| format!("call body: {error}"))?;
            let session = body["session"].as_str().unwrap_or("").to_owned();
            if session.is_empty() || session.len() > 128 || !body.is_object() {
                return respond(&mut stream, 400, "text/plain", "bad call update", &[]);
            }
            state.apply_call(&session, &body);
            let _ = commands.send(Command::Call);
            respond_json(&mut stream, 200, "{\"ok\":true}")
        }
        ("POST", "/api/peers") => peers(&mut stream, state, &request),
        ("POST", "/api/signal") => signal_post(&mut stream, state, &request),
        ("GET", "/api/signal") => signal_get(&mut stream, state, &request),
        _ => respond(&mut stream, 404, "text/plain", "not found", &[]),
    }
}

fn read_request(stream: &mut TcpStream) -> Result<Request, String> {
    let mut reader = BufReader::new(stream.try_clone().map_err(|error| error.to_string())?);
    let mut line = String::new();
    reader
        .by_ref()
        .take(MAX_LINE)
        .read_line(&mut line)
        .map_err(|error| format!("read request line: {error}"))?;
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_owned();
    let target = parts.next().unwrap_or_default().to_owned();
    let (path, query) = target
        .split_once('?')
        .map_or((target.as_str(), ""), |(path, query)| (path, query));

    let mut headers = String::new();
    loop {
        let mut header = String::new();
        let read = reader
            .by_ref()
            .take(MAX_LINE)
            .read_line(&mut header)
            .map_err(|error| format!("read header: {error}"))?;
        if read == 0 || header.trim().is_empty() {
            break;
        }
        headers.push_str(&header);
        if headers.len() > MAX_LINE as usize {
            return Err("headers too large".into());
        }
    }
    let length = header(&headers, "content-length")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(0);
    if length > MAX_BODY {
        return Err("request body too large".into());
    }
    let mut body = vec![0; length];
    reader
        .read_exact(&mut body)
        .map_err(|error| format!("read body: {error}"))?;
    Ok(Request {
        method,
        path: path.to_owned(),
        query: query.to_owned(),
        headers,
        body,
    })
}

fn scene(
    stream: &mut TcpStream,
    state: &State,
    paths: &Paths,
    request: &Request,
) -> Result<(), String> {
    let width = query_value(&request.query, "w")
        .unwrap_or(1182)
        .clamp(1, MAX_SIDE);
    let height = query_value(&request.query, "h")
        .unwrap_or(800)
        .clamp(1, MAX_SIDE);
    let session = query_text(&request.query, "session");
    let snapshot = state.snapshot_for(&session);
    let source = snapshot.to_string();
    let key = format!("{width}x{height}:{source}");
    let cached = {
        let guard = state.cache.lock().map_err(|error| error.to_string())?;
        match guard.as_ref() {
            Some((candidate, body)) if candidate == &key => Some(body.clone()),
            _ => None,
        }
    };
    let body = match cached {
        Some(body) => body,
        None => {
            let body = render_scene(state, paths, &snapshot, width, height)?;
            if let Ok(mut guard) = state.cache.lock() {
                *guard = Some((key, body.clone()));
            }
            body
        }
    };
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    body.hash(&mut hasher);
    let etag = format!("\"{:x}-{}\"", hasher.finish(), body.len());
    if header(&request.headers, "if-none-match")
        .is_some_and(|value| value.split(',').any(|tag| tag.trim() == etag))
    {
        return respond(stream, 304, "application/json", "", &[("etag", &etag)]);
    }
    respond(
        stream,
        200,
        "application/json",
        &body,
        &[("etag", &etag), ("cache-control", "no-cache")],
    )
}

fn render_scene(
    state: &State,
    paths: &Paths,
    snapshot: &Value,
    width: u32,
    height: u32,
) -> Result<String, String> {
    let id = state.temporary.fetch_add(1, Ordering::Relaxed);
    let directory = std::env::temp_dir().join(format!("rooms-scene-{}-{id}", std::process::id()));
    std::fs::create_dir_all(&directory)
        .map_err(|error| format!("create {}: {error}", directory.display()))?;
    let snapshot_path = directory.join("snapshot.lua");
    let output_path = directory.join("scene.json");
    std::fs::write(&snapshot_path, format!("return {}", lua(snapshot)))
        .map_err(|error| format!("write {}: {error}", snapshot_path.display()))?;

    let result = ProcessCommand::new("lua")
        .arg(&paths.renderer)
        .arg(&paths.view)
        .arg(&snapshot_path)
        .arg(&paths.ui)
        .arg(&output_path)
        .arg(width.to_string())
        .arg(height.to_string())
        .output()
        .map_err(|error| format!("run lua {}: {error}", paths.renderer.display()))?;
    let body = if result.status.success() {
        std::fs::read_to_string(&output_path).map_err(|error| format!("read scene: {error}"))
    } else {
        Err(format!(
            "renderer failed: {}{}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        ))
    };
    let _ = std::fs::remove_dir_all(&directory);
    body
}

/// JSON to Lua 5.4 table literal. The snapshot only contains JSON values, and
/// the renderer is loaded as Lua data; no code is ever carried through this.
fn lua(value: &Value) -> String {
    match value {
        Value::Null => "nil".into(),
        Value::Bool(value) => value.to_string(),
        Value::Number(value) => value.to_string(),
        Value::String(value) => {
            let mut out = String::from("\"");
            for character in value.chars() {
                match character {
                    '"' => out.push_str("\\\""),
                    '\\' => out.push_str("\\\\"),
                    '\n' => out.push_str("\\n"),
                    '\r' => out.push_str("\\r"),
                    '\t' => out.push_str("\\t"),
                    character if character < ' ' => {
                        out.push_str(&format!("\\u{{{:x}}}", character as u32))
                    }
                    character => out.push(character),
                }
            }
            out.push('"');
            out
        }
        Value::Array(items) => format!(
            "{{{}}}",
            items.iter().map(lua).collect::<Vec<_>>().join(",")
        ),
        Value::Object(object) => format!(
            "{{{}}}",
            object
                .iter()
                .map(|(key, value)| format!(
                    "[{}]={}",
                    lua(&Value::String(key.clone())),
                    lua(value)
                ))
                .collect::<Vec<_>>()
                .join(",")
        ),
    }
}

fn peers(stream: &mut TcpStream, state: &State, request: &Request) -> Result<(), String> {
    let body: Value =
        serde_json::from_slice(&request.body).map_err(|error| format!("peer body: {error}"))?;
    let id = body["id"].as_str().ok_or("peer needs an id")?;
    let name = body["name"].as_str().unwrap_or("Guest");
    if id.len() > 128 || name.len() > 64 {
        return respond(stream, 400, "text/plain", "peer identity too long", &[]);
    }
    let in_call = body["in_call"].as_bool().unwrap_or(false);
    let now = Instant::now();
    let mut peers = state.peers.lock().map_err(|error| error.to_string())?;
    peers.retain(|_, peer| now.duration_since(peer.seen) < PEER_TTL);
    peers.insert(
        id.to_owned(),
        Peer {
            name: name.to_owned(),
            in_call,
            seen: now,
        },
    );
    let list = peers
        .iter()
        .map(|(id, peer)| json!({"id": id, "name": peer.name, "in_call": peer.in_call}))
        .collect::<Vec<_>>();
    respond_json(stream, 200, &json!({"peers": list}).to_string())
}

fn signal_post(stream: &mut TcpStream, state: &State, request: &Request) -> Result<(), String> {
    let body: Value =
        serde_json::from_slice(&request.body).map_err(|error| format!("signal body: {error}"))?;
    let from = body["from"].as_str().unwrap_or("");
    let to = body["to"].as_str().unwrap_or("");
    let kind = body["kind"].as_str().unwrap_or("");
    if from.is_empty() || to.is_empty() || !matches!(kind, "offer" | "answer" | "ice") {
        return respond(stream, 400, "text/plain", "bad signal", &[]);
    }
    let message = json!({"from": from, "kind": kind, "data": body["data"]});
    let mut signals = state.signals.lock().map_err(|error| error.to_string())?;
    let queue = signals.entry(to.to_owned()).or_default();
    if queue.len() >= MAX_SIGNALS {
        queue.pop_front();
    }
    queue.push_back(message);
    respond_json(stream, 200, "{\"ok\":true}")
}

fn signal_get(stream: &mut TcpStream, state: &State, request: &Request) -> Result<(), String> {
    let peer = query_text(&request.query, "peer");
    if peer.is_empty() {
        return respond(stream, 400, "text/plain", "signal needs a peer", &[]);
    }
    let mut signals = state.signals.lock().map_err(|error| error.to_string())?;
    let messages = signals
        .remove(&peer)
        .map(|queue| queue.into_iter().collect::<Vec<_>>())
        .unwrap_or_default();
    respond_json(stream, 200, &json!({"messages": messages}).to_string())
}

fn respond_json(stream: &mut TcpStream, status: u16, body: &str) -> Result<(), String> {
    respond(
        stream,
        status,
        "application/json",
        body,
        &[("cache-control", "no-store")],
    )
}

fn respond(
    stream: &mut TcpStream,
    status: u16,
    content_type: &str,
    body: &str,
    headers: &[(&str, &str)],
) -> Result<(), String> {
    let reason = match status {
        200 => "OK",
        304 => "Not Modified",
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        _ => "Internal Server Error",
    };
    let mut head = format!(
        "HTTP/1.1 {status} {reason}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n",
        body.len()
    );
    for (name, value) in headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");
    stream
        .write_all(head.as_bytes())
        .map_err(|error| error.to_string())?;
    if status != 304 {
        stream
            .write_all(body.as_bytes())
            .map_err(|error| error.to_string())?;
    }
    Ok(())
}

fn header<'a>(headers: &'a str, name: &str) -> Option<&'a str> {
    headers.lines().find_map(|line| {
        let (key, value) = line.split_once(':')?;
        key.trim().eq_ignore_ascii_case(name).then(|| value.trim())
    })
}

fn query_value(query: &str, name: &str) -> Option<u32> {
    query_text(query, name).parse().ok()
}

fn query_text(query: &str, name: &str) -> String {
    query
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .find(|(key, _)| *key == name)
        .map(|(_, value)| percent_decode(value))
        .unwrap_or_default()
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%'
            && index + 2 < bytes.len()
            && let Ok(byte) = u8::from_str_radix(&value[index + 1..index + 3], 16)
        {
            out.push(byte);
            index += 3;
            continue;
        }
        out.push(if bytes[index] == b'+' {
            b' '
        } else {
            bytes[index]
        });
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_browser_gets_its_own_call_panel() {
        let state = State::new();
        state.set_snapshot(json!({"call": idle_call(), "room": {"id": "studio"}}));
        state.apply_call(
            "alice",
            &json!({
                "state": "connected", "muted": false, "mic_rms": 0.25, "peer_count": 2
            }),
        );
        assert_eq!(state.snapshot_for("alice")["call"]["state"], "connected");
        assert_eq!(state.snapshot_for("alice")["call"]["peer_count"], 2);
        assert_eq!(state.snapshot_for("bob")["call"]["state"], "idle");
        assert_eq!(state.aggregate_call()["state"], "connected");

        state.apply_call("alice", &json!({"muted": true}));
        assert_eq!(state.aggregate_call()["state"], "muted");
        state.apply_call("alice", &json!({"state": "idle"}));
        assert_eq!(state.aggregate_call()["state"], "idle");
    }

    #[test]
    fn snapshot_is_serialized_as_data_not_lua_code() {
        let source = lua(&json!({"text": "a\"b\\c\n", "ok": true, "none": null}));
        assert!(source.contains("[\"none\"]=nil"));
        assert!(source.contains("[\"ok\"]=true"));
        assert!(source.contains(r#"["text"]="a\"b\\c\n""#));
        assert!(!source.contains("function"));
    }
}
