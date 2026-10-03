//! Provider-neutral semantic stream descriptions and ephemeral endpoint announcements.
//!
//! These documents do not prescribe discovery or data transport. UniBus, HTTP,
//! static configuration, Mycelium, or a local probe may distribute them; media
//! continues over the endpoint's native transport.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

pub const STREAM_DESCRIPTOR_KIND: &str = "stream.descriptor.v1";
pub const ENDPOINT_ANNOUNCEMENT_KIND: &str = "stream.endpoints.v1";
pub const SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Freshness {
    Live,
    Snapshot,
    Recorded,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MediaCharacteristics {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub video: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sample_format: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub framerate: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sample_rate: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channels: Option<u16>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StreamDescriptorV1 {
    pub schema_version: u32,
    pub stream_id: String,
    pub semantic_type: String,
    pub source_resource: String,
    #[serde(default)]
    pub content: BTreeMap<String, serde_json::Value>,
    #[serde(default)]
    pub media: MediaCharacteristics,
    pub freshness: Freshness,
    pub provenance: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TransportEndpoint {
    pub transport: String,
    /// Logical, provider-owned reference resolved by an authorized adapter.
    /// Credentials and raw pipeline commands never belong in this document.
    pub uri_reference: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EndpointAnnouncementV1 {
    pub schema_version: u32,
    pub stream_id: String,
    pub endpoints: Vec<TransportEndpoint>,
    pub observed_at_unix_ms: u64,
    pub valid_until_unix_ms: u64,
    pub provenance: String,
}

impl StreamDescriptorV1 {
    pub fn validate(&self) -> Result<(), String> {
        version(self.schema_version)?;
        token("stream_id", &self.stream_id, 128, true)?;
        token("semantic_type", &self.semantic_type, 128, true)?;
        token("source_resource", &self.source_resource, 128, true)?;
        token("provenance", &self.provenance, 96, false)?;
        if self.content.len() > 32
            || self
                .content
                .keys()
                .any(|key| token("content key", key, 64, false).is_err())
        {
            return Err("stream content metadata is invalid".into());
        }
        if self.media.width == Some(0)
            || self.media.height == Some(0)
            || self
                .media
                .framerate
                .is_some_and(|value| !value.is_finite() || value <= 0.0)
            || self.media.sample_rate == Some(0)
            || self.media.channels == Some(0)
        {
            return Err("stream media characteristics are invalid".into());
        }
        for value in [
            self.media.video.as_deref(),
            self.media.audio.as_deref(),
            self.media.sample_format.as_deref(),
        ]
        .into_iter()
        .flatten()
        {
            token("media characteristic", value, 64, false)?;
        }
        Ok(())
    }
}

impl EndpointAnnouncementV1 {
    pub fn validate(&self, now_unix_ms: u64) -> Result<(), String> {
        version(self.schema_version)?;
        token("stream_id", &self.stream_id, 128, true)?;
        token("provenance", &self.provenance, 96, false)?;
        if self.endpoints.is_empty() || self.endpoints.len() > 16 {
            return Err("endpoint announcement must contain 1..=16 endpoints".into());
        }
        for endpoint in &self.endpoints {
            token("transport", &endpoint.transport, 64, false)?;
            token("uri_reference", &endpoint.uri_reference, 256, true)?;
        }
        if self.observed_at_unix_ms == 0
            || self.valid_until_unix_ms <= self.observed_at_unix_ms
            || self.valid_until_unix_ms - self.observed_at_unix_ms > 24 * 60 * 60 * 1_000
            || self.valid_until_unix_ms <= now_unix_ms
        {
            return Err("endpoint freshness window is invalid or expired".into());
        }
        Ok(())
    }
}

fn version(value: u32) -> Result<(), String> {
    (value == SCHEMA_VERSION)
        .then_some(())
        .ok_or_else(|| "unsupported stream document schema".into())
}

fn token(name: &str, value: &str, max: usize, slash: bool) -> Result<(), String> {
    let valid = !value.is_empty()
        && value.len() <= max
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || b"._-".contains(&byte) || (slash && byte == b'/')
        });
    valid
        .then_some(())
        .ok_or_else(|| format!("{name} is invalid"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptor_contains_semantics_but_no_transport() {
        let descriptor = StreamDescriptorV1 {
            schema_version: 1,
            stream_id: "camera/front-door/live".into(),
            semantic_type: "video.camera".into(),
            source_resource: "camera/front-door".into(),
            content: BTreeMap::from([("role".into(), serde_json::json!("security"))]),
            media: MediaCharacteristics {
                video: Some("h264".into()),
                width: Some(1920),
                height: Some(1080),
                framerate: Some(15.0),
                ..Default::default()
            },
            freshness: Freshness::Live,
            provenance: "frigate".into(),
        };
        descriptor.validate().unwrap();
        let value = serde_json::to_value(descriptor).unwrap();
        assert!(value.get("endpoints").is_none());
    }

    #[test]
    fn endpoints_expire_independently() {
        let announcement = EndpointAnnouncementV1 {
            schema_version: 1,
            stream_id: "camera/front-door/live".into(),
            endpoints: vec![TransportEndpoint {
                transport: "rtsp".into(),
                uri_reference: "frigate/front-door".into(),
            }],
            observed_at_unix_ms: 100,
            valid_until_unix_ms: 200,
            provenance: "frigate".into(),
        };
        announcement.validate(150).unwrap();
        assert!(announcement.validate(200).is_err());
    }
}
