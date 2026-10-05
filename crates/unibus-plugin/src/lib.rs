//! Read-only leased plugin-host advertisements. Discovery never authorizes loading.
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

pub const INVENTORY_CONTRACT: &str = "plugins.inventory.v1";
pub const DISCOVERY_CAPABILITY: &str = "plugins.discovery";
pub const MAX_PLUGINS: usize = 1024;
pub const MAX_HOSTS: usize = 64;
pub const MAX_TTL_MS: u64 = 30_000;

pub fn valid_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._:-".contains(&byte))
}
fn text(value: &str, max: usize) -> bool {
    value.len() <= max && !value.chars().any(char::is_control)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginRole {
    Instrument,
    Effect,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Plugin {
    /// VST3 Audio Module class UUID, uppercase hexadecimal; no host filesystem path.
    pub class_id: String,
    pub name: String,
    pub vendor: String,
    pub version: String,
    pub role: PluginRole,
    pub audio_inputs: u16,
    pub audio_outputs: u16,
    pub midi_input: bool,
    pub midi_output: bool,
    pub editor: bool,
    /// Multiple installed bundles with the same class are ambiguous, not silently selected.
    pub matching_installations: u16,
}
impl Plugin {
    pub fn selectable(&self) -> bool {
        self.matching_installations == 1
    }
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.class_id.len() == 32
                && self
                    .class_id
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'A'..=b'F').contains(&b)),
            "invalid VST3 class ID"
        );
        ensure!(
            !self.name.trim().is_empty()
                && text(&self.name, 256)
                && text(&self.vendor, 256)
                && text(&self.version, 128),
            "invalid plugin metadata"
        );
        ensure!(
            self.matching_installations > 0
                && self.audio_inputs <= 256
                && self.audio_outputs <= 256,
            "invalid plugin capabilities"
        );
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Inventory {
    pub schema_version: u32,
    pub producer_id: String,
    pub host_label: String,
    pub os: String,
    pub architecture: String,
    pub observed_at_unix_ms: u64,
    pub valid_until_unix_ms: u64,
    /// Scanning age is distinct from heartbeat freshness; cached metadata is not a fresh scan.
    pub scanned_at_unix_ms: Option<u64>,
    pub scan_pending: bool,
    pub scan_failures: u16,
    /// A successful status request, not just the presence of files on disk.
    pub host_reachable: bool,
    pub active_instance: Option<String>,
    pub plugins: Vec<Plugin>,
}
impl Inventory {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.schema_version == 1 && valid_id(&self.producer_id),
            "invalid plugin inventory identity/version"
        );
        ensure!(
            !self.host_label.trim().is_empty()
                && text(&self.host_label, 256)
                && valid_id(&self.os)
                && valid_id(&self.architecture),
            "invalid host metadata"
        );
        ensure!(
            self.valid_until_unix_ms > self.observed_at_unix_ms
                && self.valid_until_unix_ms - self.observed_at_unix_ms <= MAX_TTL_MS,
            "invalid plugin lease"
        );
        ensure!(
            self.scanned_at_unix_ms
                .is_none_or(|time| time <= self.observed_at_unix_ms),
            "future scan timestamp"
        );
        ensure!(
            self.active_instance.as_ref().is_none_or(|id| valid_id(id))
                && (self.host_reachable || self.active_instance.is_none()),
            "invalid active instance"
        );
        ensure!(self.plugins.len() <= MAX_PLUGINS, "too many plugins");
        let mut ids = BTreeSet::new();
        for plugin in &self.plugins {
            plugin.validate()?;
            ensure!(ids.insert(&plugin.class_id), "duplicate plugin class");
        }
        Ok(())
    }
    pub fn fresh(&self, now: u64) -> bool {
        unibus::envelope::lease_is_fresh(self.observed_at_unix_ms, self.valid_until_unix_ms, now)
    }
}

#[derive(Default)]
pub struct Registry {
    hosts: BTreeMap<String, Inventory>,
}
impl Registry {
    pub fn observe(
        &mut self,
        authenticated_producer: &str,
        inventory: Inventory,
        now: u64,
    ) -> Result<()> {
        inventory.validate()?;
        ensure!(
            inventory.producer_id == authenticated_producer,
            "plugin publisher mismatch"
        );
        ensure!(
            inventory.fresh(now),
            "expired or future plugin advertisement"
        );
        self.hosts.retain(|_, value| value.fresh(now));
        ensure!(
            self.hosts.contains_key(authenticated_producer) || self.hosts.len() < MAX_HOSTS,
            "plugin host registry full"
        );
        if let Some(previous) = self.hosts.get(authenticated_producer) {
            ensure!(
                inventory.observed_at_unix_ms > previous.observed_at_unix_ms,
                "replayed plugin advertisement"
            );
        }
        self.hosts.insert(authenticated_producer.into(), inventory);
        Ok(())
    }
    pub fn current(&mut self, now: u64) -> Vec<Inventory> {
        self.hosts.retain(|_, value| value.fresh(now));
        self.hosts.values().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn inventory() -> Inventory {
        Inventory {
            schema_version: 1,
            producer_id: "studio".into(),
            host_label: "Studio".into(),
            os: "macos".into(),
            architecture: "aarch64".into(),
            observed_at_unix_ms: 100,
            valid_until_unix_ms: 1100,
            scanned_at_unix_ms: Some(90),
            scan_pending: false,
            scan_failures: 0,
            host_reachable: true,
            active_instance: None,
            plugins: vec![Plugin {
                class_id: "ABCDEF019182FAEB5446544654334B30".into(),
                name: "amp".into(),
                vendor: "vendor".into(),
                version: "1".into(),
                role: PluginRole::Effect,
                audio_inputs: 2,
                audio_outputs: 2,
                midi_input: true,
                midi_output: false,
                editor: true,
                matching_installations: 1,
            }],
        }
    }
    #[test]
    fn identity_replay_expiry_and_full_snapshot_withdrawal() {
        let mut registry = Registry::default();
        assert!(registry.observe("impostor", inventory(), 100).is_err());
        let mut future = inventory();
        future.observed_at_unix_ms = 6000;
        future.valid_until_unix_ms = 7000;
        assert!(registry.observe("studio", future.clone(), 999).is_err());
        let mut skewed = Registry::default();
        skewed.observe("studio", future.clone(), 1000).unwrap();
        assert_eq!(skewed.current(1000).len(), 1);
        assert!(skewed.observe("studio", future, 1001).is_err());
        assert!(skewed.current(7000).is_empty());
        registry.observe("studio", inventory(), 100).unwrap();
        assert!(registry.observe("studio", inventory(), 101).is_err());
        let mut empty = inventory();
        empty.observed_at_unix_ms = 200;
        empty.plugins.clear();
        registry.observe("studio", empty, 200).unwrap();
        assert!(registry.current(200)[0].plugins.is_empty());
        assert!(registry.current(1100).is_empty());
    }
    #[test]
    fn invalid_metadata_duplicate_classes_and_leases_are_rejected() {
        let mut bad = inventory();
        bad.plugins.push(bad.plugins[0].clone());
        assert!(bad.validate().is_err());
        let mut bad = inventory();
        bad.plugins[0].class_id = "../path".into();
        assert!(bad.validate().is_err());
        let mut bad = inventory();
        bad.valid_until_unix_ms = 100 + MAX_TTL_MS + 1;
        assert!(bad.validate().is_err());
        let mut bad = inventory();
        bad.plugins[0].matching_installations = 2;
        assert!(!bad.plugins[0].selectable());
        bad.validate().unwrap();
        let mut json = serde_json::to_value(inventory()).unwrap();
        json["endpoint"] = "http://evil".into();
        assert!(serde_json::from_value::<Inventory>(json).is_err());
    }

    #[test]
    fn inventory_crosses_authenticated_unibus_without_control_authority() {
        use std::{
            sync::Arc,
            time::{Duration, Instant},
        };
        use unibus::{
            Message,
            client::{Client, ClientConfig},
        };
        let tokens = tempfile::tempdir().unwrap();
        let token_path = tokens.path().join("test.token");
        let token = unibus_router::auth::generate_token().unwrap();
        unibus_router::auth::issue_token(&token_path, &token).unwrap();
        let config: unibus_router::HubConfig = serde_json::from_value(serde_json::json!({
            "installation_id":"test", "listen":"127.0.0.1:0",
            "routes":[{"kind":INVENTORY_CONTRACT,"target":{"mode":"producer"}}], "nodes":[
                {"producer_id":"studio","token_file":token_path,"zones":["default"],
                 "capabilities":[DISCOVERY_CAPABILITY],"publishes":[INVENTORY_CONTRACT],"publish_producers":["console"]},
                {"producer_id":"console","token_file":token_path,"zones":["default"],"capabilities":["plugins.inventory"]}
            ]
        })).unwrap();
        let hub = Arc::new(unibus_router::Hub::new(config).unwrap());
        let endpoint = hub.start().unwrap();
        let client =
            |id: &str, capability: &str| {
                Client::connect(serde_json::from_value::<ClientConfig>(serde_json::json!({
                "installation_id":"test","producer_id":id,"hub_endpoint":endpoint.to_string(),
                "token_file":token_path,"zones":["default"],"capabilities":[capability],
                "schema_version":2,"read_timeout_ms":25
            })).unwrap()).unwrap()
            };
        let consumer = client("console", "plugins.inventory");
        let publisher = client("studio", DISCOVERY_CAPABILITY);
        let now = unibus::envelope::now_ms();
        let mut observed = inventory();
        observed.observed_at_unix_ms = now;
        observed.valid_until_unix_ms = now + 10_000;
        observed.scanned_at_unix_ms = Some(now);
        publisher
            .send(
                &publisher
                    .envelope(
                        Some("console"),
                        Message::new(INVENTORY_CONTRACT, serde_json::to_value(observed).unwrap()),
                    )
                    .unwrap(),
                &[],
            )
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        let (envelope, payload) = loop {
            assert!(Instant::now() < deadline, "inventory not delivered");
            if let Some(message) = consumer.try_read().unwrap() {
                if message.0.message.kind == INVENTORY_CONTRACT {
                    break message;
                }
            }
        };
        assert!(payload.is_empty());
        assert_eq!(envelope.message.kind, INVENTORY_CONTRACT);
        let observation =
            serde_json::from_value(serde_json::Value::Object(envelope.message.body)).unwrap();
        let mut registry = Registry::default();
        registry
            .observe(
                &envelope.producer_id,
                observation,
                unibus::envelope::now_ms(),
            )
            .unwrap();
        assert_eq!(registry.current(now)[0].plugins[0].name, "amp");
        assert!(registry.current(now + 10_000).is_empty());
        // This publisher has no grant to load plugins or change audio routing.
        publisher
            .send(
                &publisher
                    .envelope(
                        Some("console"),
                        Message::new("plugins.load.v1", serde_json::json!({})),
                    )
                    .unwrap(),
                &[],
            )
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            assert!(
                Instant::now() < deadline,
                "missing explicit grant rejection"
            );
            match publisher.try_read() {
                Ok(Some((reply, _)))
                    if reply
                        .message
                        .body
                        .get("success")
                        .and_then(serde_json::Value::as_bool)
                        == Some(false) =>
                {
                    break;
                }
                Err(error) => {
                    let message = error.to_string();
                    assert!(
                        message.contains("closed") || message.contains("reset"),
                        "unexpected denial error: {error:#}"
                    );
                    break;
                }
                _ => {}
            }
        }
    }
}
