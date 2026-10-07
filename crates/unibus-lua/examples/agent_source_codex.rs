//! Read-only owner protocol plumbing. Lua owns all state mapping and presentation.
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    os::unix::net::UnixStream,
    time::{Duration, Instant},
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args()
        .nth(1)
        .ok_or("expected private app-server socket path")?;
    let details = match std::env::args().nth(2).as_deref() {
        None => false,
        Some("--display-details") => true,
        _ => return Err("expected optional --display-details".into()),
    };
    if std::env::args().nth(3).is_some() {
        return Err("unexpected source argument".into());
    }
    println!("{}", observe(std::path::Path::new(&path), details)?);
    Ok(())
}

fn display_label(value: &str) -> String {
    let mut output = String::new();
    for c in value.chars().filter(|c| !c.is_control()) {
        if output.len() + c.len_utf8() > 160 {
            break;
        }
        output.push(c);
    }
    output
}

fn observe(path: &std::path::Path, details: bool) -> Result<Value, Box<dyn std::error::Error>> {
    let stream = UnixStream::connect(path)?;
    stream.set_read_timeout(Some(Duration::from_secs(3)))?;
    stream.set_write_timeout(Some(Duration::from_secs(3)))?;
    let mut config = tungstenite::protocol::WebSocketConfig::default();
    config.max_message_size = Some(4 * 1024 * 1024);
    config.max_frame_size = Some(4 * 1024 * 1024);
    let (mut ws, _) =
        tungstenite::client::client_with_config("ws://localhost/", stream, Some(config))?;
    let deadline = Instant::now() + Duration::from_secs(15);
    let rpc = |ws: &mut tungstenite::WebSocket<UnixStream>,
               id: u64,
               method: &str,
               params: Value|
     -> Result<Value, Box<dyn std::error::Error>> {
        ws.send(tungstenite::Message::Text(
            json!({"id":id,"method":method,"params":params})
                .to_string()
                .into(),
        ))?;
        for _ in 0..128 {
            if Instant::now() >= deadline {
                return Err("owner observation timed out".into());
            }
            if let tungstenite::Message::Text(text) = ws.read()? {
                let value: Value = serde_json::from_str(&text)?;
                if value["id"] == id {
                    if value.get("error").is_some() {
                        return Err("owner rejected read-only request".into());
                    }
                    return value
                        .get("result")
                        .cloned()
                        .ok_or("missing owner result".into());
                }
            }
        }
        Err("owner reply budget exceeded".into())
    };
    rpc(
        &mut ws,
        1,
        "initialize",
        json!({"clientInfo":{"name":"unibus-agent-source","version":"0.1.0"}}),
    )?;
    ws.send(tungstenite::Message::Text(
        json!({"method":"initialized","params":{}})
            .to_string()
            .into(),
    ))?;
    let loaded = rpc(&mut ws, 2, "thread/loaded/list", json!({}))?;
    let ids = loaded["data"]
        .as_array()
        .ok_or("missing loaded session IDs")?;
    if ids.len() > 256 {
        return Err("loaded roster exceeds budget".into());
    }
    let mut sessions = Vec::new();
    let mut seen = BTreeSet::new();
    for id in ids {
        let id = id.as_str().ok_or("invalid session identity")?;
        if !seen.insert(id) {
            continue;
        }
        let result = rpc(
            &mut ws,
            3 + sessions.len() as u64,
            "thread/read",
            json!({"threadId":id,"includeTurns":false}),
        )?;
        // Discard metadata and conversation content immediately.
        let mut item = json!({"id":id,"status":result["thread"]["status"]});
        if details {
            let thread = &result["thread"];
            // Opt-in named title only: never preview/turns/prompts or full paths.
            if let Some(name) = thread["name"].as_str() {
                item["display_task"] = json!(display_label(name));
            }
            if let Some(name) = thread["cwd"]
                .as_str()
                .and_then(|cwd| std::path::Path::new(cwd).file_name())
                .and_then(|name| name.to_str())
            {
                item["display_project"] = json!(display_label(name));
            }
        }
        sessions.push(item);
    }
    Ok(json!({"provider":"codex","mode":"snapshot","sessions":sessions}))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn display_labels_are_utf8_bounded_and_have_no_controls() {
        let value = display_label(&format!("\n{}\t", "界".repeat(100)));
        assert!(value.len() <= 160 && !value.contains('\n') && !value.contains('\t'));
        assert_eq!(value.chars().count(), 53);
    }
    #[test]
    fn unix_websocket_reads_status_without_forwarding_conversations() {
        for details in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("owner.sock");
            let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
            let server = std::thread::spawn(move || {
                let (stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                let mut ws = tungstenite::accept(stream).unwrap();
                for _ in 0..4 {
                    let text = ws.read().unwrap().into_text().unwrap();
                    let request: Value = serde_json::from_str(&text).unwrap();
                    let result = match request["method"].as_str().unwrap() {
                        "initialize" => json!({}),
                        "initialized" => continue,
                        "thread/loaded/list" => json!({"data":["session-a","session-a"]}),
                        "thread/read" => {
                            assert_eq!(request["params"]["includeTurns"], false);
                            json!({"thread":{"name":"TV attention", "cwd":"/private/Canvas", "preview":"private preview", "status":{"type":"idle"},"turns":[{"text":"private conversation"}]}})
                        }
                        _ => panic!("unexpected owner mutation"),
                    };
                    ws.send(tungstenite::Message::Text(
                        json!({"id":request["id"],"result":result})
                            .to_string()
                            .into(),
                    ))
                    .unwrap();
                }
            });
            let output = observe(&path, details).unwrap();
            server.join().unwrap();
            assert_eq!(output["sessions"].as_array().unwrap().len(), 1);
            assert_eq!(output["sessions"][0]["status"]["type"], "idle");
            assert!(!output.to_string().contains("private conversation"));
            assert!(!output.to_string().contains("private preview"));
            assert!(!output.to_string().contains("/private"));
            if details {
                assert_eq!(output["sessions"][0]["display_project"], "Canvas");
                assert_eq!(output["sessions"][0]["display_task"], "TV attention");
            } else {
                assert!(output["sessions"][0].get("display_task").is_none());
            }
        }
    }
}
