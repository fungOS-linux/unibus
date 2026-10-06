//! HDHomeRun owns tuning; this adapter owns compatible LAN projection.
//! Media uses bounded, reliable mTLS QUIC streams, never discovery broadcasts.

pub mod discovery;
pub mod runtime;

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

pub const CONTRACT: &str = "unibus.hdhomerun.request.v2";
pub const LEGACY_CONTRACT: &str = "unibus.hdhomerun.request.v1";
pub const MAX_METADATA: usize = 1024 * 1024;
pub const MAX_HEADER: usize = 4096;
pub const IO_DEADLINE_MS: u64 = 10_000;

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
    /// Physical allocation reported by the tuner, never a fabricated proxy ID.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource: Option<String>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Catalog {
    pub device: Map<String, Value>,
    pub channels: Vec<Map<String, Value>>,
}

impl Catalog {
    /// Non-secret inventory derived from the current physical tuner catalog.
    /// Owner and presenter references are explicit, transport-neutral identities.
    pub fn service_catalog(
        &self,
        owner_reference: &str,
        presenter_reference: &str,
        observed_at_unix_ms: u64,
        valid_until_unix_ms: u64,
    ) -> Result<stream_descriptors::service::ServiceCatalog> {
        use stream_descriptors::service::{
            SERVICE_CATALOG_KIND, ServiceCatalog, ServiceOperation, ServiceResource,
        };
        let (device, _) = self.identity()?;
        let service_id = format!("hdhomerun.{device:08X}");
        let mut resources = vec![ServiceResource {
            id: service_id.clone(),
            kind: "tv.tuner".into(),
            display_name: format!("HDHomeRun {device:08X}"),
            stream_ids: vec![],
            operations: vec![ServiceOperation {
                id: "lineup_status".into(),
                request_kind: CONTRACT.into(),
                requires: "hdhomerun.catalog.read".into(),
            }],
        }];
        for channel in self
            .channels
            .iter()
            .filter(|channel| !is_protected(channel))
        {
            let number = channel["GuideNumber"].as_str().context("missing channel")?;
            let resource_id = format!("{service_id}.channel.{number}");
            resources.push(ServiceResource {
                id: resource_id.clone(),
                kind: "tv.channel".into(),
                display_name: channel["GuideName"]
                    .as_str()
                    .context("missing name")?
                    .into(),
                stream_ids: vec![format!("{resource_id}.mpegts")],
                operations: vec![ServiceOperation {
                    id: "stream".into(),
                    request_kind: CONTRACT.into(),
                    requires: "hdhomerun.stream.consume".into(),
                }],
            });
        }
        let catalog = ServiceCatalog {
            contract: SERVICE_CATALOG_KIND.into(),
            service_id,
            adapter_kind: "hdhomerun".into(),
            owner_reference: owner_reference.into(),
            presenter_reference: presenter_reference.into(),
            observed_at_unix_ms,
            valid_until_unix_ms,
            resources,
        };
        catalog.validate().map_err(anyhow::Error::msg)?;
        Ok(catalog)
    }

    /// Existing v2 contracts, not a new media protocol. Endpoint references are
    /// resolved by the HDHomeRun adapter to its scoped native HTTP stream.
    pub fn stream_descriptors(
        &self,
        observed_at_unix_ms: u64,
        valid_until_unix_ms: u64,
    ) -> Result<
        Vec<(
            stream_descriptors::StreamDescriptorV2,
            stream_descriptors::EndpointAnnouncementV2,
        )>,
    > {
        use stream_descriptors::*;
        let (device, _) = self.identity()?;
        let mut streams = Vec::new();
        for channel in self
            .channels
            .iter()
            .filter(|channel| !is_protected(channel))
        {
            let number = channel["GuideNumber"].as_str().context("missing channel")?;
            let resource = format!("hdhomerun.{device:08X}.channel.{number}");
            let stream_id = format!("{resource}.mpegts");
            let provenance = ProvenanceFacet {
                producer: "unibus-hdhomerun".into(),
                calibration: None,
                algorithm: None,
                transform: None,
            };
            let descriptor = StreamDescriptorV2 {
                schema_version: V2_SCHEMA_VERSION,
                stream_id: stream_id.clone(),
                source_resource: resource.clone(),
                semantic: SemanticFacet {
                    kind: "tv.broadcast".into(),
                    dimensions: Default::default(),
                },
                payload: Some(PayloadContract {
                    contract: "mpeg.transport-stream".into(),
                    maximum_bytes: None,
                }),
                observation: None,
                encoding: Some(EncodingFacet {
                    kind: "mpeg-ts".into(),
                    parameters: Default::default(),
                }),
                clock: None,
                media: None,
                delivery: DeliveryFacet {
                    class: DeliveryClass::Stream,
                    maximum_age_ms: Some(IO_DEADLINE_MS),
                    coalescing: None,
                },
                required_consumer_capabilities: vec!["hdhomerun.stream.consume".into()],
                provenance: provenance.clone(),
            };
            let endpoints = EndpointAnnouncementV2 {
                schema_version: V2_SCHEMA_VERSION,
                stream_id,
                generation: None,
                bindings: vec![EndpointBindingV2 {
                    transport: "http".into(),
                    profile: "hdhomerun-mpegts".into(),
                    role: ConnectionRole::Listener,
                    endpoint_reference: resource,
                    requires: "hdhomerun.stream.consume".into(),
                }],
                observed_at_unix_ms,
                valid_until_unix_ms,
                provenance,
            };
            descriptor.validate().map_err(anyhow::Error::msg)?;
            endpoints
                .validate(observed_at_unix_ms)
                .map_err(anyhow::Error::msg)?;
            streams.push((descriptor, endpoints));
        }
        Ok(streams)
    }

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
        || channel.get("Tags").is_some_and(|tags| {
            tags.as_array()
                .is_some_and(|tags| tags.iter().any(|tag| tag.as_str() == Some("drm")))
                || tags
                    .as_str()
                    .is_some_and(|tags| tags.split(',').any(|tag| tag.trim() == "drm"))
        })
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

    #[test]
    fn service_inventory_reuses_v2_streams_without_secrets_or_protected_channels() {
        let catalog = fixture();
        let inventory = catalog
            .service_catalog("tls/owner.internal", "lan/pi/8088", 100, 200)
            .unwrap();
        assert_eq!(inventory.resources.len(), 2);
        assert!(inventory.is_current(150));
        assert!(!inventory.is_current(200));
        let json = serde_json::to_string(&inventory).unwrap();
        assert!(!json.contains("DeviceAuth"));
        assert!(!json.contains("private"));
        let streams = catalog.stream_descriptors(100, 200).unwrap();
        assert_eq!(streams.len(), 1);
        assert_eq!(inventory.resources[1].stream_ids[0], streams[0].0.stream_id);
        assert_eq!(streams[0].0.source_resource, inventory.resources[1].id);
        let mut protected = catalog.clone();
        protected.channels[0].insert("Tags".into(), "hd, drm".into());
        assert!(is_protected(&protected.channels[0]));
        assert!(protected.stream_descriptors(100, 200).unwrap().is_empty());
        assert_eq!(
            protected
                .service_catalog("owner", "proxy", 100, 200)
                .unwrap()
                .resources
                .len(),
            1
        );
    }
}
