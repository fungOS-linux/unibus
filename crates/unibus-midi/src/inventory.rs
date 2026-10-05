//! Expiring OS port observations. Discovery conveys availability, not routing authorization.
use crate::{MIDI_SCHEMA_VERSION, valid_identifier};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

pub const MIDI_PORTS_CONTRACT: &str = "midi.ports.v1";
pub const MIDI_DISCOVERY_CAPABILITY: &str = "midi.discovery";
pub const MAX_PORTS: usize = 512;
pub const MAX_INVENTORY_TTL_MS: u64 = 30_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PortDirection {
    Input,
    Output,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MidiPort {
    pub id: String,
    pub name: String,
    pub direction: PortDirection,
    /// Duplicate OS names cannot be selected unambiguously by the bridge.
    pub matching_ports: u16,
}

impl MidiPort {
    pub fn selectable(&self) -> bool {
        self.matching_ports == 1
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MidiPortInventory {
    pub schema_version: u32,
    pub producer_id: String,
    pub observed_at_unix_ms: u64,
    pub valid_until_unix_ms: u64,
    pub ports: Vec<MidiPort>,
}

fn port_id(direction: PortDirection, name: &str) -> String {
    let digest = Sha256::digest(name.as_bytes());
    let hash = digest
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!(
        "midi.{}.{}",
        if direction == PortDirection::Input {
            "in"
        } else {
            "out"
        },
        hash
    )
}

impl MidiPortInventory {
    pub fn from_names(
        producer_id: String,
        observed_at_unix_ms: u64,
        ttl_ms: u64,
        names: impl IntoIterator<Item = (PortDirection, String)>,
    ) -> Result<Self> {
        let mut grouped = BTreeMap::<(PortDirection, String), u16>::new();
        let mut count = 0;
        for key in names {
            count += 1;
            ensure!(
                count <= MAX_PORTS,
                "MIDI inventory exceeds {MAX_PORTS} ports"
            );
            *grouped.entry(key).or_default() += 1;
        }
        let inventory = Self {
            schema_version: MIDI_SCHEMA_VERSION,
            producer_id,
            observed_at_unix_ms,
            valid_until_unix_ms: observed_at_unix_ms
                .checked_add(ttl_ms)
                .ok_or_else(|| anyhow::anyhow!("inventory expiry overflow"))?,
            ports: grouped
                .into_iter()
                .map(|((direction, name), matching_ports)| MidiPort {
                    id: port_id(direction, &name),
                    name,
                    direction,
                    matching_ports,
                })
                .collect(),
        };
        inventory.validate()?;
        Ok(inventory)
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.schema_version == MIDI_SCHEMA_VERSION && valid_identifier(&self.producer_id),
            "invalid MIDI inventory identity"
        );
        ensure!(
            self.valid_until_unix_ms > self.observed_at_unix_ms
                && self.valid_until_unix_ms - self.observed_at_unix_ms <= MAX_INVENTORY_TTL_MS,
            "invalid inventory lifetime"
        );
        ensure!(self.ports.len() <= MAX_PORTS, "too many MIDI ports");
        let mut ids = BTreeSet::new();
        let mut count = 0_usize;
        for port in &self.ports {
            ensure!(
                !port.name.trim().is_empty()
                    && port.name.len() <= 512
                    && !port.name.chars().any(char::is_control),
                "invalid OS MIDI port name"
            );
            ensure!(
                port.matching_ports > 0
                    && port.id == port_id(port.direction, &port.name)
                    && ids.insert(&port.id),
                "invalid or duplicate MIDI port identity"
            );
            count += usize::from(port.matching_ports);
        }
        ensure!(count <= MAX_PORTS, "too many matching MIDI ports");
        Ok(())
    }

    pub fn is_fresh(&self, now_ms: u64) -> bool {
        self.observed_at_unix_ms <= now_ms && now_ms < self.valid_until_unix_ms
    }
}

/// One latest full snapshot per authenticated publisher. Empty snapshots
/// withdraw unplugged ports; absent publishers disappear when their lease expires.
#[derive(Default)]
pub struct MidiInventoryRegistry {
    inventories: BTreeMap<String, MidiPortInventory>,
}

impl MidiInventoryRegistry {
    pub fn observe(
        &mut self,
        authenticated_producer: &str,
        inventory: MidiPortInventory,
        now_ms: u64,
    ) -> Result<()> {
        inventory.validate()?;
        ensure!(
            inventory.producer_id == authenticated_producer,
            "MIDI inventory publisher mismatch"
        );
        ensure!(inventory.is_fresh(now_ms), "stale or future MIDI inventory");
        if let Some(previous) = self.inventories.get(authenticated_producer) {
            ensure!(
                inventory.observed_at_unix_ms > previous.observed_at_unix_ms,
                "replayed MIDI inventory"
            );
        }
        self.inventories
            .insert(authenticated_producer.into(), inventory);
        Ok(())
    }

    pub fn current(&mut self, now_ms: u64) -> Vec<MidiPortInventory> {
        self.inventories
            .retain(|_, inventory| inventory.is_fresh(now_ms));
        self.inventories.values().cloned().collect()
    }
}

#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
pub fn enumerate(producer_id: &str, now_ms: u64, ttl_ms: u64) -> Result<MidiPortInventory> {
    let input = midir::MidiInput::new("unibus-midi-discovery-input")?;
    let output = midir::MidiOutput::new("unibus-midi-discovery-output")?;
    let mut names = Vec::new();
    for port in input.ports() {
        names.push((PortDirection::Input, input.port_name(&port)?));
    }
    for port in output.ports() {
        names.push((PortDirection::Output, output.port_name(&port)?));
    }
    MidiPortInventory::from_names(producer_id.into(), now_ms, ttl_ms, names)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn identities_survive_enumeration_order_and_duplicates_stay_ambiguous() {
        let names = vec![
            (PortDirection::Input, "Launchkey".into()),
            (PortDirection::Output, "TR-6S".into()),
        ];
        let mut reversed = names.clone();
        reversed.reverse();
        assert_eq!(
            MidiPortInventory::from_names("pi".into(), 100, 1000, names.clone()).unwrap(),
            MidiPortInventory::from_names("pi".into(), 100, 1000, reversed).unwrap()
        );
        let duplicates = MidiPortInventory::from_names(
            "pi".into(),
            100,
            1000,
            vec![names[0].clone(), names[0].clone()],
        )
        .unwrap();
        assert!(!duplicates.ports[0].selectable());
    }
    #[test]
    fn expiry_unplug_replay_and_identity_are_enforced() {
        let mut registry = MidiInventoryRegistry::default();
        let inventory = MidiPortInventory::from_names(
            "pi".into(),
            100,
            1000,
            [(PortDirection::Output, "TR-6S".into())],
        )
        .unwrap();
        assert!(
            registry
                .observe("impostor", inventory.clone(), 100)
                .is_err()
        );
        registry.observe("pi", inventory.clone(), 100).unwrap();
        assert!(registry.observe("pi", inventory, 101).is_err());
        registry
            .observe(
                "pi",
                MidiPortInventory::from_names("pi".into(), 200, 1000, []).unwrap(),
                200,
            )
            .unwrap();
        assert!(registry.current(200)[0].ports.is_empty());
        assert!(registry.current(1200).is_empty());
    }
}
