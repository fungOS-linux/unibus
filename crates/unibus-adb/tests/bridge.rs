//! Real Unibus hub + both bridge processes + a fake local USB/ADB backend.
use std::{
    io::{BufRead, BufReader, Read, Write},
    net::{Shutdown, TcpListener, TcpStream},
    process::{Child, Command, Stdio},
    sync::Arc,
    thread,
    time::{Duration, Instant},
};
use unibus_router::{Hub, config::HubConfig};

struct Processes(Vec<Child>);
impl Drop for Processes {
    fn drop(&mut self) {
        for child in &mut self.0 {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}
fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

#[test]
fn binary_stream_survives_chunking_and_half_close_through_real_bus() {
    let temp = tempfile::tempdir().unwrap();
    let token_path = temp.path().join("token");
    std::fs::write(&token_path, "0123456789abcdef0123456789abcdef").unwrap();
    let hub_port = free_port();
    let local_port = free_port();
    let backend = TcpListener::bind("127.0.0.1:0").unwrap();
    let backend_addr = backend.local_addr().unwrap();
    let hub_config: HubConfig = serde_json::from_value(serde_json::json!({
        "installation_id":"test", "listen":format!("127.0.0.1:{hub_port}"),
        "routes":[{"kind":"adb.socket.v1","target":{"mode":"producer"}}],
        "nodes":(["studio-adb","neo-adb","rogue-adb"].map(|id| serde_json::json!({
            "producer_id":id,"token_file":token_path,"zones":["default"],"capabilities":[],
            "publishes":["adb.socket.v1"],"publish_producers":["studio-adb","neo-adb"]
        })))
    }))
    .unwrap();
    let hub = Arc::new(Hub::new(hub_config).unwrap());
    thread::spawn(move || hub.serve().unwrap());
    let deadline = Instant::now() + Duration::from_secs(5);
    while TcpStream::connect(("127.0.0.1", hub_port)).is_err() {
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(10));
    }
    let bus = |id: &str| {
        serde_json::json!({"installation_id":"test","producer_id":id,
        "hub_endpoint":format!("127.0.0.1:{hub_port}"),"token_file":token_path,
        "zones":["default"],"capabilities":[],"schema_version":2})
    };
    let portable_bus = |id: &str| {
        let mut config = bus(id);
        config["token_file"] = serde_json::json!("token");
        config
    };
    let server_path = temp.path().join("server.json");
    let client_path = temp.path().join("client.json");
    std::fs::write(
        &server_path,
        serde_json::to_vec(&serde_json::json!({"bus":portable_bus("studio-adb"),
        "mode":{"mode":"serve","adb_endpoint":backend_addr,"allowed_clients":["neo-adb"],"publish_inventory":false}}))
        .unwrap(),
    )
    .unwrap();
    std::fs::write(&client_path, serde_json::to_vec(&serde_json::json!({"bus":portable_bus("neo-adb"),
        "mode":{"mode":"attach","listen":format!("127.0.0.1:{local_port}"),"destination":"studio-adb"}})).unwrap()).unwrap();
    let spawn = |path| {
        let mut child = Command::new(env!("CARGO_BIN_EXE_unibus-adb"))
            .arg(path)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let stderr = child.stderr.take().unwrap();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(stderr).lines() {
                let Ok(line) = line else { break };
                if line.starts_with("ADB bridge registered: ") {
                    let _ = ready_tx.send(());
                }
                eprintln!("{line}");
            }
        });
        if ready_rx.recv_timeout(Duration::from_secs(10)).is_err() {
            let _ = child.kill();
            let _ = child.wait();
            panic!("ADB child failed to register within deadline");
        }
        child
    };
    let mut processes = Processes(vec![spawn(&server_path)]);
    // The host is registered, not merely spawned or listening on its backend.
    let rogue =
        unibus::client::Client::connect(serde_json::from_value(bus("rogue-adb")).unwrap()).unwrap();
    let open = rogue.envelope(Some("studio-adb"), unibus::Message::new("adb.socket.v1",
        serde_json::json!({"session":"unauthorized-session","operation":"open","sequence":0}))).unwrap();
    rogue.send(&open, &[]).unwrap();
    thread::sleep(Duration::from_millis(100));
    backend.set_nonblocking(true).unwrap();
    assert!(
        matches!(backend.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock)
    );
    backend.set_nonblocking(false).unwrap();
    processes.0.push(spawn(&client_path));
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut local = loop {
        match TcpStream::connect(("127.0.0.1", local_port)) {
            Ok(socket) => break socket,
            Err(_) => {
                assert!(Instant::now() < deadline);
                thread::sleep(Duration::from_millis(10));
            }
        }
    };
    local
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let input: Vec<u8> = (0..4 * 1024 * 1024).map(|i| (i % 251) as u8).collect();
    let expected = input.clone();
    let echo = thread::spawn(move || {
        let (mut socket, _) = backend.accept().unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let mut received = Vec::new();
        socket.read_to_end(&mut received).unwrap();
        assert_eq!(received, expected);
        socket.write_all(&received).unwrap();
        socket.shutdown(Shutdown::Write).unwrap();
    });
    local.write_all(&input).unwrap();
    local.shutdown(Shutdown::Write).unwrap();
    let mut output = Vec::new();
    local.read_to_end(&mut output).unwrap();
    assert_eq!(output, input);
    echo.join().unwrap();
}
