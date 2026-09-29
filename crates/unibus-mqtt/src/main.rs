use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use rumqttc::{
    Client as MqttClient, Event, Incoming, MqttOptions, QoS, Transport, matches, valid_filter,
};
use serde::Deserialize;
use tracing::{info, warn};
use unibus::Message;
use unibus::client::{Client as UnibusClient, ClientConfig};
use unibus_mqtt::{
    BridgedPublication, EchoSuppressor, MAX_MQTT_PAYLOAD, MQTT_BRIDGE_SCHEMA_VERSION,
    MQTT_PUBLISH_CONTRACT, MqttQos, PublicationFingerprint, new_message_id,
    valid_bridge_identifier,
};

const DEFAULT_QUEUE_CAPACITY: usize = 128;
const DEFAULT_ECHO_CAPACITY: usize = 4_096;
const DEFAULT_ECHO_TTL_MS: u64 = 10_000;
const DEFAULT_KEEP_ALIVE_SECS: u64 = 30;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct BridgeConfig {
    namespace: String,
    mqtt: BrokerConfig,
    bus: ClientConfig,
    subscriptions: Vec<Subscription>,
    #[serde(default = "default_queue_capacity")]
    queue_capacity: usize,
    #[serde(default = "default_echo_capacity")]
    echo_capacity: usize,
    #[serde(default = "default_echo_ttl_ms")]
    echo_ttl_ms: u64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct BrokerConfig {
    host: String,
    port: u16,
    client_id: String,
    #[serde(default)]
    tls: bool,
    #[serde(default)]
    username_env: Option<String>,
    #[serde(default)]
    password_env: Option<String>,
    #[serde(default = "default_keep_alive_secs")]
    keep_alive_secs: u64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Subscription {
    filter: String,
    #[serde(default = "default_subscription_qos")]
    qos: MqttQos,
}

fn default_queue_capacity() -> usize {
    DEFAULT_QUEUE_CAPACITY
}

fn default_echo_capacity() -> usize {
    DEFAULT_ECHO_CAPACITY
}

fn default_echo_ttl_ms() -> u64 {
    DEFAULT_ECHO_TTL_MS
}

fn default_keep_alive_secs() -> u64 {
    DEFAULT_KEEP_ALIVE_SECS
}

fn default_subscription_qos() -> MqttQos {
    MqttQos::AtLeastOnce
}

impl BridgeConfig {
    fn read(path: &Path) -> Result<Self> {
        let bytes = std::fs::read(path)
            .with_context(|| format!("read bridge config {}", path.display()))?;
        let config: Self = serde_json::from_slice(&bytes)
            .with_context(|| format!("parse bridge config {}", path.display()))?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<()> {
        ensure!(
            valid_bridge_identifier(&self.namespace),
            "invalid bridge namespace"
        );
        ensure!(!self.mqtt.host.is_empty(), "MQTT host is empty");
        ensure!(!self.mqtt.client_id.is_empty(), "MQTT client_id is empty");
        ensure!(
            valid_bridge_identifier(&self.mqtt.client_id),
            "MQTT client_id must be a bridge identifier"
        );
        ensure!(self.queue_capacity > 0, "queue_capacity must be positive");
        ensure!(self.echo_capacity > 0, "echo_capacity must be positive");
        ensure!(self.echo_ttl_ms > 0, "echo_ttl_ms must be positive");
        ensure!(
            !self.subscriptions.is_empty(),
            "at least one MQTT subscription is required"
        );
        for subscription in &self.subscriptions {
            ensure!(
                valid_filter(&subscription.filter),
                "invalid MQTT subscription filter {}",
                subscription.filter
            );
        }
        ensure!(
            self.mqtt.username_env.is_some() == self.mqtt.password_env.is_some(),
            "MQTT username_env and password_env must be set together"
        );
        for name in [
            self.mqtt.username_env.as_deref(),
            self.mqtt.password_env.as_deref(),
        ]
        .into_iter()
        .flatten()
        {
            ensure!(
                valid_env_name(name),
                "invalid environment variable name {name}"
            );
        }
        ensure!(
            self.bus.schema_version == unibus::SCHEMA_VERSION_V2,
            "MQTT bridge requires Unibus schema_version 2"
        );
        ensure!(
            self.bus
                .capabilities
                .iter()
                .any(|value| value == "mqtt.bridge"),
            "bus capabilities must include mqtt.bridge"
        );
        Ok(())
    }
}

fn valid_env_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    matches!(bytes.next(), Some(b'A'..=b'Z' | b'_'))
        && bytes.all(|byte| matches!(byte, b'A'..=b'Z' | b'0'..=b'9' | b'_'))
}

fn to_rumqtt_qos(qos: MqttQos) -> QoS {
    match qos {
        MqttQos::AtMostOnce => QoS::AtMostOnce,
        MqttQos::AtLeastOnce => QoS::AtLeastOnce,
        MqttQos::ExactlyOnce => QoS::ExactlyOnce,
    }
}

fn from_rumqtt_qos(qos: QoS) -> MqttQos {
    match qos {
        QoS::AtMostOnce => MqttQos::AtMostOnce,
        QoS::AtLeastOnce => MqttQos::AtLeastOnce,
        QoS::ExactlyOnce => MqttQos::ExactlyOnce,
    }
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let path = std::env::args_os()
        .nth(1)
        .context("usage: unibus-mqtt <bridge.json>")?;
    run(BridgeConfig::read(Path::new(&path))?)
}

fn run(config: BridgeConfig) -> Result<()> {
    let mut options = MqttOptions::new(
        config.mqtt.client_id.clone(),
        config.mqtt.host.clone(),
        config.mqtt.port,
    );
    options
        .set_keep_alive(Duration::from_secs(config.mqtt.keep_alive_secs))
        .set_clean_session(false)
        .set_max_packet_size(unibus::envelope::MAX_FRAME as usize, MAX_MQTT_PAYLOAD);
    if config.mqtt.tls {
        options.set_transport(Transport::tls_with_default_config());
    }
    if let (Some(username), Some(password)) = (
        config.mqtt.username_env.as_deref(),
        config.mqtt.password_env.as_deref(),
    ) {
        options.set_credentials(
            std::env::var(username).with_context(|| format!("read {username}"))?,
            std::env::var(password).with_context(|| format!("read {password}"))?,
        );
    }

    let (mqtt, mut connection) = MqttClient::new(options, config.queue_capacity);
    for subscription in &config.subscriptions {
        mqtt.subscribe(&subscription.filter, to_rumqtt_qos(subscription.qos))
            .with_context(|| format!("subscribe to {}", subscription.filter))?;
    }

    let bus = Arc::new(UnibusClient::connect(config.bus)?);
    let echoes = Arc::new(Mutex::new(EchoSuppressor::new(
        config.echo_capacity,
        Duration::from_millis(config.echo_ttl_ms),
    )?));
    let reader = bus.clone();
    let publisher = mqtt.clone();
    let reader_echoes = echoes.clone();
    let namespace = config.namespace.clone();
    let bridge_id = config.mqtt.client_id.clone();
    let allowed_filters: Vec<String> = config
        .subscriptions
        .iter()
        .map(|subscription| subscription.filter.clone())
        .collect();
    std::thread::Builder::new()
        .name("unibus-mqtt-egress".into())
        .spawn(move || {
            if let Err(error) = forward_unibus_to_mqtt(
                &reader,
                &publisher,
                &reader_echoes,
                &namespace,
                &bridge_id,
                &allowed_filters,
            ) {
                // A stopped egress path makes bidirectional operation false;
                // abort loudly instead of leaving an ingress-only process.
                tracing::error!(%error, "Unibus-to-MQTT forwarding stopped");
                std::process::abort();
            }
        })?;

    info!(
        namespace = %config.namespace,
        broker = %config.mqtt.host,
        "MQTT bridge started"
    );
    for notification in connection.iter() {
        match notification.context("MQTT connection failed")? {
            Event::Incoming(Incoming::Publish(publish)) => {
                let qos = from_rumqtt_qos(publish.qos);
                let fingerprint = PublicationFingerprint::new(
                    &publish.topic,
                    qos,
                    publish.retain,
                    &publish.payload,
                );
                if echoes
                    .lock()
                    .expect("echo cache poisoned")
                    .take(&fingerprint)
                {
                    continue;
                }
                let publication = BridgedPublication {
                    schema_version: MQTT_BRIDGE_SCHEMA_VERSION,
                    namespace: config.namespace.clone(),
                    topic: publish.topic,
                    qos,
                    retain: publish.retain,
                    origin: config.mqtt.client_id.clone(),
                    message_id: new_message_id()?,
                };
                publication.validate(publish.payload.len())?;
                let mut envelope = bus.envelope(
                    None,
                    Message::new(MQTT_PUBLISH_CONTRACT, serde_json::to_value(&publication)?),
                )?;
                envelope.payload_bytes = publish.payload.len();
                bus.send(&envelope, &publish.payload)?;
            }
            Event::Incoming(Incoming::ConnAck(_)) => {
                info!("connected to MQTT broker");
            }
            _ => {}
        }
    }
    anyhow::bail!("MQTT event loop ended")
}

fn forward_unibus_to_mqtt(
    bus: &UnibusClient,
    mqtt: &MqttClient,
    echoes: &Mutex<EchoSuppressor>,
    namespace: &str,
    bridge_id: &str,
    allowed_filters: &[String],
) -> Result<()> {
    loop {
        let Some((envelope, payload)) = bus.try_read()? else {
            continue;
        };
        if matches!(
            unibus::kinds::classify(&envelope.message.kind),
            unibus::kinds::Control::Reply
        ) {
            ensure!(
                envelope
                    .message
                    .body
                    .get("success")
                    .and_then(|value| value.as_bool())
                    == Some(true),
                "hub rejected MQTT publication: {:?}",
                envelope.message.body
            );
            continue;
        }
        if envelope.message.kind == unibus::kinds::NODE_STATE {
            continue;
        }
        if envelope.message.kind != MQTT_PUBLISH_CONTRACT {
            warn!(kind = %envelope.message.kind, "ignoring unexpected Unibus message");
            continue;
        }
        let publication: BridgedPublication =
            serde_json::from_value(serde_json::Value::Object(envelope.message.body))?;
        publication.validate(payload.len())?;
        if publication.namespace != namespace || publication.origin == bridge_id {
            continue;
        }
        if !allowed_filters
            .iter()
            .any(|filter| matches(&publication.topic, filter))
        {
            warn!(topic = %publication.topic, "rejecting remote MQTT topic outside configured filters");
            continue;
        }
        let fingerprint = PublicationFingerprint::new(
            &publication.topic,
            publication.qos,
            publication.retain,
            &payload,
        );
        echoes
            .lock()
            .expect("echo cache poisoned")
            .mark(fingerprint);
        mqtt.publish(
            publication.topic,
            to_rumqtt_qos(publication.qos),
            publication.retain,
            payload,
        )?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> BridgeConfig {
        serde_json::from_str(
            r#"{
                "namespace":"building-a",
                "mqtt":{"host":"127.0.0.1","port":1883,"client_id":"bridge-west"},
                "bus":{
                    "installation_id":"home","producer_id":"bridge-west",
                    "hub_endpoint":"127.0.0.1:18790","token_env":"UNIBUS_TOKEN",
                    "zones":["default"],"capabilities":["mqtt.bridge"],"schema_version":2
                },
                "subscriptions":[{"filter":"sensors/#"}]
            }"#,
        )
        .unwrap()
    }

    #[test]
    fn minimal_config_is_valid() {
        config().validate().unwrap();
    }

    #[test]
    fn credentials_must_be_a_pair() {
        let mut config = config();
        config.mqtt.username_env = Some("MQTT_USER".into());
        assert!(
            config
                .validate()
                .unwrap_err()
                .to_string()
                .contains("together")
        );
    }

    #[test]
    fn bridge_requires_v2_capability() {
        let mut config = config();
        config.bus.capabilities.clear();
        assert!(
            config
                .validate()
                .unwrap_err()
                .to_string()
                .contains("mqtt.bridge")
        );
    }
}
