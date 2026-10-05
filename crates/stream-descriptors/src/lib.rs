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
pub const STREAM_DESCRIPTOR_V2_KIND: &str = "stream.descriptor.v2";
pub const ENDPOINT_ANNOUNCEMENT_V2_KIND: &str = "stream.endpoints.v2";
pub const V2_SCHEMA_VERSION: u32 = 2;

const MAXIMUM_AGE_MS: u64 = 24 * 60 * 60 * 1_000;

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

/// Stable semantic identity for a stream. Endpoint addresses and credentials
/// deliberately do not belong in this document.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StreamDescriptorV2 {
    pub schema_version: u32,
    pub stream_id: String,
    pub source_resource: String,
    pub semantic: SemanticFacet,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload: Option<PayloadContract>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observation: Option<ObservationFacet>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encoding: Option<EncodingFacet>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clock: Option<ClockFacet>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media: Option<MediaFacet>,
    pub delivery: DeliveryFacet,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub required_consumer_capabilities: Vec<String>,
    pub provenance: ProvenanceFacet,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SemanticFacet {
    pub kind: String,
    /// Bounded identity dimensions such as zone, car, session, or sensor.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub dimensions: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PayloadContract {
    pub contract: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub maximum_bytes: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObservationFacet {
    pub values: Vec<ObservationValue>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub dimensions: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_cadence_ms: Option<u64>,
    pub maximum_age_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObservationValue {
    pub name: String,
    pub quantity: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
    pub value: ObservationValueKind,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "shape", rename_all = "snake_case", deny_unknown_fields)]
pub enum ObservationValueKind {
    Scalar {
        metric: ScalarMetric,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        aggregations: Vec<Aggregation>,
    },
    Vector {
        maximum_elements: u32,
        metric: ScalarMetric,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        aggregations: Vec<Aggregation>,
    },
    Histogram {
        metric: HistogramMetric,
        boundaries: Vec<f64>,
    },
    State {
        states: Vec<String>,
    },
    Event {
        event_types: Vec<String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScalarMetric {
    Gauge,
    MonotonicCounter,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HistogramMetric {
    Distribution,
    CumulativeDistribution,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Aggregation {
    Last,
    Minimum,
    Maximum,
    Sum,
    Mean,
    Count,
    Rate,
    P50,
    P95,
    P99,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EncodingFacet {
    pub kind: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub parameters: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClockKind {
    Media,
    Rtp,
    WallClock,
    Monotonic,
    ProducerSequence,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClockFacet {
    pub kind: ClockKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_hz: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timestamp_bits: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub synchronization: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MediaFacet {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frame_rate_millihz: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sample_rate_hz: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channels: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frames_per_packet: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryClass {
    Command,
    Stream,
    Snapshot,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeliveryFacet {
    pub class: DeliveryClass,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub maximum_age_ms: Option<u64>,
    /// Stable replacement identity for latency-first or latest-value work.
    /// Producers cannot supply arbitrary key expressions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub coalescing: Option<CoalescingKey>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CoalescingKey {
    StreamId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProvenanceFacet {
    pub producer: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calibration: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub algorithm: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transform: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionRole {
    Listener,
    Connector,
    Connectionless,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EndpointBindingV2 {
    pub transport: String,
    pub profile: String,
    pub role: ConnectionRole,
    /// Opaque logical reference resolved locally by an authorized adapter.
    pub endpoint_reference: String,
    pub requires: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EndpointAnnouncementV2 {
    pub schema_version: u32,
    pub stream_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation: Option<String>,
    pub bindings: Vec<EndpointBindingV2>,
    pub observed_at_unix_ms: u64,
    pub valid_until_unix_ms: u64,
    pub provenance: ProvenanceFacet,
}

impl StreamDescriptorV2 {
    pub fn validate(&self) -> Result<(), String> {
        version_v2(self.schema_version)?;
        token("stream_id", &self.stream_id, 128, true)?;
        token("source_resource", &self.source_resource, 128, true)?;
        token("semantic kind", &self.semantic.kind, 128, false)?;
        bounded_map("semantic dimensions", &self.semantic.dimensions, 16, 64, 96)?;

        if self.payload.is_none() && self.observation.is_none() {
            return Err("descriptor must declare a payload or observation facet".into());
        }
        if let Some(payload) = &self.payload {
            token("payload contract", &payload.contract, 128, false)?;
            if payload
                .maximum_bytes
                .is_some_and(|size| size == 0 || size > 64 * 1024 * 1024)
            {
                return Err("payload maximum_bytes must be in 1..=67108864".into());
            }
        }
        if let Some(observation) = &self.observation {
            observation.validate()?;
            if self.delivery.class == DeliveryClass::Command {
                return Err("observation facets cannot use command delivery".into());
            }
            if self.delivery.class == DeliveryClass::Snapshot
                && observation
                    .values
                    .iter()
                    .any(|value| matches!(value.value, ObservationValueKind::Event { .. }))
            {
                return Err("event observations cannot use snapshot delivery".into());
            }
            if self
                .delivery
                .maximum_age_ms
                .is_some_and(|delivery_age| delivery_age > observation.maximum_age_ms)
            {
                return Err(
                    "delivery maximum_age_ms cannot exceed observation maximum_age_ms".into(),
                );
            }
        }
        if let Some(encoding) = &self.encoding {
            token("encoding kind", &encoding.kind, 64, false)?;
            bounded_map("encoding parameters", &encoding.parameters, 16, 64, 96)?;
        }
        if let Some(clock) = &self.clock {
            clock.validate()?;
        }
        if let Some(media) = &self.media {
            media.validate()?;
        }
        self.delivery.validate()?;

        bounded_unique_tokens(
            "required consumer capabilities",
            &self.required_consumer_capabilities,
            32,
            128,
            false,
        )?;
        self.provenance.validate()?;
        Ok(())
    }
}

impl ObservationFacet {
    fn validate(&self) -> Result<(), String> {
        if self.values.is_empty() || self.values.len() > 32 {
            return Err("observation must contain 1..=32 values".into());
        }
        bounded_map("observation dimensions", &self.dimensions, 16, 64, 96)?;
        duration("observation maximum_age_ms", self.maximum_age_ms)?;
        if let Some(cadence) = self.expected_cadence_ms {
            duration("observation expected_cadence_ms", cadence)?;
            if cadence > self.maximum_age_ms {
                return Err("observation cadence cannot exceed maximum age".into());
            }
        }

        let mut names = std::collections::BTreeSet::new();
        for value in &self.values {
            token("observation name", &value.name, 64, false)?;
            token("observation quantity", &value.quantity, 96, false)?;
            if !names.insert(&value.name) {
                return Err("observation value names must be unique".into());
            }
            if let Some(unit) = &value.unit {
                unit_token(unit)?;
                if !known_unit(unit) {
                    return Err(format!("observation unit {unit} is not registered"));
                }
            }
            value.value.validate()?;
        }
        Ok(())
    }
}

impl ObservationValueKind {
    fn validate(&self) -> Result<(), String> {
        match self {
            Self::Scalar {
                metric,
                aggregations,
            }
            | Self::Vector {
                metric,
                aggregations,
                ..
            } => {
                if aggregations.len() > 10 || has_duplicates(aggregations) {
                    return Err("observation aggregations are duplicated or over limit".into());
                }
                if *metric == ScalarMetric::MonotonicCounter
                    && aggregations.iter().any(|aggregation| {
                        !matches!(
                            aggregation,
                            Aggregation::Last
                                | Aggregation::Sum
                                | Aggregation::Count
                                | Aggregation::Rate
                        )
                    })
                {
                    return Err("monotonic counters contain an invalid aggregation".into());
                }
                if let Self::Vector {
                    maximum_elements, ..
                } = self
                {
                    if *maximum_elements == 0 || *maximum_elements > 65_536 {
                        return Err("vector maximum_elements must be in 1..=65536".into());
                    }
                }
            }
            Self::Histogram { boundaries, .. } => {
                if boundaries.is_empty()
                    || boundaries.len() > 64
                    || boundaries.iter().any(|value| !value.is_finite())
                    || boundaries.windows(2).any(|pair| pair[0] >= pair[1])
                {
                    return Err(
                        "histogram boundaries must be finite, increasing, and 1..=64".into(),
                    );
                }
            }
            Self::State { states } => {
                bounded_unique_tokens("states", states, 64, 64, false)?;
                if states.is_empty() {
                    return Err("state observations require at least one state".into());
                }
            }
            Self::Event { event_types } => {
                bounded_unique_tokens("event types", event_types, 64, 96, false)?;
                if event_types.is_empty() {
                    return Err("event observations require at least one event type".into());
                }
            }
        }
        Ok(())
    }
}

impl ClockFacet {
    fn validate(&self) -> Result<(), String> {
        if matches!(self.kind, ClockKind::Media | ClockKind::Rtp) && self.rate_hz.is_none() {
            return Err("media and RTP clocks require rate_hz".into());
        }
        if self
            .rate_hz
            .is_some_and(|rate| rate == 0 || rate > 1_000_000_000)
        {
            return Err("clock rate_hz must be in 1..=1000000000".into());
        }
        if self
            .timestamp_bits
            .is_some_and(|bits| !(8..=64).contains(&bits))
        {
            return Err("clock timestamp_bits must be in 8..=64".into());
        }
        if let Some(sync) = &self.synchronization {
            token("clock synchronization", sync, 64, false)?;
        }
        Ok(())
    }
}

impl MediaFacet {
    fn validate(&self) -> Result<(), String> {
        if self.width.is_some() != self.height.is_some() {
            return Err("media width and height must be declared together".into());
        }
        if self.width.is_some_and(|value| value == 0 || value > 16_384)
            || self
                .height
                .is_some_and(|value| value == 0 || value > 16_384)
            || self
                .frame_rate_millihz
                .is_some_and(|value| value == 0 || value > 1_000_000)
            || self
                .sample_rate_hz
                .is_some_and(|value| value == 0 || value > 768_000)
            || self.channels.is_some_and(|value| value == 0 || value > 64)
            || self
                .frames_per_packet
                .is_some_and(|value| value == 0 || value > 65_536)
        {
            return Err("media geometry is outside supported bounds".into());
        }
        if self.frame_rate_millihz.is_some() && self.width.is_none() {
            return Err("video frame rate requires width and height".into());
        }
        if (self.channels.is_some() || self.frames_per_packet.is_some())
            && self.sample_rate_hz.is_none()
        {
            return Err("audio channels and packet geometry require sample_rate_hz".into());
        }
        if self.frames_per_packet.is_some() && self.channels.is_none() {
            return Err("frames_per_packet requires channels".into());
        }
        if self.width.is_none() && self.sample_rate_hz.is_none() {
            return Err("media facet must declare video or audio geometry".into());
        }
        Ok(())
    }
}

impl DeliveryFacet {
    fn validate(&self) -> Result<(), String> {
        match (self.class, self.maximum_age_ms, self.coalescing) {
            (DeliveryClass::Command, None, None) => Ok(()),
            (DeliveryClass::Command, Some(_), _) => {
                Err("command delivery cannot declare maximum_age_ms".into())
            }
            (DeliveryClass::Command, None, Some(_)) => {
                Err("command delivery cannot declare coalescing".into())
            }
            (DeliveryClass::Snapshot, Some(maximum_age_ms), Some(_)) => {
                duration("delivery maximum_age_ms", maximum_age_ms)
            }
            (DeliveryClass::Snapshot, Some(_), None) => {
                Err("snapshot delivery requires a coalescing key".into())
            }
            (DeliveryClass::Stream, Some(maximum_age_ms), _) => {
                duration("delivery maximum_age_ms", maximum_age_ms)
            }
            (_, None, _) => Err("stream and snapshot delivery require maximum_age_ms".into()),
        }
    }
}

impl ProvenanceFacet {
    fn validate(&self) -> Result<(), String> {
        token("provenance producer", &self.producer, 96, true)?;
        for (name, value) in [
            ("calibration", self.calibration.as_deref()),
            ("algorithm", self.algorithm.as_deref()),
            ("transform", self.transform.as_deref()),
        ] {
            if let Some(value) = value {
                token(name, value, 128, true)?;
            }
        }
        Ok(())
    }
}

impl EndpointAnnouncementV2 {
    pub fn validate(&self, now_unix_ms: u64) -> Result<(), String> {
        version_v2(self.schema_version)?;
        token("stream_id", &self.stream_id, 128, true)?;
        if let Some(generation) = &self.generation {
            token("generation", generation, 96, false)?;
        }
        if self.bindings.is_empty() || self.bindings.len() > 16 {
            return Err("endpoint announcement must contain 1..=16 bindings".into());
        }
        for binding in &self.bindings {
            token("transport", &binding.transport, 64, false)?;
            token("transport profile", &binding.profile, 64, false)?;
            logical_reference(&binding.endpoint_reference)?;
            token("adapter capability", &binding.requires, 128, false)?;
        }
        if self.observed_at_unix_ms == 0
            || self.valid_until_unix_ms <= self.observed_at_unix_ms
            || self.valid_until_unix_ms - self.observed_at_unix_ms > MAXIMUM_AGE_MS
            || self.valid_until_unix_ms <= now_unix_ms
        {
            return Err("endpoint freshness window is invalid or expired".into());
        }
        self.provenance.validate()
    }
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

fn version_v2(value: u32) -> Result<(), String> {
    (value == V2_SCHEMA_VERSION)
        .then_some(())
        .ok_or_else(|| "unsupported v2 stream document schema".into())
}

fn duration(name: &str, value: u64) -> Result<(), String> {
    (value > 0 && value <= MAXIMUM_AGE_MS)
        .then_some(())
        .ok_or_else(|| format!("{name} must be in 1..={MAXIMUM_AGE_MS}"))
}

fn bounded_map(
    name: &str,
    values: &BTreeMap<String, String>,
    maximum: usize,
    key_maximum: usize,
    value_maximum: usize,
) -> Result<(), String> {
    if values.len() > maximum {
        return Err(format!("{name} exceeds {maximum} entries"));
    }
    for (key, value) in values {
        token(name, key, key_maximum, false)?;
        token(name, value, value_maximum, true)?;
    }
    Ok(())
}

fn bounded_unique_tokens(
    name: &str,
    values: &[String],
    maximum: usize,
    token_maximum: usize,
    slash: bool,
) -> Result<(), String> {
    if values.len() > maximum || has_duplicates(values) {
        return Err(format!("{name} is duplicated or exceeds {maximum} entries"));
    }
    for value in values {
        token(name, value, token_maximum, slash)?;
    }
    Ok(())
}

fn has_duplicates<T: PartialEq>(values: &[T]) -> bool {
    values
        .iter()
        .enumerate()
        .any(|(index, value)| values[..index].contains(value))
}

fn unit_token(value: &str) -> Result<(), String> {
    let valid = !value.is_empty()
        && value.len() <= 32
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"%._/-^{}".contains(&byte));
    valid
        .then_some(())
        .ok_or_else(|| "observation unit is invalid".into())
}

/// Initial closed unit registry for descriptor v2. Expanding it is a
/// compatible data change; arbitrary strings would let spelling drift create
/// distinct metrics in downstream adapters.
fn known_unit(value: &str) -> bool {
    matches!(
        value,
        "1" | "%"
            | "Cel"
            | "K"
            | "Hz"
            | "kHz"
            | "MHz"
            | "s"
            | "ms"
            | "us"
            | "ns"
            | "By"
            | "bit"
            | "By/s"
            | "bit/s"
            | "m/s"
            | "km/h"
            | "dB"
            | "dBm"
            | "dBFS"
            | "V"
            | "A"
            | "W"
            | "Pa"
            | "/min"
            | "{count}"
            | "{frame}"
            | "{packet}"
            | "{sample}"
    )
}

fn logical_reference(value: &str) -> Result<(), String> {
    let valid = !value.is_empty()
        && value.len() <= 256
        && !value.starts_with('/')
        && !value.contains("://")
        && !value.contains('@')
        && !value.contains("..")
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._-/:".contains(&byte));
    valid
        .then_some(())
        .ok_or_else(|| "endpoint_reference must be an opaque logical reference".into())
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
