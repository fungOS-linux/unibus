//! The transport envelope. Field names and JSON shape are frozen against the
//! JARVIS router v1 wire (`RouterEnvelope`); a v1 frame from `jarvis-router`
//! or `jarvis-edge` must parse here unchanged, and vice versa.

use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// JARVIS router v1 schema version. v1 frames stay valid forever.
pub const SCHEMA_VERSION_V1: u32 = 1;
/// Namespaced message kinds and capabilities. Same envelope shape as v1.
pub const SCHEMA_VERSION_V2: u32 = 2;

/// Maximum frame size: 16 MiB (bulk bytes belong on the content plane).
pub const MAX_FRAME: u32 = 16 * 1024 * 1024;

/// Default freshness window for a control envelope.
pub const DEFAULT_TTL_MS: u64 = 30_000;

/// An opaque, routed message: `message_type` is the routing name, the rest of
/// the object is the body. L1 never reads the body.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    #[serde(rename = "message_type")]
    pub kind: String,
    #[serde(flatten)]
    pub body: Map<String, Value>,
}

impl Message {
    pub fn new(kind: &str, body: Value) -> Self {
        let body = match body {
            Value::Object(map) => map,
            Value::Null => Map::new(),
            other => {
                let mut map = Map::new();
                map.insert("value".into(), other);
                map
            }
        };
        Self {
            kind: kind.to_owned(),
            body,
        }
    }

    pub fn is(&self, kind: &str) -> bool {
        self.kind == kind
    }

    /// Domain prefix for v2 namespaced kinds (`canvas.command` -> `canvas`);
    /// v1 snake_case tags have no namespace and map to `core`.
    pub fn domain(&self) -> &str {
        match self.kind.find('.') {
            Some(i) => &self.kind[..i],
            None => "core",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnvelopeError {
    UnsupportedVersion(u32),
    InstallationMismatch,
    Stale { age_ms: u64, ttl_ms: u64 },
    FutureDated { skew_ms: u64 },
    EmptyField(&'static str),
    PayloadSizeMismatch { declared: usize, actual: usize },
    Malformed(String),
}

impl fmt::Display for EnvelopeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedVersion(v) => write!(f, "unsupported unibus schema version {v}"),
            Self::InstallationMismatch => write!(f, "installation mismatch"),
            Self::Stale { age_ms, ttl_ms } => {
                write!(f, "envelope stale: age {age_ms} ms exceeds ttl {ttl_ms} ms")
            }
            Self::FutureDated { skew_ms } => {
                write!(f, "envelope future-dated by {skew_ms} ms")
            }
            Self::EmptyField(name) => write!(f, "required envelope field is empty: {name}"),
            Self::PayloadSizeMismatch { declared, actual } => {
                write!(
                    f,
                    "payload size mismatch: declared {declared}, actual {actual}"
                )
            }
            Self::Malformed(msg) => write!(f, "malformed envelope: {msg}"),
        }
    }
}

impl std::error::Error for EnvelopeError {}

/// Top-level envelope wrapping a routed message.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Envelope {
    pub schema_version: u32,
    pub installation_id: String,
    pub producer_id: String,
    pub destination: Option<String>,
    /// Target a specific screen anchor; set means deliver only to the live
    /// display node claiming that anchor.
    pub target_screen: Option<String>,
    pub request_id: String,
    pub issued_at_unix_ms: u64,
    pub ttl_ms: Option<u64>,
    pub payload_bytes: usize,
    pub token: String,
    pub message: Message,
}

/// How much a clock may run ahead of the validator before rejection.
pub const MAX_FUTURE_SKEW_MS: u64 = 5_000;

impl Envelope {
    pub fn new(
        installation_id: &str,
        producer_id: &str,
        destination: Option<&str>,
        token: &str,
        message: Message,
    ) -> Self {
        Self {
            schema_version: SCHEMA_VERSION_V2,
            installation_id: installation_id.to_owned(),
            producer_id: producer_id.to_owned(),
            destination: destination.map(str::to_owned),
            target_screen: None,
            request_id: next_request_id(),
            issued_at_unix_ms: now_ms(),
            ttl_ms: Some(DEFAULT_TTL_MS),
            payload_bytes: 0,
            token: token.to_owned(),
            message,
        }
    }

    /// Envelope addressed to one screen anchor.
    pub fn for_screen(
        installation_id: &str,
        producer_id: &str,
        token: &str,
        screen_anchor: &str,
        message: Message,
    ) -> Self {
        let mut envelope = Self::new(installation_id, producer_id, None, token, message);
        envelope.target_screen = Some(screen_anchor.to_owned());
        envelope
    }

    /// Freshness and identity checks, with the clock injected for tests.
    pub fn validate(&self, installation_id: &str, now_unix_ms: u64) -> Result<(), EnvelopeError> {
        if self.schema_version != SCHEMA_VERSION_V1 && self.schema_version != SCHEMA_VERSION_V2 {
            return Err(EnvelopeError::UnsupportedVersion(self.schema_version));
        }
        if self.installation_id != installation_id {
            return Err(EnvelopeError::InstallationMismatch);
        }
        for (name, value) in [
            ("installation_id", &self.installation_id),
            ("producer_id", &self.producer_id),
            ("request_id", &self.request_id),
        ] {
            if value.trim().is_empty() || value.len() > 128 {
                return Err(EnvelopeError::EmptyField(name));
            }
        }
        if self.message.kind.trim().is_empty() {
            return Err(EnvelopeError::EmptyField("message_type"));
        }
        let ttl = self.ttl_ms.unwrap_or(DEFAULT_TTL_MS);
        if self.issued_at_unix_ms > now_unix_ms + MAX_FUTURE_SKEW_MS {
            return Err(EnvelopeError::FutureDated {
                skew_ms: self.issued_at_unix_ms - now_unix_ms,
            });
        }
        let age = now_unix_ms.saturating_sub(self.issued_at_unix_ms);
        if age > ttl {
            return Err(EnvelopeError::Stale {
                age_ms: age,
                ttl_ms: ttl,
            });
        }
        Ok(())
    }
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn next_request_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("unibus-{}-{ts}-{seq}", std::process::id(), ts = now_ms())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message() -> Message {
        Message::new("heartbeat", Value::Null)
    }

    #[test]
    fn validate_accepts_fresh_envelope() {
        let envelope = Envelope::new("home", "node-a", None, "", message());
        assert_eq!(envelope.validate("home", now_ms()), Ok(()));
    }

    #[test]
    fn validate_rejects_stale_and_future() {
        let envelope = Envelope::new("home", "node-a", None, "", message());
        assert!(matches!(
            envelope.validate("home", envelope.issued_at_unix_ms + 31_000),
            Err(EnvelopeError::Stale { .. })
        ));
        assert!(matches!(
            envelope.validate("home", envelope.issued_at_unix_ms.saturating_sub(60_000)),
            Err(EnvelopeError::FutureDated { .. })
        ));
        assert_eq!(
            envelope.validate("other", now_ms()),
            Err(EnvelopeError::InstallationMismatch)
        );
    }

    #[test]
    fn domain_splits_v2_and_defaults_to_core_for_v1() {
        assert_eq!(
            Message::new("canvas.command", Value::Null).domain(),
            "canvas"
        );
        assert_eq!(
            Message::new("publish_presence", Value::Null).domain(),
            "core"
        );
    }

    #[test]
    fn message_body_round_trips_through_flatten() {
        let envelope = Envelope::for_screen(
            "home",
            "brain",
            "tok",
            "screen-tv",
            Message::new(
                "canvas.command",
                serde_json::json!({"command": {"op": "ping"}}),
            ),
        );
        let json = serde_json::to_string(&envelope).unwrap();
        let parsed: Envelope = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, envelope);
        assert_eq!(parsed.target_screen.as_deref(), Some("screen-tv"));
        assert_eq!(parsed.message.body["command"]["op"].as_str(), Some("ping"));
    }
}
