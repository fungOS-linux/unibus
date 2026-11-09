//! Reference Rooms host: Unibus room state -> Canvas module snapshot -> actions.
//!
//! This is deliberately outside the room service. The service carries no media
//! bytes and does not know Canvas exists; this process owns the local
//! presentation binding, the local call harness, and the action edge that a
//! scripted Canvas module cannot return over its own bus.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::{UnixDatagram, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail, ensure};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use unibus::{
    Envelope, Message,
    client::{Client, ClientConfig},
    kinds,
};
use unibus_room::{
    ADDRESS_KIND, Address, BusClass, ERROR_KIND, JOIN_KIND, Join, LEAVE_KIND, LIST_KIND,
    LIST_REPLY_KIND, MEMBER_KIND, MESSAGE_KIND, MemberEvent, Mic, Presence, Presenter, SELF_KIND,
    SelfState, WELCOME_KIND, Welcome, host::Projection,
};

const STREAM_SCHEMA_VERSION: u32 = 1;

struct Args {
    config: PathBuf,
    room: Option<String>,
    name: String,
    workspace: String,
    presenter: Presenter,
    surface: String,
    spec: PathBuf,
    canvas_socket: PathBuf,
    call_bin: PathBuf,
    call: bool,
    seconds: u64,
    verbose: bool,
    agent: String,
    address_text: String,
    transcript: Option<PathBuf>,
}

fn main() -> Result<()> {
    let args = parse_args()?;
    let client = Client::connect(client_config(&args.config)?)?;
    let canvas = Canvas::new(args.canvas_socket.clone());
    canvas.ensure_rooms_surface(&args.surface, &args.spec)?;

    let mut projection = Projection::default();
    if let Some(path) = &args.transcript {
        load_transcript(&mut projection, path)?;
    }
    let mut call = CallManager::new(args.call, args.call_bin.clone());
    send(&client, LIST_KIND, json!({}))?;
    if let Some(room) = &args.room {
        join(&client, room, &args.name, args.presenter)?;
    }

    let deadline = (args.seconds > 0).then(|| Instant::now() + Duration::from_secs(args.seconds));
    let mut next_canvas_poll = Instant::now();
    let mut next_list = Instant::now() + Duration::from_secs(5);
    let mut next_publish = Instant::now();
    let mut dirty = true;
    let mut sent_join: Option<String> = None;
    let mut sent_leave: Option<String> = None;
    let mut last_here: Option<String> = None;
    let mut last_request: Option<(String, Instant)> = None;
    let mut last_self = (Mic::On, false);
    let mut sequence = now_ms();

    loop {
        while let Some((envelope, _)) = client.try_read()? {
            match envelope.message.kind.as_str() {
                WELCOME_KIND => {
                    let welcome: Welcome = decode(&envelope)?;
                    if args.verbose {
                        eprintln!(
                            "room-host: welcome {} ({:?})",
                            welcome.room, welcome.me.role
                        );
                    }
                    if projection.room() != Some(welcome.room.as_str()) {
                        call.stop();
                    }
                    projection.apply_welcome(welcome);
                    sent_join = None;
                    sent_leave = None;
                    dirty = true;
                }
                MEMBER_KIND => {
                    let event: MemberEvent = decode(&envelope)?;
                    projection.apply_member(event);
                    dirty = true;
                }
                MESSAGE_KIND => {
                    projection.apply_message(decode(&envelope)?);
                    dirty = true;
                }
                LIST_REPLY_KIND => {
                    let reply: unibus_room::ListReply = decode(&envelope)?;
                    projection.apply_list(reply);
                    dirty = true;
                }
                ERROR_KIND => {
                    let error: Value = decode(&envelope)?;
                    let code = error["code"].as_str().unwrap_or("error");
                    let message = error["message"].as_str().unwrap_or("room command failed");
                    projection.set_error(Some(format!("{code}: {message}")));
                    dirty = true;
                }
                kinds::REPLY
                    if envelope.message.body.get("success") == Some(&Value::Bool(false)) =>
                {
                    projection.set_error(Some(format!(
                        "hub rejected room command: {:?}",
                        envelope.message.body
                    )));
                    dirty = true;
                }
                _ => {}
            }
        }

        if call.poll() {
            dirty = true;
        }

        if Instant::now() >= next_canvas_poll {
            next_canvas_poll = Instant::now() + Duration::from_millis(400);
            match canvas.scene_buttons(&args.surface) {
                Ok(buttons) => {
                    let action = SceneAction::read(&buttons);
                    let leaving =
                        action.here.is_none() && last_here.is_some() && projection.room().is_some();
                    projection.set_selected(action.selected(leaving));
                    if let Some(room) = action.here.clone() {
                        last_here = Some(room.clone());
                        if projection.room() != Some(room.as_str())
                            && sent_join.as_deref() != Some(room.as_str())
                        {
                            if args.verbose {
                                eprintln!("room-host: join {room}");
                            }
                            join(&client, &room, &args.name, args.presenter)?;
                            sent_join = Some(room);
                            sent_leave = None;
                            dirty = true;
                        }
                    } else if last_here.is_some()
                        && let Some(room) = projection.room().map(str::to_owned)
                        && sent_leave.as_deref() != Some(room.as_str())
                    {
                        if args.verbose {
                            eprintln!("room-host: leave {room}");
                        }
                        send(&client, LEAVE_KIND, json!({}))?;
                        projection.clear_room();
                        sent_leave = Some(room);
                        sent_join = None;
                        call.stop();
                        dirty = true;
                    }

                    if let Some(request) = action.request.clone() {
                        let repeat = last_request.as_ref().is_some_and(|(previous, at)| {
                            previous == &request && at.elapsed() < Duration::from_secs(1)
                        });
                        if !repeat {
                            match request.as_str() {
                                "start" => {
                                    call.start();
                                }
                                "stop" => {
                                    call.stop();
                                    projection.set_self(Mic::On, false, None);
                                }
                                "mute" => {
                                    call.set_muted(true);
                                    projection.set_self(Mic::Off, false, None);
                                }
                                "unmute" => {
                                    call.set_muted(false);
                                    projection.set_self(Mic::On, false, None);
                                }
                                "agent" => {
                                    send(
                                        &client,
                                        ADDRESS_KIND,
                                        serde_json::to_value(Address {
                                            target: args.agent.clone(),
                                            text: args.address_text.clone(),
                                        })?,
                                    )?;
                                    if let Some(path) = &args.transcript
                                        && let Some(welcome) = projection.welcome()
                                    {
                                        let room = welcome.room.clone();
                                        let speaker_id = welcome.me.id.clone();
                                        let speaker_name = welcome.me.display_name.clone();
                                        let now = now_ms();
                                        let segment_id = format!("{speaker_id}-{now}");
                                        append_transcript(
                                            path,
                                            json!({
                                                "schema_version": 1,
                                                "kind": "room.transcript.v1",
                                                "room": room,
                                                "revision": projection.revision(),
                                                "segment_id": segment_id,
                                                "speaker_id": speaker_id,
                                                "speaker_name": speaker_name,
                                                "start_ms": now,
                                                "end_ms": now,
                                                "text": args.address_text,
                                                "confidence": 1.0,
                                                "language": "en",
                                                "model": "addressed-text",
                                                "final": true
                                            }),
                                        )?;
                                        projection.add_message(
                                            &room,
                                            &speaker_name,
                                            &args.address_text,
                                            true,
                                            &segment_id,
                                        );
                                    }
                                }
                                "holodeck" => {
                                    send_self(
                                        &client,
                                        Presence::Active,
                                        Mic::On,
                                        false,
                                        Some(Presenter::Holodeck),
                                    )?;
                                    projection.set_self(Mic::On, false, Some(Presenter::Holodeck));
                                }
                                _ => {}
                            }
                            last_request = Some((request, Instant::now()));
                            dirty = true;
                        }
                    } else {
                        last_request = None;
                    }
                }
                Err(error) => eprintln!("room-host: Canvas scene unavailable: {error:#}"),
            }
        }

        if Instant::now() >= next_list {
            next_list = Instant::now() + Duration::from_secs(5);
            let _ = send(&client, LIST_KIND, json!({}));
        }

        let call_json = call.snapshot();
        let speaking = call_json["state"] == "connected"
            && call_json["mic_rms"].as_f64().unwrap_or(0.0) > 0.01;
        let mic = if call_json["muted"] == true {
            Mic::Off
        } else {
            Mic::On
        };
        if (mic, speaking) != last_self && projection.welcome().is_some() {
            send_self(&client, Presence::Active, mic, speaking, None)?;
            projection.set_self(mic, speaking, None);
            last_self = (mic, speaking);
            dirty = true;
        }

        if dirty || Instant::now() >= next_publish {
            next_publish = Instant::now() + Duration::from_secs(2);
            sequence = sequence.saturating_add(1);
            let snapshot = projection.snapshot(&args.workspace, &args.name, true, call_json);
            if args.verbose {
                eprintln!("room-host: publish {}", serde_json::to_string(&snapshot)?);
            }
            canvas.publish("soil", "rooms", sequence, json!({ "snapshot": snapshot }))?;
            dirty = false;
        }

        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }

    call.stop();
    if projection.welcome().is_some() {
        let _ = send(&client, LEAVE_KIND, json!({}));
    }
    Ok(())
}

fn send(client: &Client, kind: &str, body: Value) -> Result<()> {
    let envelope = client.envelope(None, Message::new(kind, body))?;
    client.send(&envelope, &[])?;
    Ok(())
}

fn join(client: &Client, room: &str, name: &str, presenter: Presenter) -> Result<()> {
    send(
        client,
        JOIN_KIND,
        serde_json::to_value(Join {
            room: room.to_owned(),
            space: None,
            display_name: name.to_owned(),
            presenter,
            presence: Presence::Active,
            audio: BusClass::Speech,
        })?,
    )
}

fn send_self(
    client: &Client,
    presence: Presence,
    mic: Mic,
    speaking: bool,
    presenter: Option<Presenter>,
) -> Result<()> {
    send(
        client,
        SELF_KIND,
        serde_json::to_value(SelfState {
            presence,
            mic,
            speaking,
            presenter,
        })?,
    )
}

fn decode<T: DeserializeOwned>(envelope: &Envelope) -> Result<T> {
    serde_json::from_value(Value::Object(envelope.message.body.clone()))
        .with_context(|| format!("decode {}", envelope.message.kind))
}

#[derive(Debug, Default)]
struct SceneAction {
    here: Option<String>,
    request: Option<String>,
    focus: Option<String>,
}

impl SceneAction {
    fn read(buttons: &[Value]) -> Self {
        let mut action = Self::default();
        for button in buttons {
            let Some(id) = button["id"].as_str() else {
                continue;
            };
            let label = button["label"].as_str().unwrap_or("");
            if let Some(request) = id.strip_prefix("call-request-") {
                action.request = Some(request.to_owned());
            } else if id == "request-holodeck" {
                action.request = Some("holodeck".into());
            } else if id == "request-agent" {
                action.request = Some("agent".into());
            } else if let Some(focus) = id.strip_prefix("focus-current-") {
                action.focus = Some(focus.to_owned());
            } else if id == "exit-fullscreen" {
                action.focus = None;
            } else if let Some(room) = id.strip_prefix("join-")
                && label == "Here"
            {
                action.here = Some(room.to_owned());
            }
        }
        action
    }

    fn selected(&self, leaving: bool) -> Option<String> {
        let mut parts = Vec::new();
        if let Some(here) = &self.here {
            parts.push(format!("room:{here}"));
        } else if leaving {
            parts.push("leave:1".into());
        }
        if let Some(focus) = &self.focus {
            parts.push(format!("focus:{focus}"));
        }
        (!parts.is_empty()).then(|| parts.join(";"))
    }
}

struct Canvas {
    socket: PathBuf,
    counter: AtomicU64,
}

impl Canvas {
    fn new(socket: PathBuf) -> Self {
        Self {
            socket,
            counter: AtomicU64::new(1),
        }
    }

    fn ensure_rooms_surface(&self, surface_id: &str, spec: &Path) -> Result<()> {
        for _ in 0..3 {
            let document = self.document()?;
            let mut surface = document["surfaces"]
                .as_array()
                .and_then(|surfaces| surfaces.iter().find(|surface| surface["id"] == surface_id))
                .cloned();
            if surface.is_none() {
                let spec = std::fs::canonicalize(spec)
                    .with_context(|| format!("resolve Rooms widget spec {}", spec.display()))?;
                self.request(json!({
                    "command": "open_widget",
                    "spec_path": spec,
                    "surface_id": surface_id
                }))?;
                continue;
            }
            let mut surface = surface.take().expect("surface present");
            let content = &surface["content"];
            ensure!(
                content["spec"]["kind"] == "module"
                    && content["spec"]["module"] == "spore-dev-rooms",
                "Canvas surface {surface_id} is not the Rooms module"
            );
            ensure!(
                surface["inputs"].as_array().is_some() || surface["inputs"].is_null(),
                "Canvas surface {surface_id} has malformed inputs"
            );
            if surface.get("inputs").is_none() || surface["inputs"].is_null() {
                surface["inputs"] = json!([]);
            }
            let inputs = surface["inputs"].as_array_mut().expect("inputs array");
            if !inputs.iter().any(|input| {
                input["id"] == "snapshot" && input["contract"] == "canvas.widget.snapshot.v1"
            }) {
                inputs.push(json!({
                    "id": "snapshot",
                    "contract": "canvas.widget.snapshot.v1"
                }));
            }

            let binding = json!({
                "id": "rooms-snapshot",
                "source": {
                    "owner": "soil",
                    "stream": "rooms",
                    "contract": "canvas.widget.snapshot.v1"
                },
                "target": { "surface_id": surface_id, "input_id": "snapshot" },
                "adapter": "rooms-v1",
                "delivery": { "mode": "latest" }
            });
            let existing = document["bindings"].as_array().and_then(|bindings| {
                bindings
                    .iter()
                    .find(|candidate| candidate["id"] == "rooms-snapshot")
            });
            let mut operations = Vec::new();
            if existing != Some(&binding) {
                operations.push(json!({ "op": "upsert_binding", "binding": binding }));
            }
            if document["surfaces"].as_array().and_then(|surfaces| {
                surfaces
                    .iter()
                    .find(|candidate| candidate["id"] == surface_id)
            }) != Some(&surface)
            {
                operations.push(json!({ "op": "upsert_surface", "surface": surface }));
            }
            if operations.is_empty() {
                return Ok(());
            }
            let patch = json!({
                "schema_version": 1,
                "workspace_id": document["id"],
                "base_revision": document["revision"],
                "revision": document["revision"].as_u64().unwrap_or(0) + 1,
                "operations": operations
            });
            match self.request(json!({ "command": "apply_patch", "patch": patch })) {
                Ok(_) => return Ok(()),
                Err(error) if error.to_string().contains("conflict") => continue,
                Err(error) => return Err(error),
            }
        }
        bail!("Canvas Rooms surface {surface_id} could not be bound")
    }

    fn document(&self) -> Result<Value> {
        let reply = self.request(json!({ "command": "read_document" }))?;
        reply["outcome"]["receipt"]["document"]
            .as_object()
            .map(|_| reply["outcome"]["receipt"]["document"].clone())
            .context("Canvas read_document returned no document")
    }

    fn scene_buttons(&self, surface_id: &str) -> Result<Vec<Value>> {
        let reply = self.request(json!({
            "command": "surface_scene",
            "surface_id": surface_id
        }))?;
        reply["outcome"]["receipt"]["buttons"]
            .as_array()
            .cloned()
            .context("Canvas surface_scene returned no buttons")
    }

    fn publish(&self, owner: &str, stream: &str, sequence: u64, payload: Value) -> Result<()> {
        let envelope = json!({
            "schema_version": STREAM_SCHEMA_VERSION,
            "owner": owner,
            "stream": stream,
            "contract": "canvas.widget.snapshot.v1",
            "sequence": sequence,
            "emitted_at_ms": now_ms(),
            "project_id": Value::Null,
            "payload": payload
        });
        let path = stream_socket_path()?;
        let socket = UnixDatagram::unbound()?;
        let bytes = serde_json::to_vec(&envelope)?;
        ensure!(
            bytes.len() <= 64 * 1024,
            "Canvas stream envelope exceeds 64 KiB"
        );
        socket.send_to(&bytes, &path)?;
        Ok(())
    }

    fn request(&self, command: Value) -> Result<Value> {
        let request_id = format!(
            "rooms-host-{}",
            self.counter.fetch_add(1, Ordering::Relaxed)
        );
        let request = json!({
            "schema_version": 1,
            "request_id": request_id,
            "command": command
        });
        let mut stream = UnixStream::connect(&self.socket)
            .with_context(|| format!("connect Canvas control socket {}", self.socket.display()))?;
        stream.set_read_timeout(Some(Duration::from_secs(5)))?;
        stream.write_all(serde_json::to_string(&request)?.as_bytes())?;
        stream.write_all(b"\n")?;
        let mut reply = Vec::new();
        BufReader::new(stream.take(256 * 1024)).read_until(b'\n', &mut reply)?;
        let reply: Value = serde_json::from_slice(&reply).context("parse Canvas control reply")?;
        if reply["outcome"]["status"] != "ok" {
            bail!(
                "Canvas rejected {}: {}",
                command["command"].as_str().unwrap_or("command"),
                reply["outcome"]["message"].as_str().unwrap_or("no reason")
            );
        }
        Ok(reply)
    }
}

struct CallLine {
    line: String,
}

struct CallManager {
    enabled: bool,
    bin: PathBuf,
    human: Option<Child>,
    tts: Option<Child>,
    stdin: Option<std::process::ChildStdin>,
    tts_stdin: Option<std::process::ChildStdin>,
    lines: Receiver<CallLine>,
    sender: Sender<CallLine>,
    started: Option<Instant>,
    state: String,
    error: Option<String>,
    muted: bool,
    frames: u64,
    received: u64,
    rms: f64,
}

impl CallManager {
    fn new(enabled: bool, bin: PathBuf) -> Self {
        let (sender, lines) = mpsc::channel();
        Self {
            enabled,
            bin,
            human: None,
            tts: None,
            stdin: None,
            tts_stdin: None,
            lines,
            sender,
            started: None,
            state: "idle".into(),
            error: None,
            muted: false,
            frames: 0,
            received: 0,
            rms: 0.0,
        }
    }

    fn running(&self) -> bool {
        matches!(self.state.as_str(), "starting" | "connected" | "muted")
    }

    fn start(&mut self) -> bool {
        if !self.enabled || self.running() {
            return false;
        }
        self.stop();
        match self.spawn_pair() {
            Ok(()) => {
                self.state = "starting".into();
                self.error = None;
                self.started = Some(Instant::now());
                self.muted = false;
                self.frames = 0;
                self.received = 0;
                self.rms = 0.0;
                true
            }
            Err(error) => {
                self.state = "failed".into();
                self.error = Some(format!("{error:#}"));
                true
            }
        }
    }

    fn spawn_pair(&mut self) -> Result<()> {
        let mut human = Command::new(&self.bin)
            .args([
                "human",
                "--bind",
                "127.0.0.1:47820",
                "--peer",
                "127.0.0.1:47821",
                "--seconds",
                "0",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("start {} human", self.bin.display()))?;
        let mut tts = match Command::new(&self.bin)
            .args([
                "tts",
                "--bind",
                "127.0.0.1:47821",
                "--peer",
                "127.0.0.1:47820",
                "--seconds",
                "0",
                "--text",
                "Room audio check.",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
        {
            Ok(child) => child,
            Err(error) => {
                let _ = human.kill();
                let _ = human.wait();
                return Err(error).with_context(|| format!("start {} tts", self.bin.display()));
            }
        };
        self.stdin = human.stdin.take();
        self.tts_stdin = tts.stdin.take();
        if let Some(stdout) = human.stdout.take() {
            pump(stdout, "human", self.sender.clone());
        }
        if let Some(stderr) = human.stderr.take() {
            pump(stderr, "human", self.sender.clone());
        }
        if let Some(stdout) = tts.stdout.take() {
            pump(stdout, "tts", self.sender.clone());
        }
        if let Some(stderr) = tts.stderr.take() {
            pump(stderr, "tts", self.sender.clone());
        }
        self.human = Some(human);
        self.tts = Some(tts);
        Ok(())
    }

    fn poll(&mut self) -> bool {
        let mut changed = false;
        while let Ok(line) = self.lines.try_recv() {
            let (source, body) = line
                .line
                .split_once(": ")
                .unwrap_or(("call", line.line.as_str()));
            if let Some(values) = body.strip_prefix("status ") {
                for field in values.split_whitespace() {
                    if let Some((key, value)) = field.split_once('=') {
                        match key {
                            "sent" => self.frames = value.parse().unwrap_or(self.frames),
                            "received" => self.received = value.parse().unwrap_or(self.received),
                            "rms" => self.rms = value.parse().unwrap_or(self.rms),
                            "muted" => self.muted = value == "true",
                            _ => {}
                        }
                    }
                }
                changed = true;
            } else if body.contains("error") || body.contains("Error") {
                self.error = Some(format!("{source}: {body}"));
                changed = true;
            }
        }
        if self.running() {
            let human_status = self
                .human
                .as_mut()
                .and_then(|child| child.try_wait().ok().flatten());
            let tts_status = self
                .tts
                .as_mut()
                .and_then(|child| child.try_wait().ok().flatten());
            if let Some(status) = human_status {
                self.state = "failed".into();
                self.error = Some(format!("microphone process exited with {status}"));
                self.cleanup();
                changed = true;
            } else if let Some(status) = tts_status {
                self.state = "failed".into();
                self.error = Some(format!("TTS process exited with {status}"));
                self.cleanup();
                changed = true;
            } else if self.state == "starting"
                && self
                    .started
                    .is_some_and(|started| started.elapsed() > Duration::from_millis(600))
            {
                self.state = "connected".into();
                changed = true;
            }
        }
        changed
    }

    fn set_muted(&mut self, muted: bool) -> bool {
        if !self.running() || self.muted == muted {
            return false;
        }
        let Some(stdin) = self.stdin.as_mut() else {
            eprintln!("room-host: call has no stdin to set muted={muted}");
            return false;
        };
        if let Err(error) = writeln!(stdin, "{}", if muted { "mute" } else { "unmute" }) {
            eprintln!("room-host: cannot write mute command: {error}");
            return false;
        }
        let _ = stdin.flush();
        self.muted = muted;
        self.state = if muted { "muted" } else { "connected" }.into();
        true
    }

    fn stop(&mut self) {
        for stdin in [self.stdin.as_mut(), self.tts_stdin.as_mut()]
            .into_iter()
            .flatten()
        {
            let _ = writeln!(stdin, "quit");
            let _ = stdin.flush();
        }
        for child in [self.human.as_mut(), self.tts.as_mut()]
            .into_iter()
            .flatten()
        {
            let deadline = Instant::now() + Duration::from_millis(700);
            loop {
                match child.try_wait() {
                    Ok(Some(_)) => break,
                    Ok(None) if Instant::now() < deadline => {
                        thread::sleep(Duration::from_millis(20));
                    }
                    _ => {
                        let _ = child.kill();
                        let _ = child.wait();
                        break;
                    }
                }
            }
        }
        self.human = None;
        self.tts = None;
        self.stdin = None;
        self.tts_stdin = None;
        self.started = None;
        self.state = "idle".into();
        self.error = None;
        self.muted = false;
    }

    fn cleanup(&mut self) {
        if let Some(mut child) = self.human.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        if let Some(mut child) = self.tts.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        self.stdin = None;
        self.tts_stdin = None;
    }

    fn snapshot(&self) -> Value {
        json!({
            "state": self.state,
            "route": "loopback-udp-pcm",
            "format": "48 kHz mono",
            "frame_ms": 20,
            "pid": self.human.as_ref().map(Child::id),
            "frame_count": self.frames,
            "received_samples": self.received,
            "mic_rms": self.rms,
            "muted": self.muted,
            "error": self.error
        })
    }
}

impl Drop for CallManager {
    fn drop(&mut self) {
        self.stop();
    }
}

fn pump(mut stream: impl Read + Send + 'static, source: &'static str, sender: Sender<CallLine>) {
    thread::spawn(move || {
        let mut reader = BufReader::new(&mut stream);
        let mut line = String::new();
        while reader.read_line(&mut line).unwrap_or(0) > 0 {
            let _ = sender.send(CallLine {
                line: if source.is_empty() {
                    line.trim().to_owned()
                } else {
                    format!("{source}: {}", line.trim())
                },
            });
            line.clear();
        }
    });
}

fn append_transcript(path: &Path, record: Value) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("open transcript {}", path.display()))?;
    serde_json::to_writer(&mut file, &record)?;
    file.write_all(b"\n")?;
    Ok(())
}

fn load_transcript(projection: &mut Projection, path: &Path) -> Result<()> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(error).with_context(|| format!("read transcript {}", path.display()));
        }
    };
    for (index, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let record: Value = serde_json::from_str(line)
            .with_context(|| format!("parse transcript {} line {}", path.display(), index + 1))?;
        let Some(room) = record["room"].as_str() else {
            continue;
        };
        let Some(text) = record["text"].as_str() else {
            continue;
        };
        let speaker = record["speaker_name"]
            .as_str()
            .or_else(|| record["speaker_id"].as_str())
            .unwrap_or("unknown");
        let id = record["segment_id"].as_str().unwrap_or("transcript");
        projection.add_message(room, speaker, text, false, id);
    }
    Ok(())
}

fn client_config(path: &Path) -> Result<ClientConfig> {
    let bytes = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
    let mut config: ClientConfig =
        serde_json::from_slice(&bytes).with_context(|| format!("parse {}", path.display()))?;
    config.resolve_token_file(path);
    config.schema_version = unibus::SCHEMA_VERSION_V2;
    config.read_timeout_ms = 200;
    ensure!(
        config
            .capabilities
            .iter()
            .any(|capability| capability == unibus_room::CAP_MEMBER),
        "client config must claim {}",
        unibus_room::CAP_MEMBER
    );
    Ok(config)
}

fn stream_socket_path() -> Result<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        let home = std::env::var_os("HOME").context("HOME is unavailable")?;
        Ok(PathBuf::from(home).join("Library/Application Support/Dock/canvas/stream.sock"))
    }
    #[cfg(not(target_os = "macos"))]
    {
        let runtime =
            std::env::var_os("XDG_RUNTIME_DIR").context("XDG_RUNTIME_DIR is unavailable")?;
        Ok(PathBuf::from(runtime).join("dock/canvas/stream.sock"))
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn parse_args() -> Result<Args> {
    let mut argv = std::env::args().skip(1);
    let config = argv
        .next()
        .context("usage: room-host CLIENT.json [--room ROOM] [--name NAME] [--seconds N]")?;
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
    let mut args = Args {
        config: config.into(),
        room: None,
        name: std::env::var("USER").unwrap_or_else(|_| "Rooms Host".into()),
        workspace: "FPL Rooms".into(),
        presenter: Presenter::Canvas,
        surface: "rooms".into(),
        spec: home.join(".canvas/widgets/rooms.json"),
        canvas_socket: home.join("Library/Application Support/Dock/canvas/control.sock"),
        call_bin: PathBuf::from("soil-test-call"),
        call: true,
        seconds: 0,
        verbose: false,
        agent: "jarvis".into(),
        address_text: "JARVIS, are you there?".into(),
        transcript: None,
    };
    while let Some(arg) = argv.next() {
        match arg.as_str() {
            "--room" => args.room = Some(argv.next().context("--room needs a value")?),
            "--name" => args.name = argv.next().context("--name needs a value")?,
            "--workspace" => args.workspace = argv.next().context("--workspace needs a value")?,
            "--presenter" => {
                args.presenter = match argv.next().as_deref() {
                    Some("canvas") => Presenter::Canvas,
                    Some("holodeck") => Presenter::Holodeck,
                    _ => bail!("--presenter must be canvas or holodeck"),
                };
            }
            "--surface" => args.surface = argv.next().context("--surface needs a value")?,
            "--spec" => args.spec = argv.next().context("--spec needs a value")?.into(),
            "--canvas-socket" => {
                args.canvas_socket = argv.next().context("--canvas-socket needs a value")?.into();
            }
            "--call-bin" => args.call_bin = argv.next().context("--call-bin needs a value")?.into(),
            "--no-call" => args.call = false,
            "--verbose" => args.verbose = true,
            "--agent" => args.agent = argv.next().context("--agent needs a value")?,
            "--address-text" => {
                args.address_text = argv.next().context("--address-text needs a value")?;
            }
            "--transcript" => {
                args.transcript = Some(argv.next().context("--transcript needs a path")?.into());
            }
            "--seconds" => {
                args.seconds = argv
                    .next()
                    .context("--seconds needs a value")?
                    .parse()
                    .context("--seconds must be a number")?;
            }
            other => bail!("unknown argument {other}"),
        }
    }
    Ok(args)
}
