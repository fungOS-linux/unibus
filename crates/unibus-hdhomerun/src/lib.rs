//! HDHomeRun owns tuning; this adapter owns compatible LAN projection.
//! Media uses bounded, reliable mTLS QUIC streams, never discovery broadcasts.

pub mod discovery;
pub mod runtime;

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

pub const CONTRACT: &str = "unibus.hdhomerun.request.v1";
pub const MAX_METADATA: usize = 1024 * 1024;
pub const MAX_HEADER: usize = 4096;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum Operation {
    Catalog,
    LineupStatus,
    Stream {
        channel: String,
        tuner: Option<u8>,
        duration: Option<u32>,
    },
}

impl Operation {
    pub fn stream_path(&self, tuners: u8) -> Result<String> {
        let Self::Stream {
            channel,
            tuner,
            duration,
        } = self
        else {
            anyhow::bail!("not a stream request");
        };
        ensure!(valid_channel(channel), "invalid channel");
        ensure!(tuner.is_none_or(|n| n < tuners), "unknown tuner");
        ensure!(
            duration.is_none_or(|n| (1..=86400).contains(&n)),
            "invalid duration"
        );
        let selector = tuner.map_or_else(|| "auto".into(), |n| format!("tuner{n}"));
        let mut path = format!("/{selector}/v{channel}");
        if let Some(seconds) = duration {
            path.push_str(&format!("?duration={seconds}"));
        }
        Ok(path)
    }
}

pub fn valid_channel(value: &str) -> bool {
    let parts: Vec<_> = value.split('.').collect();
    (1..=2).contains(&parts.len())
        && parts.iter().all(|part| {
            !part.is_empty() && part.len() <= 5 && part.bytes().all(|b| b.is_ascii_digit())
        })
}

/// No arbitrary URL, HTTP method, or device control crosses this boundary.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub contract: String,
    pub operation: Operation,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Reply {
    pub status: u16,
    pub content_type: String,
    pub tuner_error: Option<String>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Catalog {
    pub device: Map<String, Value>,
    pub channels: Vec<Map<String, Value>>,
}

impl Catalog {
    pub fn identity(&self) -> Result<(u32, u8)> {
        let id = self
            .device
            .get("DeviceID")
            .and_then(Value::as_str)
            .context("missing DeviceID")?;
        ensure!(
            id.len() == 8 && id.bytes().all(|b| b.is_ascii_hexdigit()),
            "invalid DeviceID"
        );
        let id = u32::from_str_radix(id, 16)?;
        let count = self
            .device
            .get("TunerCount")
            .and_then(Value::as_u64)
            .context("missing TunerCount")?;
        ensure!((1..=16).contains(&count), "invalid tuner count");
        ensure!(self.channels.len() <= 1024, "too many channels");
        let mut numbers = std::collections::BTreeSet::new();
        for channel in &self.channels {
            let number = channel
                .get("GuideNumber")
                .and_then(Value::as_str)
                .context("missing GuideNumber")?;
            ensure!(
                valid_channel(number) && numbers.insert(number),
                "invalid/duplicate channel number"
            );
            let name = channel
                .get("GuideName")
                .and_then(Value::as_str)
                .context("missing GuideName")?;
            ensure!(
                name.len() <= 256 && !name.chars().any(char::is_control),
                "invalid GuideName"
            );
        }
        Ok((id, count as u8))
    }

    pub fn project(&self, base: &str, stream_base: &str, share_device_auth: bool) -> Result<Self> {
        self.identity()?;
        let mut device = self.device.clone();
        // Do not propagate upstream addresses, capabilities, or unknown extensions.
        device.retain(|key, _| {
            matches!(
                key.as_str(),
                "FriendlyName"
                    | "ModelNumber"
                    | "FirmwareName"
                    | "FirmwareVersion"
                    | "DeviceID"
                    | "TunerCount"
            ) || (share_device_auth && key == "DeviceAuth")
        });
        device.insert("BaseURL".into(), base.into());
        device.insert("LineupURL".into(), format!("{base}/lineup.json").into());
        let channels = self
            .channels
            .iter()
            .filter(|c| !is_protected(c))
            .map(|channel| {
                let mut channel = channel.clone();
                channel.retain(|key, _| {
                    matches!(
                        key.as_str(),
                        "GuideNumber"
                            | "GuideName"
                            | "HD"
                            | "Favorite"
                            | "Tags"
                            | "VideoCodec"
                            | "AudioCodec"
                            | "SignalStrength"
                            | "SignalQuality"
                    )
                });
                let number = channel["GuideNumber"].as_str().expect("validated number");
                channel.insert("URL".into(), format!("{stream_base}/auto/v{number}").into());
                channel
            })
            .collect();
        Ok(Self { device, channels })
    }
}

pub fn is_protected(channel: &Map<String, Value>) -> bool {
    channel
        .get("DRM")
        .is_some_and(|v| v.as_u64().is_some_and(|n| n != 0) || v.as_bool() == Some(true))
        || channel
            .get("Tags")
            .and_then(Value::as_array)
            .is_some_and(|tags| tags.iter().any(|tag| tag.as_str() == Some("drm")))
}

pub fn playlist(catalog: &Catalog) -> String {
    let mut output = String::from("#EXTM3U\n");
    for channel in &catalog.channels {
        output.push_str(&format!(
            "#EXTINF:-1 tvg-chno=\"{}\",{}\n{}\n",
            channel["GuideNumber"].as_str().unwrap_or_default(),
            channel["GuideName"].as_str().unwrap_or_default(),
            channel["URL"].as_str().unwrap_or_default()
        ));
    }
    output
}

pub fn lineup_xml(catalog: &Catalog) -> String {
    fn escape(s: &str) -> String {
        s.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;")
            .replace('\'', "&apos;")
    }
    let mut output = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?><Lineup>");
    for channel in &catalog.channels {
        output.push_str("<Program>");
        for key in ["GuideNumber", "GuideName", "URL"] {
            output.push_str(&format!(
                "<{key}>{}</{key}>",
                escape(channel[key].as_str().unwrap_or_default())
            ));
        }
        output.push_str("</Program>");
    }
    output.push_str("</Lineup>");
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    pub fn fixture() -> Catalog {
        serde_json::from_value(json!({"device":{"DeviceID":"12345678","TunerCount":2,"DeviceAuth":"private",
            "BaseURL":"http://192.168.0.208","StorageURL":"http://upstream"},
            "channels":[{"GuideNumber":"5.1","GuideName":"A&B","URL":"http://192.168.0.208:5004/auto/v5.1"},
                {"GuideNumber":"9.1","GuideName":"Protected","DRM":1,"URL":"http://upstream"}]})).unwrap()
    }

    #[test]
    fn projection_rewrites_every_address_and_hides_drm_and_auth_by_default() {
        let projected = fixture()
            .project(
                "http://192.168.2.74:8088",
                "http://192.168.2.74:5004",
                false,
            )
            .unwrap();
        assert_eq!(projected.channels.len(), 1);
        let encoded = serde_json::to_string(&projected).unwrap();
        assert!(
            !encoded.contains("192.168.0.208")
                && !encoded.contains("upstream")
                && !encoded.contains("private")
        );
        assert_eq!(projected.device["DeviceID"], "12345678");
        assert!(playlist(&projected).contains("http://192.168.2.74:5004/auto/v5.1"));
        assert!(lineup_xml(&projected).contains("A&amp;B"));
        assert!(
            fixture()
                .project("http://local", "http://local", true)
                .unwrap()
                .device
                .contains_key("DeviceAuth")
        );
    }

    #[test]
    fn stream_contract_cannot_inject_a_path_query_or_arbitrary_target() {
        for bad in [
            "",
            "../admin",
            "5.1?x=1",
            "5/1",
            "http://evil",
            "5.1.2",
            "5%2f1",
            "５",
            "999999",
        ] {
            assert!(!valid_channel(bad), "{bad}");
        }
        let op = Operation::Stream {
            channel: "5.1".into(),
            tuner: Some(1),
            duration: Some(3),
        };
        assert_eq!(op.stream_path(2).unwrap(), "/tuner1/v5.1?duration=3");
        assert!(op.stream_path(1).is_err());
        assert!(serde_json::from_value::<Operation>(json!({"operation":"stream","channel":"5.1","tuner":null,"duration":null,"url":"http://evil"})).is_err());
    }

    #[test]
    fn malformed_catalog_fails_before_projection() {
        let mut catalog = fixture();
        catalog.channels.push(catalog.channels[0].clone());
        assert!(catalog.identity().is_err());
        catalog.channels.pop();
        catalog.device.insert("TunerCount".into(), 0.into());
        assert!(catalog.identity().is_err());
    }
}
