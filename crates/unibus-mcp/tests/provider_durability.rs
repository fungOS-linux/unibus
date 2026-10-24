//! Hermetic regression test for provider durability.
//!
//! One unreachable downstream MCP server must not stop the publisher from
//! advertising every healthy provider. The test runs the real `unibus-mcp
//! publish` binary against a fake hub and fake loopback MCP servers, so it can
//! also be pointed at an older binary with `UNIBUS_MCP_TEST_BIN`.

use std::collections::BTreeSet;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use unibus::{Envelope, Message, kinds};

struct FakeDownstream {
    endpoint: String,
}

impl FakeDownstream {
    fn start(tool: &str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind fake downstream");
        let endpoint = format!("http://{}/mcp", listener.local_addr().unwrap());
        let tool = tool.to_owned();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { break };
                let tool = tool.clone();
                std::thread::spawn(move || {
                    let _ = serve_mcp(stream, &tool);
                });
            }
        });
        Self { endpoint }
    }
}

fn serve_mcp(mut stream: TcpStream, tool: &str) -> std::io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    loop {
        let mut request_line = String::new();
        if reader.read_line(&mut request_line)? == 0 {
            return Ok(());
        }
        if request_line.trim().is_empty() {
            continue;
        }
        let mut length = 0usize;
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line)? == 0 {
                return Ok(());
            }
            if line == "\r\n" || line == "\n" {
                break;
            }
            if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                length = value.trim().parse().unwrap_or(0);
            }
        }
        let mut body = vec![0; length];
        reader.read_exact(&mut body)?;
        let request: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
        let id = request.get("id").cloned().unwrap_or(Value::Null);
        if id.is_null() {
            stream.write_all(b"HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\n\r\n")?;
            stream.flush()?;
            continue;
        }
        let method = request
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let result = match method {
            "initialize" => json!({
                "protocolVersion":"2025-06-18",
                "capabilities":{"tools":{}},
                "serverInfo":{"name":"fake-downstream","version":"1"}
            }),
            "tools/list" => json!({
                "tools":[{
                    "name":tool,
                    "description":"fake read-only tool",
                    "inputSchema":{"type":"object"},
                    "annotations":{"readOnlyHint":true}
                }]
            }),
            _ => json!({}),
        };
        let response = json!({"jsonrpc":"2.0","id":id,"result":result}).to_string();
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            response.len(),
            response
        )?;
        stream.flush()?;
    }
}

fn refused_endpoint() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("reserve refused port");
    let address = listener.local_addr().unwrap();
    drop(listener);
    format!("http://{address}/mcp")
}

fn start_fake_hub(advertised: Arc<Mutex<BTreeSet<String>>>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind fake hub");
    let endpoint = listener.local_addr().unwrap().to_string();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            let advertised = advertised.clone();
            std::thread::spawn(move || {
                let Ok((register, _)) = unibus::framing::read_frame(&mut stream) else {
                    return;
                };
                let reply = Envelope::new(
                    "test-home",
                    "test-hub",
                    None,
                    "",
                    Message::new(
                        kinds::V1_REPLY,
                        json!({"success":true,"request_id":register.request_id}),
                    ),
                );
                if unibus::framing::write_frame(&mut stream, &reply, &[]).is_err() {
                    return;
                }
                while let Ok((envelope, _)) = unibus::framing::read_frame(&mut stream) {
                    if envelope.message.kind != unibus_mcp::PROVIDER_KIND {
                        continue;
                    }
                    if let Some(provider_id) = envelope
                        .message
                        .body
                        .get("provider_id")
                        .and_then(Value::as_str)
                    {
                        advertised.lock().unwrap().insert(provider_id.to_owned());
                    }
                }
            });
        }
    });
    endpoint
}

fn binary() -> PathBuf {
    std::env::var_os("UNIBUS_MCP_TEST_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_unibus-mcp")))
}

fn wait_for_catalog(
    child: &mut Child,
    advertised: &Arc<Mutex<BTreeSet<String>>>,
    expected: &[&str],
    stderr_path: &Path,
) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if let Some(status) = child.try_wait().expect("poll publisher") {
            panic!(
                "publisher exited with {status} after advertising {:?}; stderr:\n{}",
                advertised.lock().unwrap(),
                fs::read_to_string(stderr_path).unwrap_or_default()
            );
        }
        let have = advertised.lock().unwrap().clone();
        if expected.iter().all(|id| have.contains(*id)) {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!(
        "publisher did not advertise {:?} before the deadline; saw {:?}; stderr:\n{}",
        expected,
        advertised.lock().unwrap(),
        fs::read_to_string(stderr_path).unwrap_or_default()
    );
}

#[test]
fn a_dead_downstream_does_not_kill_the_publisher() {
    let healthy_a = FakeDownstream::start("ping_a");
    let healthy_b = FakeDownstream::start("ping_b");
    let dead_endpoint = refused_endpoint();
    let advertised = Arc::new(Mutex::new(BTreeSet::new()));
    let hub_endpoint = start_fake_hub(advertised.clone());

    let root = std::env::temp_dir().join(format!(
        "unibus-provider-durability-{}-{}",
        std::process::id(),
        unibus::envelope::now_ms()
    ));
    fs::create_dir_all(&root).unwrap();
    let token_path = root.join("token");
    fs::write(&token_path, "test-token\n").unwrap();
    let config_path = root.join("mcp.json");
    let stderr_path = root.join("publisher.err");
    let config = json!({
        "unibus": {
            "installation_id": "test-home",
            "producer_id": "test-provider",
            "hub_endpoint": hub_endpoint,
            "token_file": token_path,
            "zones": ["default"],
            "capabilities": ["mcp.provider"],
            "schema_version": 2,
            "read_timeout_ms": 200
        },
        "lease_ms": 5000,
        "providers": [
            {
                "provider_id": "healthy-a",
                "node_id": "test-node",
                "producer_id": "test-provider",
                "endpoint": healthy_a.endpoint,
                "mutation_overrides": {}
            },
            {
                "provider_id": "dead-provider",
                "node_id": "test-node",
                "producer_id": "test-provider",
                "endpoint": dead_endpoint,
                "mutation_overrides": {}
            },
            {
                "provider_id": "healthy-b",
                "node_id": "test-node",
                "producer_id": "test-provider",
                "endpoint": healthy_b.endpoint,
                "mutation_overrides": {}
            }
        ]
    });
    fs::write(&config_path, serde_json::to_vec_pretty(&config).unwrap()).unwrap();
    let stderr = fs::File::create(&stderr_path).unwrap();

    let mut child = Command::new(binary())
        .arg("publish")
        .arg(&config_path)
        .stderr(Stdio::from(stderr))
        .spawn()
        .expect("spawn publisher");

    wait_for_catalog(
        &mut child,
        &advertised,
        &["healthy-a", "healthy-b"],
        &stderr_path,
    );

    let ids = advertised.lock().unwrap().clone();
    assert!(
        child.try_wait().unwrap().is_none(),
        "publisher exited: {ids:?}"
    );
    assert!(
        !ids.contains("dead-provider"),
        "dead provider must not be advertised: {ids:?}"
    );
    let log = fs::read_to_string(&stderr_path).unwrap_or_default();
    assert!(
        log.contains("dead-provider") && log.contains(&dead_endpoint),
        "warning must name the skipped provider and URL; log:\n{log}"
    );
    assert!(
        log.contains("retry in"),
        "warning must say the dead provider will be retried; log:\n{log}"
    );

    let _ = child.kill();
    let _ = child.wait();
    let _ = fs::remove_dir_all(&root);
}
