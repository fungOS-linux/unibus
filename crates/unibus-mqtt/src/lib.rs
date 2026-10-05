//! Opaque MQTT-over-Unibus contract and bounded loop suppression.
//!
//! MQTT brokers retain ownership of sessions, retained publications, and
//! protocol acknowledgements. Unibus transports a publication hop-to-hop;
//! L1 never interprets this body.

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
pub mod subscriber;

pub const MQTT_PUBLISH_CONTRACT: &str = "mqtt.publish.v1";
pub const MQTT_BRIDGE_CAPABILITY: &str = "mqtt.bridge";
pub const MQTT_BRIDGE_SCHEMA_VERSION: u32 = 1;
/// Leave room for the JSON envelope beneath Unibus's 16 MiB frame ceiling.
pub const MAX_MQTT_PAYLOAD: usize = 15 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MqttQos {
    AtMostOnce,
    AtLeastOnce,
    ExactlyOnce,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BridgedPublication {
    pub schema_version: u32,
    pub namespace: String,
    pub topic: String,
    pub qos: MqttQos,
    pub retain: bool,
    pub origin: String,
    pub message_id: String,
}

impl BridgedPublication {
    pub fn validate(&self, payload_len: usize) -> Result<()> {
        ensure!(
            self.schema_version == MQTT_BRIDGE_SCHEMA_VERSION,
            "unsupported MQTT bridge schema {}",
            self.schema_version
        );
        ensure!(
            valid_bridge_identifier(&self.namespace),
            "invalid bridge namespace"
        );
        ensure!(!self.topic.is_empty(), "MQTT topic is empty");
        ensure!(!self.topic.contains('\0'), "MQTT topic contains NUL");
        ensure!(
            !self.topic.contains(['#', '+']),
            "publication topic contains wildcard"
        );
        ensure!(
            valid_bridge_identifier(&self.origin),
            "invalid bridge origin"
        );
        ensure!(
            valid_message_id(&self.message_id),
            "invalid bridge message_id"
        );
        ensure!(payload_len <= MAX_MQTT_PAYLOAD, "MQTT payload is too large");
        Ok(())
    }
}

pub fn valid_bridge_identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

fn valid_message_id(value: &str) -> bool {
    value.len() == 32 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

pub fn new_message_id() -> Result<String> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes)?;
    Ok(hex(&bytes))
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(DIGITS[(byte >> 4) as usize] as char);
        output.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    output
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PublicationFingerprint([u8; 32]);

impl PublicationFingerprint {
    pub fn new(topic: &str, qos: MqttQos, retain: bool, payload: &[u8]) -> Self {
        let mut hash = Sha256::new();
        hash.update((topic.len() as u64).to_be_bytes());
        hash.update(topic.as_bytes());
        hash.update([qos as u8, retain as u8]);
        hash.update((payload.len() as u64).to_be_bytes());
        hash.update(payload);
        Self(hash.finalize().into())
    }
}

/// A bounded, expiring set. `take` consumes one marker so a later identical
/// local publication is not suppressed indefinitely.
pub struct EchoSuppressor {
    capacity: usize,
    ttl: Duration,
    entries: HashMap<PublicationFingerprint, Instant>,
    order: VecDeque<(PublicationFingerprint, Instant)>,
}

impl EchoSuppressor {
    pub fn new(capacity: usize, ttl: Duration) -> Result<Self> {
        ensure!(capacity > 0, "echo cache capacity must be positive");
        ensure!(!ttl.is_zero(), "echo cache TTL must be positive");
        Ok(Self {
            capacity,
            ttl,
            entries: HashMap::new(),
            order: VecDeque::new(),
        })
    }

    pub fn mark(&mut self, fingerprint: PublicationFingerprint) {
        self.prune();
        let expiry = Instant::now() + self.ttl;
        self.entries.insert(fingerprint.clone(), expiry);
        self.order.push_back((fingerprint, expiry));
        while self.entries.len() > self.capacity {
            self.evict_oldest();
        }
    }

    pub fn take(&mut self, fingerprint: &PublicationFingerprint) -> bool {
        self.prune();
        self.entries.remove(fingerprint).is_some()
    }

    fn prune(&mut self) {
        let now = Instant::now();
        while self.order.front().is_some_and(|(_, expiry)| *expiry <= now) {
            self.evict_oldest();
        }
    }

    fn evict_oldest(&mut self) {
        if let Some((fingerprint, expiry)) = self.order.pop_front()
            && self.entries.get(&fingerprint) == Some(&expiry)
        {
            self.entries.remove(&fingerprint);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn publication() -> BridgedPublication {
        BridgedPublication {
            schema_version: MQTT_BRIDGE_SCHEMA_VERSION,
            namespace: "building-a".into(),
            topic: "sensors/room/temperature".into(),
            qos: MqttQos::AtLeastOnce,
            retain: true,
            origin: "bridge-east".into(),
            message_id: "0123456789abcdef0123456789abcdef".into(),
        }
    }

    #[test]
    fn contract_round_trips_and_validates() {
        let publication = publication();
        publication.validate(3).unwrap();
        let json = serde_json::to_vec(&publication).unwrap();
        assert_eq!(
            serde_json::from_slice::<BridgedPublication>(&json).unwrap(),
            publication
        );
    }

    #[test]
    fn publication_topics_may_not_be_filters() {
        let mut publication = publication();
        publication.topic = "sensors/+/temperature".into();
        assert!(publication.validate(0).is_err());
    }

    #[test]
    fn echo_marker_is_consumed_once() {
        let fingerprint =
            PublicationFingerprint::new("events/button", MqttQos::AtMostOnce, false, b"down");
        let mut suppressor = EchoSuppressor::new(4, Duration::from_secs(1)).unwrap();
        suppressor.mark(fingerprint.clone());
        assert!(suppressor.take(&fingerprint));
        assert!(!suppressor.take(&fingerprint));
    }

    #[test]
    fn echo_cache_is_bounded() {
        let mut suppressor = EchoSuppressor::new(1, Duration::from_secs(1)).unwrap();
        let first = PublicationFingerprint::new("a", MqttQos::AtMostOnce, false, b"1");
        let second = PublicationFingerprint::new("b", MqttQos::AtMostOnce, false, b"2");
        suppressor.mark(first.clone());
        suppressor.mark(second.clone());
        assert!(!suppressor.take(&first));
        assert!(suppressor.take(&second));
    }
}
