//! End-to-end hub tests over loopback TCP: registration, grants, routing,
//! acks, screen anchors, and the plugin seam.

use std::net::TcpStream;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use serde_json::json;
use unibus::envelope::{Envelope, Message};
use unibus::framing::{read_frame, write_frame};
use unibus::kinds::{self, Control};
use unibus_router::{
    Hub, HubConfig,
    plugin::{Envelope as PluginEnvelope, HubHandle, HubPlugin},
};

const INSTALLATION: &str = "home";
const TOKEN: &str = "0123456789abcdef0123456789abcdef";

struct TestHub {
    addr: std::net::SocketAddr,
    _dir: tempfile::TempDir,
}

fn hub_config(dir: &std::path::Path, extra_routes: &str) -> HubConfig {
    std::fs::write(dir.join("token"), TOKEN).unwrap();
    let json = format!(
        r#"{{
        "installation_id": "{INSTALLATION}",
        "listen": "127.0.0.1:0",
        "routes": [
            {extra_routes}
            {{"kind": "canvas_command", "target": {{"mode": "capability", "name": "display"}}}},
            {{"kind": "audio_playback_chunk", "target": {{"mode": "capability", "name": "audio_output"}}}}
        ],
        "nodes": [
            {{"producer_id": "brain", "token_file": "token", "zones": ["default", "living-room"], "capabilities": ["display", "audio_output", "presence_capture", "canvas.command"]}},
            {{"producer_id": "tv", "token_file": "token", "zones": ["living-room"], "capabilities": ["display"], "screen_anchors": ["screen-tv"]}},
            {{"producer_id": "studio", "token_file": "token", "zones": ["default"], "capabilities": ["display"], "screen_anchors": ["screen-left"]}},
            {{"producer_id": "speaker", "token_file": "token", "zones": ["living-room"], "capabilities": ["audio_output"]}}
        ]
    }}"#
    );
    std::fs::write(dir.join("hub.json"), json).unwrap();
    HubConfig::read(&dir.join("hub.json")).unwrap()
}

fn start(extra_routes: &str) -> TestHub {
    TestHub::with(extra_routes, vec![])
}

impl TestHub {
    fn with(extra_routes: &str, plugins: Vec<Arc<dyn HubPlugin>>) -> Self {
        let _ = tracing_subscriber::fmt()
            .with_env_filter("debug")
            .with_test_writer()
            .try_init();
        let dir = tempfile::tempdir().unwrap();
        let config = hub_config(dir.path(), extra_routes);
        let mut hub = Hub::new(config).unwrap();
        for plugin in plugins {
            hub = hub.with_plugin(plugin);
        }
        let hub = Arc::new(hub);
        let addr = hub.start().unwrap();
        Self { addr, _dir: dir }
    }

    fn node(&self, producer_id: &str, capabilities: &[&str], anchors: &[&str]) -> Node {
        self.node_with_token(producer_id, capabilities, anchors, TOKEN)
    }

    fn node_at_schema(&self, schema: u32, producer_id: &str, capabilities: &[&str]) -> Node {
        let stream = TcpStream::connect(self.addr).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut node = Node {
            stream,
            producer_id: producer_id.to_string(),
        };
        let mut register = Envelope::new(
            INSTALLATION,
            producer_id,
            None,
            TOKEN,
            Message::new(
                kinds::V1_REGISTER,
                json!({
                    "capabilities": capabilities,
                    "zones": zones_for(producer_id),
                    "screen_anchors": [],
                }),
            ),
        );
        register.schema_version = schema;
        write_frame(&mut node.stream, &register, &[]).unwrap();
        let (reply, _) = read_frame(&mut node.stream).unwrap();
        assert!(reply.message.body["success"].as_bool() == Some(true));
        node
    }

    fn node_with_token(
        &self,
        producer_id: &str,
        capabilities: &[&str],
        anchors: &[&str],
        token: &str,
    ) -> Node {
        let stream = TcpStream::connect(self.addr).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut node = Node {
            stream,
            producer_id: producer_id.to_string(),
        };
        let register = Envelope::new(
            INSTALLATION,
            producer_id,
            None,
            token,
            Message::new(
                kinds::V1_REGISTER,
                json!({
                    "capabilities": capabilities,
                    "zones": zones_for(producer_id),
                    "screen_anchors": anchors,
                }),
            ),
        );
        write_frame(&mut node.stream, &register, &[]).unwrap();
        let (reply, _) = read_frame(&mut node.stream).unwrap();
        assert!(
            reply.message.body["success"].as_bool() == Some(true),
            "register rejected for {producer_id}: {:?}",
            reply.message.body
        );
        node
    }
}

fn zones_for(producer_id: &str) -> Vec<&'static str> {
    match producer_id {
        "brain" => vec!["default", "living-room"],
        "tv" | "speaker" => vec!["living-room"],
        _ => vec!["default"],
    }
}

struct Node {
    stream: TcpStream,
    producer_id: String,
}

impl Node {
    fn send(&mut self, message: Message, destination: Option<&str>) -> Result<()> {
        let envelope = Envelope::new(INSTALLATION, &self.producer_id, destination, TOKEN, message);
        write_frame(&mut self.stream, &envelope, &[])?;
        Ok(())
    }

    fn send_for_screen(&mut self, anchor: &str, message: Message) -> Result<()> {
        let mut envelope =
            Envelope::for_screen(INSTALLATION, &self.producer_id, TOKEN, anchor, message);
        envelope.ttl_ms = Some(30_000);
        write_frame(&mut self.stream, &envelope, &[])?;
        Ok(())
    }

    fn recv(&mut self) -> (Envelope, Vec<u8>) {
        read_frame(&mut self.stream).unwrap()
    }

    fn recv_reply(&mut self) -> Envelope {
        self.recv_until("reply")
    }

    fn recv_until(&mut self, want: &str) -> Envelope {
        for _ in 0..16 {
            let (envelope, _) = self.recv();
            if envelope.message.kind == want {
                return envelope;
            }
        }
        panic!("never saw kind {want}");
    }

    fn heartbeat(&mut self) {
        let envelope = Envelope::new(
            INSTALLATION,
            &self.producer_id,
            None,
            TOKEN,
            Message::new(kinds::V1_HEARTBEAT, json!(null)),
        );
        write_frame(&mut self.stream, &envelope, &[]).unwrap();
    }
}

#[test]
fn canvas_command_reaches_display_in_destination_zone() {
    let hub = start("");
    let mut brain = hub.node("brain", &["display", "canvas.command"], &[]);
    let mut tv = hub.node("tv", &["display"], &["screen-tv"]);
    let mut studio = hub.node("studio", &["display"], &["screen-left"]);

    brain
        .send(
            Message::new("canvas_command", json!({"command": {"op": "ping"}})),
            Some("living-room"),
        )
        .unwrap();

    let received = tv.recv_until("canvas_command");
    assert_eq!(
        received.message.body["command"]["op"].as_str(),
        Some("ping")
    );
    // zone-scoped: the studio display must not receive it
    studio
        .stream
        .set_read_timeout(Some(Duration::from_millis(200)))
        .unwrap();
    assert!(read_frame(&mut studio.stream).is_err());
}

#[test]
fn sender_is_never_echoed_and_stream_gets_no_ack() {
    let hub = start("");
    let mut brain = hub.node("brain", &["display", "audio_output"], &[]);
    brain
        .send(Message::new("canvas_command", json!({"command": 1})), None)
        .unwrap();
    // brain is itself a display in `default`; it must only get the ack —
    // and the ack is a loud failure because no other display is connected.
    let ack = brain.recv_reply();
    assert_eq!(kinds::classify(&ack.message.kind), Control::Reply);
    assert_eq!(ack.message.body["success"], json!(false));
    assert_eq!(ack.message.body["message"], json!("no eligible nodes"));
    brain
        .stream
        .set_read_timeout(Some(Duration::from_millis(200)))
        .unwrap();
    assert!(read_frame(&mut brain.stream).is_err());

    // stream-class messages are fire-and-forget: no ack
    let pcm = vec![9u8; 64];
    let envelope = Envelope::new(
        INSTALLATION,
        "brain",
        Some("living-room"),
        TOKEN,
        Message::new(
            "audio_playback_chunk",
            json!({"format": "s16_le", "sample_rate_hz": 48000, "channels": 1, "timestamp_ms": 1, "payload_bytes": pcm.len()}),
        ),
    );
    let mut speaker = hub.node("speaker", &["audio_output"], &[]);
    let mut envelope = envelope;
    envelope.payload_bytes = pcm.len();
    write_frame(&mut brain.stream, &envelope, &pcm).unwrap();
    let (received, payload) = speaker.recv();
    assert_eq!(received.message.kind, "audio_playback_chunk");
    assert_eq!(payload, pcm);
}

#[test]
fn target_screen_routes_to_exactly_one_anchor() {
    let hub = start("");
    let mut brain = hub.node("brain", &["canvas.command"], &[]);
    let mut studio = hub.node("studio", &["display"], &["screen-left"]);
    let _tv = hub.node("tv", &["display"], &["screen-tv"]);

    brain
        .send_for_screen(
            "screen-left",
            Message::new("canvas_command", json!({"command": {"op": "focus"}})),
        )
        .unwrap();
    let got = studio.recv_until("canvas_command");
    assert_eq!(got.target_screen.as_deref(), Some("screen-left"));
    // ack back to the brain (liveness announcements may arrive first)
    let ack = brain.recv_reply();
    assert_eq!(kinds::classify(&ack.message.kind), Control::Reply);
    assert_eq!(ack.message.body["success"], json!(true));
}

#[test]
fn unroutable_command_fails_loudly() {
    let hub = start("");
    let mut brain = hub.node("brain", &["canvas.command"], &[]);
    brain
        .send(Message::new("totally_unknown_kind", json!({"x": 1})), None)
        .unwrap();
    let (ack, _) = brain.recv();
    assert_eq!(ack.message.body["success"], json!(false));
    assert!(
        ack.message.body["message"]
            .as_str()
            .unwrap()
            .contains("no route")
    );
}

#[test]
fn ungranted_capability_claim_is_rejected() {
    let hub = start("");
    let stream = TcpStream::connect(hub.addr).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut node = Node {
        stream,
        producer_id: "tv".into(),
    };
    let register = Envelope::new(
        INSTALLATION,
        "tv",
        None,
        TOKEN,
        Message::new(
            kinds::V1_REGISTER,
            json!({"capabilities": ["presence_capture"], "zones": ["living-room"], "screen_anchors": []}),
        ),
    );
    write_frame(&mut node.stream, &register, &[]).unwrap();
    let outcome = read_frame(&mut node.stream);
    // either an explicit rejection or the hub closing the socket: both loud
    if let Ok((reply, _)) = outcome {
        assert_eq!(reply.message.body["success"], json!(false));
    }
}

#[test]
fn bad_token_is_rejected() {
    let hub = start("");
    let stream = TcpStream::connect(hub.addr).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut node = Node {
        stream,
        producer_id: "brain".into(),
    };
    let register = Envelope::new(
        INSTALLATION,
        "brain",
        None,
        "wrong-wrong-wrong-wrong-wrong-wrong",
        Message::new(
            kinds::V1_REGISTER,
            json!({"capabilities": ["display"], "zones": ["default"]}),
        ),
    );
    write_frame(&mut node.stream, &register, &[]).unwrap();
    assert!(read_frame(&mut node.stream).is_err());
}

#[test]
fn stale_envelope_closes_the_connection() {
    let hub = start("");
    let mut brain = hub.node("brain", &["canvas.command"], &[]);
    let mut old = Envelope::new(
        INSTALLATION,
        "brain",
        None,
        TOKEN,
        Message::new("canvas_command", json!({"command": 1})),
    );
    old.issued_at_unix_ms -= 60_000;
    write_frame(&mut brain.stream, &old, &[]).unwrap();
    assert!(read_frame(&mut brain.stream).is_err());
}

/// Test plugin: a latest-value store keyed by body `zone`, fanning the
/// stored value out to `display` holders — the shape of JARVIS presence,
/// written as an L2 plugin rather than baked into the relay.
#[derive(Default)]
struct SnapshotRelay;

impl HubPlugin for SnapshotRelay {
    fn handle(
        &self,
        hub: &HubHandle,
        source: &str,
        envelope: &PluginEnvelope,
        _payload: &[u8],
    ) -> Result<bool> {
        if envelope.message.kind != "test.publish" {
            return Ok(false);
        }
        let allowed = hub
            .grant_of(source)
            .map(|grant| grant.can("presence_capture"))
            .unwrap_or(false);
        anyhow::ensure!(allowed, "node {source} not authorized to publish");
        let zone = envelope.message.body["zone"].as_str().unwrap_or("default");
        let summary = Envelope::new(
            hub.installation_id(),
            source,
            Some(zone),
            "",
            Message::new(
                "test.snapshot",
                serde_json::Value::Object(envelope.message.body.clone()),
            ),
        );
        hub.send_to_capability("display", zone, source, &summary, &[]);
        Ok(true)
    }
}

#[test]
fn plugin_can_gate_and_fan_out() {
    let hub = TestHub::with(
        r#"{"kind": "test.publish", "target": {"mode": "capability", "name": "display"}},"#,
        vec![Arc::new(SnapshotRelay)],
    );
    let mut tv = hub.node("tv", &["display"], &["screen-tv"]);
    let mut sensor = hub.node("brain", &["presence_capture", "display"], &[]);

    sensor
        .send(
            Message::new(
                "test.publish",
                json!({"zone": "living-room", "occupied": true}),
            ),
            None,
        )
        .unwrap();
    let got = tv.recv_until("test.snapshot");
    assert_eq!(got.message.body["occupied"], json!(true));
    let ack = sensor.recv_reply();
    assert_eq!(ack.message.body["success"], json!(true));

    // a node without presence_capture may not publish
    let mut rogue = hub.node("studio", &["display"], &["screen-left"]);
    rogue
        .send(
            Message::new("test.publish", json!({"zone": "default"})),
            None,
        )
        .unwrap();
    let ack = rogue.recv_reply();
    assert_eq!(ack.message.body["success"], json!(false));
}

#[test]
fn heartbeat_keeps_registration_and_reconnect_works() {
    let hub = start("");
    let mut tv = hub.node("tv", &["display"], &["screen-tv"]);
    tv.heartbeat();
    let mut brain = hub.node("brain", &["canvas.command"], &[]);
    brain
        .send(
            Message::new("canvas_command", json!({"command": 1})),
            Some("living-room"),
        )
        .unwrap();
    tv.recv_until("canvas_command");
    drop(tv);
    // After the original connection drops, a re-registration restores it.
    let mut tv2 = hub.node("tv", &["display"], &["screen-tv"]);
    brain
        .send(
            Message::new("canvas_command", json!({"command": 2})),
            Some("living-room"),
        )
        .unwrap();
    let got = tv2.recv_until("canvas_command");
    assert_eq!(got.message.body["command"], json!(2));
}

#[test]
fn producer_mode_routes_response_by_destination() {
    let hub = TestHub::with(
        r#"{"kind": "fr_response", "target": {"mode": "producer"}},"#,
        vec![],
    );
    let mut face_id = hub.node("brain", &["canvas.command"], &[]);
    let mut requester = hub.node("tv", &["display"], &["screen-tv"]);
    let _other = hub.node("studio", &["display"], &["screen-left"]);

    face_id
        .send(
            Message::new("fr_response", json!({"faces": []})),
            Some("tv"),
        )
        .unwrap();
    let got = requester.recv_until("fr_response");
    assert_eq!(got.message.body["faces"], json!([]));
    let first_ack = face_id.recv_reply();
    assert_eq!(first_ack.message.body["success"], json!(true));

    // unknown destination fails the ack loudly
    face_id
        .send(
            Message::new("fr_response", json!({"faces": []})),
            Some("ghost"),
        )
        .unwrap();
    let (ack, _) = face_id.recv();
    assert_eq!(ack.message.body["success"], json!(false));
}

/// Broadcast relay: the shape of JARVIS's mute propagation, as a plugin.
#[derive(Default)]
struct MuteRelay;

impl HubPlugin for MuteRelay {
    fn handle(
        &self,
        hub: &HubHandle,
        source: &str,
        envelope: &PluginEnvelope,
        _payload: &[u8],
    ) -> Result<bool> {
        if envelope.message.kind != "mute_request" {
            return Ok(false);
        }
        let state = Envelope::new(
            hub.installation_id(),
            source,
            None,
            "",
            Message::new("mute_state", json!({"muted": true, "actor": source})),
        );
        hub.broadcast(source, &state, &[]);
        Ok(true)
    }
}

#[test]
fn broadcast_plugin_reaches_every_other_node() {
    let hub = TestHub::with("", vec![Arc::new(MuteRelay)]);
    let mut tv = hub.node("tv", &["display"], &["screen-tv"]);
    let mut studio = hub.node("studio", &["display"], &["screen-left"]);
    let mut muter = hub.node("brain", &["canvas.command"], &[]);

    muter
        .send(Message::new("mute_request", json!({"global": true})), None)
        .unwrap();
    for listener in [&mut tv, &mut studio] {
        let got = listener.recv_until("mute_state");
        assert_eq!(got.message.body["muted"], json!(true));
    }
}

#[test]
fn second_same_producer_connection_survives_firsts_disconnect() {
    // JARVIS Sound dials the hub twice under one producer id (status +
    // remote input). The first connection's cleanup must not evict the
    // registration or socket slot of the one that replaced it.
    let hub = start("");
    let first = hub.node("tv", &["display"], &["screen-tv"]);
    let mut second = hub.node("tv", &["display"], &["screen-tv"]);
    drop(first);
    std::thread::sleep(Duration::from_millis(150));

    let mut brain = hub.node("brain", &["canvas.command"], &[]);
    brain
        .send(
            Message::new("canvas_command", json!({"command": {"n": 1}})),
            Some("living-room"),
        )
        .unwrap();
    let (ack, _) = brain.recv();
    assert_eq!(
        ack.message.body["success"],
        json!(true),
        "tv slot was evicted"
    );
    let got = second.recv_until("canvas_command");
    assert_eq!(got.message.body["command"]["n"], json!(1));
}

#[test]
fn producers_learn_when_a_peer_goes_offline() {
    let hub = start("");
    let mut watcher = hub.node("studio", &["display"], &["screen-left"]);
    let transient = hub.node("tv", &["display"], &["screen-tv"]);
    let online = watcher.recv_until("unibus.node_state");
    assert_eq!(online.message.body["producer_id"], json!("tv"));
    assert_eq!(online.message.body["online"], json!(true));
    drop(transient);
    let offline = watcher.recv_until("unibus.node_state");
    assert_eq!(offline.message.body["producer_id"], json!("tv"));
    assert_eq!(offline.message.body["online"], json!(false));
}

#[test]
fn v1_peers_never_see_hub_advisories() {
    let hub = start("");
    let mut v1 = hub.node_at_schema(1, "tv", &["display"]);
    v1.stream
        .set_read_timeout(Some(Duration::from_millis(300)))
        .unwrap();
    let mut brain = hub.node("brain", &["canvas.command"], &[]);
    brain
        .send(
            Message::new("canvas_command", json!({"command": {}})),
            Some("nowhere"),
        )
        .unwrap();
    // tv must see nothing: not the unroutable-command ack (goes to brain),
    // not the schema-2 brain's presence in advisories.
    assert!(read_frame(&mut v1.stream).is_err());
    // v2 peers DO get advisories: tv's own offline announcement reaches brain.
    drop(v1);
    let online = brain.recv_until("unibus.node_state");
    assert_eq!(online.message.body["producer_id"], json!("tv"));
    assert_eq!(online.message.body["online"], json!(false));
}
