#![cfg(feature = "viewer")]
use coherence_widget_contract::{MESSAGE_KIND, Snapshot};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use unibus::{
    Message,
    client::{Client, ClientConfig},
    envelope::now_ms,
};
use unibus_router::{Hub, HubConfig, viewer::widgets::Definition};

#[test]
fn authenticated_bus_publication_reaches_only_the_authorized_projection() {
    let directory = tempfile::tempdir().unwrap();
    let token = directory.path().join("source.token");
    unibus_router::auth::issue_token(&token, &"a".repeat(64)).unwrap();
    let config: HubConfig = serde_json::from_value(serde_json::json!({
        "installation_id":"test", "listen":"127.0.0.1:0",
        "routes":[{"kind":MESSAGE_KIND,"target":{"mode":"capability","name":"widget.reader"}}],
        "nodes":[
            {"producer_id":"source","token_file":token,"zones":["lab"],"capabilities":["widget.source"],"publishes":[MESSAGE_KIND],"publish_zones":["lab"]},
            {"producer_id":"reader","token_file":token,"zones":["lab"],"capabilities":["unibus.viewer","umie.telemetry","widget.reader"]},
            {"producer_id":"other","token_file":token,"zones":["home"],"capabilities":["unibus.viewer","umie.telemetry"]}
        ]
    })).unwrap();
    let hub = Arc::new(Hub::new(config).unwrap());
    let definition = Definition {
        id: "umie".into(),
        title: "UMIE".into(),
        producer_id: "source".into(),
        zone: "lab".into(),
        capability: "umie.telemetry".into(),
    };
    hub.configure_widgets(&[definition.clone()]).unwrap();
    let endpoint = hub.start().unwrap();
    let config: ClientConfig = serde_json::from_value(serde_json::json!({
        "installation_id":"test","producer_id":"source","hub_endpoint":endpoint.to_string(),"token_file":token,"zones":["lab"],"capabilities":["widget.source"],"schema_version":2
    })).unwrap();
    let client = Client::connect(config).unwrap();
    let reader_config: ClientConfig = serde_json::from_value(serde_json::json!({
        "installation_id":"test","producer_id":"reader","hub_endpoint":endpoint.to_string(),"token_file":token,"zones":["lab"],"capabilities":["widget.reader"],"schema_version":2,"read_timeout_ms":20
    })).unwrap();
    let reader = Client::connect(reader_config).unwrap();
    let now = now_ms();
    let snapshot = Snapshot {
        schema_version: 1,
        widget_id: "umie".into(),
        observed_at_unix_ms: now,
        expires_at_unix_ms: now + 30000,
        metrics: vec![],
    };
    let envelope = client
        .envelope(
            Some("lab"),
            Message::new(MESSAGE_KIND, serde_json::to_value(snapshot).unwrap()),
        )
        .unwrap();
    client.send(&envelope, &[]).unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if let Some(snapshot) = hub.viewer_widget("reader", &definition).unwrap() {
            assert_eq!(snapshot.observed_at_unix_ms, now);
            break;
        }
        assert!(
            Instant::now() < deadline,
            "bus publication never reached retention"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(hub.viewer_widget("other", &definition).is_err());
    assert!(hub.viewer_widget("unknown", &definition).is_err());
    // The retention plugin must not steal normal fan-out, even for widgets
    // this daemon does not present.
    let mut unknown = envelope.clone();
    unknown
        .message
        .body
        .insert("widget_id".into(), serde_json::json!("unconfigured"));
    client.send(&unknown, &[]).unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut delivered = Vec::new();
    while delivered.len() < 2 {
        if let Some((packet, _)) = reader.try_read().unwrap() {
            if packet.message.kind == MESSAGE_KIND {
                delivered.push(
                    packet.message.body["widget_id"]
                        .as_str()
                        .unwrap()
                        .to_owned(),
                );
            }
        }
        assert!(
            Instant::now() < deadline,
            "widget retention stole normal relay delivery"
        );
    }
    assert_eq!(delivered, ["umie", "unconfigured"]);
    hub.configure_widgets(&[]).unwrap();
    assert!(hub.viewer_widget("reader", &definition).unwrap().is_none());
}
