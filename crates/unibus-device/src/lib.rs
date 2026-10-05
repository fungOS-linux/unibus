//! Shared hotplug identity and binding vocabulary.
//!
//! OS-specific USB, HID, MIDI, and serial adapters publish these facts. The
//! contract does not open devices or make discovery an authorization grant.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

pub mod printer;

pub const DEVICE_SCHEMA_VERSION: u32 = 1;
pub const DEVICE_PRESENCE_KIND: &str = "device.presence.v1";
pub const DEVICE_BINDING_KIND: &str = "device.binding.v1";
pub const MAX_PRESENCE_LEASE_MS: u64 = 120_000;
pub const MAX_BINDING_LEASE_MS: u64 = 300_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PresenceState {
    Present,
    Removed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IdentityStrength {
    /// Location-derived identity; moving ports may change the ID.
    Topology,
    /// Stable hash of bounded immutable descriptors without a serial number.
    Derived,
    /// Manufacturer-provided stable serial identity.
    Serial,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeviceBus {
    Usb,
    Bluetooth,
    Virtual,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeviceInterface {
    pub id: String,
    /// `hid`, `midi`, `serial`, or another bounded protocol identifier.
    pub protocol: String,
    #[serde(default)]
    pub capabilities: BTreeSet<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DevicePresence {
    pub schema_version: u32,
    pub node_id: String,
    pub device_id: String,
    pub generation: u64,
    pub state: PresenceState,
    pub bus: DeviceBus,
    pub identity_strength: IdentityStrength,
    #[serde(default)]
    pub vendor_id: Option<u16>,
    #[serde(default)]
    pub product_id: Option<u16>,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub interfaces: Vec<DeviceInterface>,
    #[serde(default)]
    pub attributes: BTreeMap<String, String>,
    pub observed_at_unix_ms: u64,
    pub valid_until_unix_ms: u64,
}

impl DevicePresence {
    pub fn validate(&self) -> Result<(), String> {
        if self.schema_version != DEVICE_SCHEMA_VERSION
            || !id(&self.node_id)
            || !id(&self.device_id)
            || self.generation == 0
            || self.observed_at_unix_ms == 0
            || self.valid_until_unix_ms <= self.observed_at_unix_ms
            || self.valid_until_unix_ms - self.observed_at_unix_ms > MAX_PRESENCE_LEASE_MS
            || self.interfaces.len() > 32
            || self.attributes.len() > 32
            || self
                .display_name
                .as_ref()
                .is_some_and(|name| name.len() > 256)
        {
            return Err("invalid device presence".into());
        }
        let mut interfaces = BTreeSet::new();
        for interface in &self.interfaces {
            if !id(&interface.id)
                || !id(&interface.protocol)
                || !interfaces.insert(&interface.id)
                || interface.capabilities.len() > 64
                || interface.capabilities.iter().any(|value| !id(value))
            {
                return Err("invalid device interface".into());
            }
        }
        if self
            .attributes
            .iter()
            .any(|(key, value)| !id(key) || value.len() > 256)
        {
            return Err("invalid device attributes".into());
        }
        if self.state == PresenceState::Removed && !self.interfaces.is_empty() {
            return Err("removed device must not advertise live interfaces".into());
        }
        Ok(())
    }

    pub fn live_at(&self, now_unix_ms: u64) -> bool {
        self.state == PresenceState::Present
            && self.observed_at_unix_ms <= now_unix_ms
            && now_unix_ms < self.valid_until_unix_ms
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeviceSelector {
    pub protocol: String,
    #[serde(default)]
    pub capabilities: BTreeSet<String>,
    #[serde(default)]
    pub device_id: Option<String>,
    #[serde(default)]
    pub node_id: Option<String>,
    #[serde(default)]
    pub minimum_identity_strength: Option<IdentityStrength>,
}

impl DeviceSelector {
    pub fn validate(&self) -> Result<(), String> {
        if !id(&self.protocol)
            || self.capabilities.len() > 64
            || self.capabilities.iter().any(|value| !id(value))
            || self.device_id.as_ref().is_some_and(|value| !id(value))
            || self.node_id.as_ref().is_some_and(|value| !id(value))
        {
            return Err("invalid device selector".into());
        }
        Ok(())
    }

    pub fn matching_interface<'a>(
        &self,
        presence: &'a DevicePresence,
        now_unix_ms: u64,
    ) -> Option<&'a DeviceInterface> {
        if !presence.live_at(now_unix_ms)
            || self
                .device_id
                .as_ref()
                .is_some_and(|id| id != &presence.device_id)
            || self
                .node_id
                .as_ref()
                .is_some_and(|id| id != &presence.node_id)
            || self
                .minimum_identity_strength
                .is_some_and(|minimum| presence.identity_strength < minimum)
        {
            return None;
        }
        presence.interfaces.iter().find(|interface| {
            interface.protocol == self.protocol
                && self.capabilities.is_subset(&interface.capabilities)
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeviceBindingLease {
    pub schema_version: u32,
    pub binding_id: String,
    pub consumer_id: String,
    pub selector: DeviceSelector,
    pub issued_at_unix_ms: u64,
    pub valid_until_unix_ms: u64,
}

impl DeviceBindingLease {
    pub fn validate(&self) -> Result<(), String> {
        self.selector.validate()?;
        if self.schema_version != DEVICE_SCHEMA_VERSION
            || !id(&self.binding_id)
            || !id(&self.consumer_id)
            || self.issued_at_unix_ms == 0
            || self.valid_until_unix_ms <= self.issued_at_unix_ms
            || self.valid_until_unix_ms - self.issued_at_unix_ms > MAX_BINDING_LEASE_MS
        {
            return Err("invalid device binding lease".into());
        }
        Ok(())
    }
}

/// Select one live device deterministically. Stronger identities win, followed
/// by stable node/device/interface ordering.
pub fn resolve_binding<'a>(
    selector: &DeviceSelector,
    devices: &'a [DevicePresence],
    now_unix_ms: u64,
) -> Option<(&'a DevicePresence, &'a DeviceInterface)> {
    let mut candidates = devices
        .iter()
        .filter_map(|device| {
            selector
                .matching_interface(device, now_unix_ms)
                .map(|interface| (device, interface))
        })
        .collect::<Vec<_>>();
    candidates.sort_by(|(left, left_interface), (right, right_interface)| {
        right
            .identity_strength
            .cmp(&left.identity_strength)
            .then_with(|| left.node_id.cmp(&right.node_id))
            .then_with(|| left.device_id.cmp(&right.device_id))
            .then_with(|| left_interface.id.cmp(&right_interface.id))
    });
    candidates.into_iter().next()
}

fn id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_' | b':' | b'/')
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device(node: &str, strength: IdentityStrength, protocol: &str) -> DevicePresence {
        DevicePresence {
            schema_version: 1,
            node_id: node.into(),
            device_id: "usb:046d:c215:stick".into(),
            generation: 1,
            state: PresenceState::Present,
            bus: DeviceBus::Usb,
            identity_strength: strength,
            vendor_id: Some(0x046d),
            product_id: Some(0xc215),
            display_name: Some("Flight controls".into()),
            interfaces: vec![DeviceInterface {
                id: "interface:0".into(),
                protocol: protocol.into(),
                capabilities: BTreeSet::from(["axis".into(), "button".into()]),
            }],
            attributes: BTreeMap::new(),
            observed_at_unix_ms: 100,
            valid_until_unix_ms: 1_100,
        }
    }

    #[test]
    fn binding_follows_a_serial_device_between_nodes() {
        let selector = DeviceSelector {
            protocol: "hid".into(),
            capabilities: BTreeSet::from(["axis".into()]),
            device_id: Some("usb:046d:c215:stick".into()),
            node_id: None,
            minimum_identity_strength: Some(IdentityStrength::Serial),
        };
        let old = device("neo", IdentityStrength::Serial, "hid");
        let mut moved = device("studio", IdentityStrength::Serial, "hid");
        moved.generation = 2;
        moved.observed_at_unix_ms = 200;
        moved.valid_until_unix_ms = 1_200;
        let mut removed = old.clone();
        removed.state = PresenceState::Removed;
        removed.interfaces.clear();
        assert_eq!(
            resolve_binding(&selector, &[removed, moved], 300)
                .unwrap()
                .0
                .node_id,
            "studio"
        );
    }

    #[test]
    fn midi_and_hid_claim_only_their_own_interfaces() {
        let device = device("neo", IdentityStrength::Derived, "hid");
        let midi = DeviceSelector {
            protocol: "midi".into(),
            capabilities: BTreeSet::new(),
            device_id: None,
            node_id: None,
            minimum_identity_strength: None,
        };
        assert!(resolve_binding(&midi, &[device], 200).is_none());
    }

    #[test]
    fn expired_presence_cannot_remain_bound() {
        let selector = DeviceSelector {
            protocol: "hid".into(),
            capabilities: BTreeSet::new(),
            device_id: None,
            node_id: None,
            minimum_identity_strength: None,
        };
        assert!(
            resolve_binding(
                &selector,
                &[device("neo", IdentityStrength::Derived, "hid")],
                1_100
            )
            .is_none()
        );
    }
}
