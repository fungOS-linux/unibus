use holodeck_discovery::{PROBE, decode_advert, encode_advert};
use std::{
    net::{Ipv4Addr, SocketAddr},
    time::Duration,
};
use tokio::net::UdpSocket;
use unibus_holodeck::runtime::{Config, Export, Import, Tls, run};

fn port(ip: &str) -> u16 {
    std::net::UdpSocket::bind((ip, 0))
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

#[tokio::test]
async fn private_relay_isolates_clients_and_withdraws_dead_host() {
    let dir = tempfile::tempdir().unwrap();
    let export_cert = rcgen::generate_simple_self_signed(vec!["export.test".into()]).unwrap();
    let import_cert = rcgen::generate_simple_self_signed(vec!["import.test".into()]).unwrap();
    let ca = dir.path().join("ca.pem");
    std::fs::write(
        &ca,
        format!("{}{}", export_cert.cert.pem(), import_cert.cert.pem()),
    )
    .unwrap();
    let cert_a = dir.path().join("export.pem");
    let key_a = dir.path().join("export-key.pem");
    let cert_b = dir.path().join("import.pem");
    let key_b = dir.path().join("import-key.pem");
    std::fs::write(&cert_a, export_cert.cert.pem()).unwrap();
    std::fs::write(&key_a, export_cert.signing_key.serialize_pem()).unwrap();
    std::fs::write(&cert_b, import_cert.cert.pem()).unwrap();
    std::fs::write(&key_b, import_cert.signing_key.serialize_pem()).unwrap();
    let export_tls = Tls {
        ca: ca.clone(),
        certificate: cert_a,
        key: key_a,
        expected_peer_binding: unibus_quic::certificate_key_binding(import_cert.cert.der()),
    };
    let import_tls = Tls {
        ca,
        certificate: cert_b,
        key: key_b,
        expected_peer_binding: unibus_quic::certificate_key_binding(export_cert.cert.der()),
    };
    let echo = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let target = echo.local_addr().unwrap();
    let echo_worker = tokio::spawn(async move {
        let mut bytes = vec![0; 9000];
        loop {
            let (n, source) = echo.recv_from(&mut bytes).await.unwrap();
            echo.send_to(&bytes[..n], source).await.unwrap();
        }
    });
    let responder = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let discovery = responder.local_addr().unwrap();
    let discovery_worker = tokio::spawn(async move {
        let mut bytes = [0; 64];
        loop {
            let (_, source) = responder.recv_from(&mut bytes).await.unwrap();
            responder
                .send_to(&encode_advert(target.port()).unwrap(), source)
                .await
                .unwrap();
        }
    });
    let remote: SocketAddr = format!("127.0.0.1:{}", port("127.0.0.1")).parse().unwrap();
    let relay: SocketAddr = format!("127.0.0.1:{}", port("127.0.0.1")).parse().unwrap();
    let export = tokio::spawn(run(Config::Export {
        bind: remote,
        tls: export_tls,
        rooms: vec![Export {
            room_id: "origin/room".into(),
            label: "room".into(),
            target,
            discovery,
        }],
    }));
    let imported_discovery: SocketAddr =
        format!("127.0.0.1:{}", port("127.0.0.1")).parse().unwrap();
    let import_config = Config::Import {
        lan_address: "127.0.0.1".parse().unwrap(),
        discovery_port: imported_discovery.port(),
        allowed_clients: Vec::new(),
        allowed_subnets: vec!["127.0.0.1/32".parse().unwrap()],
        tls: import_tls,
        rooms: vec![Import {
            room_id: "origin/room".into(),
            remote,
            server_name: "export.test".into(),
            lan_bind: relay,
        }],
    };
    let import = tokio::spawn(run(import_config.clone()));
    let probe = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let mut bytes = [0; 64];
    tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            assert!(!import.is_finished(), "import runtime terminated");
            probe.send_to(&PROBE, imported_discovery).await.unwrap();
            if let Ok(Ok((n, source))) =
                tokio::time::timeout(Duration::from_millis(250), probe.recv_from(&mut bytes)).await
            {
                assert_eq!(source.ip().to_string(), "127.0.0.1");
                assert_eq!(decode_advert(&bytes[..n]), Some(relay.port()));
                break;
            }
        }
    })
    .await
    .unwrap();
    let first = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let second = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let a = vec![7; 8192];
    let b = vec![9; 1400];
    first.send_to(&a, relay).await.unwrap();
    second.send_to(&b, relay).await.unwrap();
    let mut received = vec![0; 9000];
    let (n, _) = tokio::time::timeout(Duration::from_secs(3), first.recv_from(&mut received))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(&received[..n], &a);
    let (n, _) = tokio::time::timeout(Duration::from_secs(3), second.recv_from(&mut received))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(&received[..n], &b);
    // Valid TLS trust alone must not admit a wrong pinned peer or an export
    // name that the server has not explicitly offered.
    for bad_pin in [true, false] {
        let mut rejected_config = import_config.clone();
        let rejected_discovery = port("127.0.0.1");
        if let Config::Import {
            discovery_port,
            tls,
            rooms,
            ..
        } = &mut rejected_config
        {
            *discovery_port = rejected_discovery;
            rooms[0].lan_bind.set_port(port("127.0.0.1"));
            if bad_pin {
                tls.expected_peer_binding = format!("sha256:{}", "0".repeat(64));
            } else {
                rooms[0].room_id = "origin/not-exported".into();
            }
        }
        let rejected_import = tokio::spawn(run(rejected_config));
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(!rejected_import.is_finished());
        probe
            .send_to(&PROBE, (Ipv4Addr::LOCALHOST, rejected_discovery))
            .await
            .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(400), probe.recv_from(&mut bytes))
                .await
                .is_err()
        );
        rejected_import.abort();
    }
    discovery_worker.abort();
    tokio::time::sleep(Duration::from_secs(3)).await;
    probe.send_to(&PROBE, imported_discovery).await.unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(400), probe.recv_from(&mut bytes))
            .await
            .is_err()
    );
    import.abort();
    export.abort();
    echo_worker.abort();
}
