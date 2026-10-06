//! Actual adapter and downloader binaries through an authenticated router.
use serde_json::json;
use std::{
    io::{Read, Write},
    process::{Child, Command},
    sync::Arc,
    time::Duration,
};
use unibus_ipfs::{CONTENT, ERROR, FETCH};

struct Process(Child);
impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn cid_download_crosses_real_bus_without_json_blob_encoding() {
    let dir = tempfile::tempdir().unwrap();
    for name in ["adapter", "reader"] {
        unibus_router::auth::issue_token(
            &dir.path().join(format!("{name}.token")),
            &unibus_router::auth::generate_token().unwrap(),
        )
        .unwrap();
    }
    let routes =
        [FETCH, CONTENT, ERROR].map(|kind| json!({"kind":kind,"target":{"mode":"producer"}}));
    let config = serde_json::from_value(json!({"installation_id":"test", "listen":"127.0.0.1:0",
        "routes": routes,
        "nodes":[{"producer_id":"adapter","token_file":dir.path().join("adapter.token"),"zones":["default"],"capabilities":[],"publishes":[CONTENT,ERROR],"publish_producers":["reader"]},
        {"producer_id":"reader","token_file":dir.path().join("reader.token"),"zones":["default"],"capabilities":[],"publishes":[FETCH],"publish_producers":["adapter"]}]})).unwrap();
    let hub = Arc::new(unibus_router::Hub::new(config).unwrap());
    let address = hub.start().unwrap();
    let client = |name: &str| json!({"installation_id":"test","producer_id":name,"hub_endpoint":address.to_string(),"token_file":dir.path().join(format!("{name}.token")),"zones":["default"],"capabilities":[],"schema_version":2,"read_timeout_ms":100});
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    listener.set_nonblocking(true).unwrap();
    let server = std::thread::spawn(move || {
        for body in [br#"{"Size":3,"Type":"file"}"#.as_slice(), b"pdf"] {
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            let mut stream = loop {
                if let Ok((stream, _)) = listener.accept() {
                    break stream;
                }
                assert!(std::time::Instant::now() < deadline, "missing Kubo request");
                std::thread::sleep(Duration::from_millis(5));
            };
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let mut header = Vec::new();
            while !header.ends_with(b"\r\n\r\n") {
                let mut byte = [0];
                stream.read_exact(&mut byte).unwrap();
                header.push(byte[0]);
                assert!(header.len() < 8192);
            }
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .unwrap();
            stream.write_all(body).unwrap();
        }
    });
    let adapter_config = dir.path().join("adapter.json");
    std::fs::write(
        &adapter_config,
        serde_json::to_vec(
            &json!({"client":client("adapter"),"kubo_rpc":endpoint,"allowed_producers":["reader"]}),
        )
        .unwrap(),
    )
    .unwrap();
    let mut adapter = Process(
        Command::new(env!("CARGO_BIN_EXE_unibus-ipfs"))
            .arg("serve")
            .arg(adapter_config)
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap(),
    );
    // The ready line is emitted only after authenticated registration completes.
    let mut readiness = String::new();
    std::io::BufRead::read_line(
        &mut std::io::BufReader::new(adapter.0.stderr.take().unwrap()),
        &mut readiness,
    )
    .unwrap();
    assert_eq!(readiness.trim(), "unibus-ipfs ready");
    let reader_config = dir.path().join("reader.json");
    std::fs::write(
        &reader_config,
        serde_json::to_vec(&client("reader")).unwrap(),
    )
    .unwrap();
    let destination = dir.path().join("document.pdf");
    let result = Command::new(env!("CARGO_BIN_EXE_unibus-ipfs"))
        .arg("download")
        .arg(reader_config)
        .arg("adapter")
        .arg("QmYwAPJzv5CZsnAzt8auVZRnGi2C5pnPZGoMHfGMhL6VWJ")
        .arg(&destination)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(std::fs::read(destination).unwrap(), b"pdf");
    server.join().unwrap();
}
