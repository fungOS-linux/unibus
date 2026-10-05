//! ADB smart-socket inventory; no USB ownership or app-internal access.
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    io::{Read, Write},
    net::{SocketAddr, TcpStream},
    time::Duration,
};
pub const INVENTORY_KIND: &str = "adb.devices.v1";
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InventoryState {
    Available,
    Unavailable,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Inventory {
    pub schema_version: u32,
    pub producer_id: String,
    pub generation: u64,
    pub observed_at_unix_ms: u64,
    pub valid_until_unix_ms: u64,
    pub state: InventoryState,
    pub devices: Vec<Device>,
}
impl Inventory {
    pub fn validate(&self, producer: &str, now: u64) -> Result<()> {
        ensure!(
            self.schema_version == 1
                && self.producer_id == producer
                && self.generation > 0
                && self.observed_at_unix_ms > 0
                && self.observed_at_unix_ms <= now
                && self.valid_until_unix_ms > now
                && self
                    .valid_until_unix_ms
                    .saturating_sub(self.observed_at_unix_ms)
                    <= 6000
                && self.devices.len() <= 128,
            "invalid or expired ADB inventory"
        );
        ensure!(
            self.state != InventoryState::Unavailable || self.devices.is_empty(),
            "unavailable inventory must not claim live devices"
        );
        let mut serials = std::collections::BTreeSet::new();
        for device in &self.devices {
            ensure!(
                !device.serial.is_empty()
                    && device.serial.len() <= 256
                    && serials.insert(&device.serial)
                    && !device.state.is_empty()
                    && device.state.len() <= 32
                    && [&device.usb, &device.model, &device.product]
                        .iter()
                        .all(|v| v.as_ref().is_none_or(|s| s.len() <= 256)),
                "invalid ADB inventory device"
            );
        }
        Ok(())
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Device {
    pub serial: String,
    pub state: String,
    pub usb: Option<String>,
    pub model: Option<String>,
    pub product: Option<String>,
}
pub fn parse_devices(text: &str) -> Result<Vec<Device>> {
    ensure!(text.len() <= 65535, "ADB list too large");
    let mut result = Vec::new();
    let mut serials = std::collections::BTreeSet::new();
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        let mut fields = line.split_whitespace();
        let serial = fields.next().unwrap();
        ensure!(serials.insert(serial), "duplicate ADB serial");
        let state = fields
            .next()
            .ok_or_else(|| anyhow::anyhow!("device missing state"))?;
        ensure!(
            serial.len() <= 256 && state.len() <= 32,
            "invalid identity/state"
        );
        let mut device = Device {
            serial: serial.into(),
            state: state.into(),
            usb: None,
            model: None,
            product: None,
        };
        for field in fields {
            if let Some((key, value)) = field.split_once(':') {
                ensure!(value.len() <= 256, "descriptor too long");
                match key {
                    "usb" => device.usb = Some(value.into()),
                    "model" => device.model = Some(value.into()),
                    "product" => device.product = Some(value.into()),
                    _ => {}
                }
            }
        }
        ensure!(result.len() < 128, "too many devices");
        result.push(device);
    }
    Ok(result)
}
pub fn enumerate(endpoint: SocketAddr) -> Result<Vec<Device>> {
    ensure!(
        endpoint.ip().is_loopback(),
        "inventory backend must be loopback"
    );
    let mut stream = TcpStream::connect_timeout(&endpoint, Duration::from_secs(1))?;
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
    stream.write_all(b"000ehost:devices-l")?;
    let mut status = [0; 4];
    stream.read_exact(&mut status)?;
    ensure!(&status == b"OKAY", "inventory rejected");
    let mut length = [0; 4];
    stream.read_exact(&mut length)?;
    let length = usize::from_str_radix(std::str::from_utf8(&length)?, 16)?;
    let mut bytes = vec![0; length];
    stream.read_exact(&mut bytes)?;
    parse_devices(std::str::from_utf8(&bytes)?)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reports_usb_authorization_and_network_devices_honestly() {
        let d = parse_devices("USB123 device usb:1-2 product:phone model:S22_FLIP\nUSB456 unauthorized usb:1-3\n192.0.2.1:5555 offline\n").unwrap();
        assert_eq!(d[0].model.as_deref(), Some("S22_FLIP"));
        assert_eq!(d[1].state, "unauthorized");
        assert_eq!(d[2].usb, None);
        assert!(parse_devices("missingstate").is_err());
        assert!(parse_devices("same device\nsame offline").is_err());
    }
    #[test]
    fn inventory_is_leased_and_binds_owner_identity() {
        let inventory = Inventory {
            schema_version: 1,
            producer_id: "studio-adb".into(),
            generation: 1,
            observed_at_unix_ms: 100,
            valid_until_unix_ms: 6100,
            state: InventoryState::Available,
            devices: vec![],
        };
        inventory.validate("studio-adb", 101).unwrap();
        assert!(inventory.validate("rogue", 101).is_err());
        assert!(inventory.validate("studio-adb", 6100).is_err());
        assert!(inventory.validate("studio-adb", 99).is_err());
    }
}
