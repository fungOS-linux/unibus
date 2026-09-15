//! Golden vectors: frames exactly as JARVIS master (`jarvis-router`,
//! `jarvis-edge`, `jarvis-core::edge_transport`) serializes them must parse
//! into unibus envelopes, and unibus envelopes must parse back into the
//! jarvis shape. This file is the compatibility contract between the repos —
//! when JARVIS changes its wire, a vector here fails first.

use serde_json::json;
use std::io::Cursor;
use unibus::envelope::{Envelope, MAX_FRAME, SCHEMA_VERSION_V1};
use unibus::framing::{read_frame, write_frame};
use unibus::qos::Qos;

fn parse(vector: &str) -> Envelope {
    serde_json::from_str(vector).unwrap_or_else(|e| panic!("vector must parse: {e}\n{vector}"))
}

#[test]
fn jarvis_v1_register_parses() {
    let envelope = parse(
        r#"{"schema_version":1,"installation_id":"home","producer_id":"macbook-neo-display","destination":null,"target_screen":null,"request_id":"router-1-2-3","issued_at_unix_ms":1700000000000,"ttl_ms":30000,"payload_bytes":0,"token":"tok","message":{"message_type":"register","capabilities":["display"],"zones":["default"],"screen_anchors":["screen-left","screen-right"]}}"#,
    );
    assert_eq!(envelope.schema_version, SCHEMA_VERSION_V1);
    assert!(envelope.message.is("register"));
    assert_eq!(
        envelope.message.body["screen_anchors"],
        json!(["screen-left", "screen-right"])
    );
    assert_eq!(Qos::of_kind(&envelope.message.kind), Qos::Command);
}

#[test]
fn jarvis_v1_canvas_command_for_screen_parses() {
    let envelope = parse(
        r#"{"schema_version":1,"installation_id":"home","producer_id":"mac-studio-brain","destination":null,"target_screen":"screen-tv","request_id":"router-9-9-9","issued_at_unix_ms":1700000000000,"ttl_ms":30000,"payload_bytes":0,"token":"tok","message":{"message_type":"canvas_command","command":{"schema_version":1,"request_id":"test-1","command":{"upsert_indicator":{"id":"jarvis-voice","state":"speaking"}}}}}"#,
    );
    assert_eq!(envelope.target_screen.as_deref(), Some("screen-tv"));
    assert_eq!(
        envelope.message.body["command"]["command"]["upsert_indicator"]["state"],
        "speaking"
    );
}

#[test]
fn jarvis_v1_presence_publish_parses_as_snapshot() {
    let envelope = parse(
        r#"{"schema_version":1,"installation_id":"home","producer_id":"presence-bedroom","destination":null,"target_screen":null,"request_id":"r-1","issued_at_unix_ms":1700000000000,"ttl_ms":30000,"payload_bytes":0,"token":"tok","message":{"message_type":"publish_presence","snapshot":{"zone":{"0":"bedroom"},"occupied":true,"recency_ms":100}}}"#,
    );
    assert!(envelope.message.is("publish_presence"));
    assert_eq!(Qos::of_kind(&envelope.message.kind), Qos::Snapshot);
}

#[test]
fn jarvis_v1_reply_parses() {
    let envelope = parse(
        r#"{"schema_version":1,"installation_id":"home","producer_id":"router","destination":null,"target_screen":null,"request_id":"r-2","issued_at_unix_ms":1700000000000,"ttl_ms":30000,"payload_bytes":0,"token":"","message":{"message_type":"reply","success":true,"message":"registered","request_id":null}}"#,
    );
    assert_eq!(envelope.message.body["success"], json!(true));
}

#[test]
fn unknown_kind_routes_opaquely() {
    // L1 must accept and route a message type it has never heard of.
    let envelope = parse(
        r#"{"schema_version":2,"installation_id":"home","producer_id":"sensor-1","destination":null,"target_screen":null,"request_id":"r-3","issued_at_unix_ms":1700000000000,"ttl_ms":30000,"payload_bytes":0,"token":"tok","message":{"message_type":"thermostat.observation","degrees_c":21.5}}"#,
    );
    assert_eq!(envelope.message.domain(), "thermostat");
    assert_eq!(envelope.message.body["degrees_c"], json!(21.5));
}

#[test]
fn frames_survive_a_jarvis_shaped_round_trip() {
    let envelope = parse(
        r#"{"schema_version":1,"installation_id":"home","producer_id":"mac-studio-brain","destination":"home-pi-audio","target_screen":null,"request_id":"r-4","issued_at_unix_ms":1700000000000,"ttl_ms":30000,"payload_bytes":8,"token":"tok","message":{"message_type":"audio_playback_chunk","format":"s16_le","sample_rate_hz":48000,"channels":2,"timestamp_ms":1,"payload_bytes":8}}"#,
    );
    let payload = vec![1u8, 2, 3, 4, 5, 6, 7, 8];
    let mut buf = Vec::new();
    write_frame(&mut buf, &envelope, &payload).unwrap();
    let (read, read_payload) = read_frame(&mut Cursor::new(&buf)).unwrap();
    assert_eq!(read, envelope);
    assert_eq!(read_payload, payload);
}

#[test]
fn frame_length_beyond_max_is_rejected_without_allocating() {
    let mut cursor = Cursor::new((MAX_FRAME + 1).to_be_bytes().to_vec());
    let err = read_frame(&mut cursor).unwrap_err();
    assert!(err.to_string().contains("too large"));
}
