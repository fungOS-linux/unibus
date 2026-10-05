//! Typed L2 media contracts carried inside opaque Unibus envelopes.
//!
//! Video stays on a negotiated media plane (GStreamer on current edges).
//! Unibus carries discovery, expiring detection state, and bounded
//! presentation intent. Consumers never execute producer-supplied pipelines.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fmt;
use stream_descriptors::{
    ClockFacet, ClockKind, ConnectionRole, DeliveryClass, DeliveryFacet, EncodingFacet,
    EndpointAnnouncementV2, EndpointBindingV2, MediaFacet, PayloadContract, ProvenanceFacet,
    SemanticFacet, StreamDescriptorV2, V2_SCHEMA_VERSION,
};

pub const CAMERA_STREAM_CONTRACT: &str = "camera.stream.v1";
pub const VIDEO_STREAM_CONTRACT: &str = "media.video_stream.v1";
pub const VIDEO_ALPHA_MASK_CONTRACT: &str = "media.video_alpha_mask.v1";
pub const CAMERA_DETECTIONS_CONTRACT: &str = "camera.detections.v1";
pub const CAMERA_PRESENT_CONTRACT: &str = "camera.present.v1";
pub const AUDIO_PLAYBACK_CHUNK_CONTRACT: &str = "audio.playback_chunk";
pub const MEDIA_SCHEMA_VERSION: u32 = 1;
pub const VIDEO_FRAME_V2_CONTRACT: &str = "media.video.frame.v2";
pub const MAX_ALPHA_MASK_PAYLOAD_BYTES: usize = 4 * 1024 * 1024;

/// A complete, expiring alpha plane for one video-stream generation.
///
/// The binary payload is row-major 8-bit alpha encoded as repeating
/// `[run_length: u32 little-endian, value: u8]` records. Every announcement
/// is self-contained: consumers never publish a partially received mask and a
/// late joiner can recover from the producer's next lease renewal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VideoAlphaMask {
    pub schema_version: u32,
    pub stream_id: String,
    pub width: u32,
    pub height: u32,
    pub generation: u64,
    pub payload_sha256: String,
    pub observed_at_unix_ms: u64,
    pub valid_until_unix_ms: u64,
}

impl VideoAlphaMask {
    pub fn validate(&self, payload: &[u8]) -> Result<(), ContractError> {
        if self.schema_version != MEDIA_SCHEMA_VERSION || !slug(&self.stream_id) {
            return Err(ContractError("invalid alpha-mask identity".into()));
        }
        if !(16..=7680).contains(&self.width)
            || !(16..=4320).contains(&self.height)
            || self.generation == 0
        {
            return Err(ContractError(
                "invalid alpha-mask geometry or generation".into(),
            ));
        }
        future(
            self.observed_at_unix_ms,
            self.valid_until_unix_ms,
            "valid_until_unix_ms",
        )?;
        if payload.is_empty() || payload.len() > MAX_ALPHA_MASK_PAYLOAD_BYTES {
            return Err(ContractError("invalid alpha-mask payload size".into()));
        }
        if self.payload_sha256 != payload_sha256(payload) {
            return Err(ContractError("alpha-mask payload digest mismatch".into()));
        }
        decode_alpha_rle(payload, self.width, self.height).map(|_| ())
    }
}

pub fn payload_sha256(payload: &[u8]) -> String {
    Sha256::digest(payload)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

pub fn encode_alpha_rle(alpha: &[u8]) -> Result<Vec<u8>, ContractError> {
    if alpha.is_empty() {
        return Err(ContractError("alpha plane is empty".into()));
    }
    let mut encoded = Vec::new();
    let mut start = 0;
    while start < alpha.len() {
        let value = alpha[start];
        let mut end = start + 1;
        while end < alpha.len() && alpha[end] == value && end - start < u32::MAX as usize {
            end += 1;
        }
        encoded.extend_from_slice(&u32::try_from(end - start).unwrap().to_le_bytes());
        encoded.push(value);
        start = end;
    }
    if encoded.len() > MAX_ALPHA_MASK_PAYLOAD_BYTES {
        return Err(ContractError(
            "compressed alpha mask exceeds payload bound".into(),
        ));
    }
    Ok(encoded)
}

pub fn decode_alpha_rle(payload: &[u8], width: u32, height: u32) -> Result<Vec<u8>, ContractError> {
    let expected = usize::try_from(width)
        .ok()
        .and_then(|width| width.checked_mul(height as usize))
        .ok_or_else(|| ContractError("alpha-mask dimensions overflow".into()))?;
    let mut decoded = Vec::with_capacity(expected);
    let mut chunks = payload.chunks_exact(5);
    for chunk in &mut chunks {
        let count = u32::from_le_bytes(chunk[..4].try_into().unwrap()) as usize;
        if count == 0
            || decoded
                .len()
                .checked_add(count)
                .is_none_or(|len| len > expected)
        {
            return Err(ContractError("invalid alpha-mask RLE run".into()));
        }
        decoded.resize(decoded.len() + count, chunk[4]);
    }
    if !chunks.remainder().is_empty() || decoded.len() != expected {
        return Err(ContractError("alpha-mask RLE length mismatch".into()));
    }
    Ok(decoded)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PcmFormat {
    S16Le,
}

/// Metadata for one bounded PCM payload. Audio bytes ride in the Unibus
/// binary payload and never in JSON. Sequence and capture time make loss and
/// end-to-end latency observable without pretending TCP delivery is playback
/// timing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AudioPlaybackChunk {
    pub schema_version: u32,
    pub stream_id: String,
    pub sequence: u64,
    pub captured_at_unix_ms: u64,
    pub sample_rate_hz: u32,
    pub channels: u16,
    pub frames: u32,
    pub format: PcmFormat,
}

impl AudioPlaybackChunk {
    pub fn validate(&self, payload_bytes: usize) -> Result<(), ContractError> {
        if self.schema_version != MEDIA_SCHEMA_VERSION || !slug(&self.stream_id) {
            return Err(ContractError("invalid audio stream identity".into()));
        }
        if !(8_000..=192_000).contains(&self.sample_rate_hz)
            || !(1..=8).contains(&self.channels)
            || self.frames == 0
            || self.frames > self.sample_rate_hz / 10
        {
            return Err(ContractError("invalid PCM geometry".into()));
        }
        let expected = self.frames as usize * self.channels as usize * 2;
        if payload_bytes != expected {
            return Err(ContractError(format!(
                "PCM payload is {payload_bytes} bytes; expected {expected}"
            )));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VideoTransport {
    Rtsp,
    Srt,
    UdpMpegTs,
    TcpMpegTs,
}

/// Which side opens a connection-oriented media transport. Datagram and RTSP
/// transports ignore this field; TCP MPEG-TS requires it so an announcement
/// cannot ambiguously make every consumer try to bind the same port.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransportRole {
    #[default]
    Listen,
    Connect,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VideoCodec {
    H264,
    H265,
}

/// A named stream announcement. `endpoint` is data passed to a trusted media
/// adapter, never a shell command or GStreamer pipeline string.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CameraStream {
    pub schema_version: u32,
    pub camera_id: String,
    pub display_name: String,
    pub zone: String,
    pub transport: VideoTransport,
    #[serde(default)]
    pub transport_role: TransportRole,
    pub endpoint: String,
    pub codec: VideoCodec,
    pub width: u32,
    pub height: u32,
    pub frame_rate_millihz: u32,
    pub announced_at_unix_ms: u64,
    pub valid_until_unix_ms: u64,
}

/// Transport-neutral moving pictures that are not cameras (games,
/// visualizers, presentations). Canvas consumes this exactly like a camera
/// frame source, without inheriting camera-only detection semantics.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VideoStream {
    pub schema_version: u32,
    pub stream_id: String,
    pub display_name: String,
    pub zone: String,
    pub transport: VideoTransport,
    #[serde(default)]
    pub transport_role: TransportRole,
    pub endpoint: String,
    pub codec: VideoCodec,
    pub width: u32,
    pub height: u32,
    pub frame_rate_millihz: u32,
    pub announced_at_unix_ms: u64,
    pub valid_until_unix_ms: u64,
}

/// Stable v2 stream meaning plus a separately expiring endpoint lease.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MediaStreamProjectionV2 {
    pub descriptor: StreamDescriptorV2,
    pub endpoints: EndpointAnnouncementV2,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NormalizedBox {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Detection {
    pub tracking_id: String,
    pub label: String,
    pub confidence: f32,
    pub bounds: NormalizedBox,
}

/// Complete latest-value replacement for one camera, not an accumulating
/// event log. Empty means the producer currently sees no tracked objects.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CameraDetections {
    pub schema_version: u32,
    pub camera_id: String,
    pub observed_at_unix_ms: u64,
    pub valid_until_unix_ms: u64,
    pub detections: Vec<Detection>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PresentationMode {
    Focus,
    Tile,
    PictureInPicture,
}

/// A bounded request to show an already-announced camera. It grants no media
/// or action authority; Canvas applies local presentation policy and expiry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CameraPresentation {
    pub schema_version: u32,
    pub camera_id: String,
    pub target_screen: String,
    pub mode: PresentationMode,
    pub reason: String,
    pub requested_at_unix_ms: u64,
    pub visible_until_unix_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContractError(pub String);

impl fmt::Display for ContractError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for ContractError {}

fn slug(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

fn future(start: u64, end: u64, field: &str) -> Result<(), ContractError> {
    (end > start)
        .then_some(())
        .ok_or_else(|| ContractError(format!("{field} must be after its source timestamp")))
}

impl CameraStream {
    pub fn validate(&self) -> Result<(), ContractError> {
        validate_video(
            self.schema_version,
            &self.camera_id,
            &self.display_name,
            &self.zone,
            &self.endpoint,
            self.width,
            self.height,
            self.frame_rate_millihz,
            self.announced_at_unix_ms,
            self.valid_until_unix_ms,
        )
    }

    /// Project a legacy camera announcement into v2 without copying its raw
    /// endpoint URL into discovery. `endpoint_reference` is an opaque key
    /// resolved by the trusted media adapter that already owns `self.endpoint`.
    pub fn project_v2(
        &self,
        endpoint_reference: &str,
        generation: &str,
        producer: &str,
    ) -> Result<MediaStreamProjectionV2, ContractError> {
        self.validate()?;
        project_video_v2(
            &format!("camera/{}/live", self.camera_id),
            &format!("camera/{}", self.camera_id),
            "video.camera",
            &self.zone,
            &self.transport,
            self.transport_role,
            &self.codec,
            self.width,
            self.height,
            self.frame_rate_millihz,
            self.announced_at_unix_ms,
            self.valid_until_unix_ms,
            endpoint_reference,
            generation,
            producer,
        )
    }
}

impl VideoStream {
    pub fn validate(&self) -> Result<(), ContractError> {
        validate_video(
            self.schema_version,
            &self.stream_id,
            &self.display_name,
            &self.zone,
            &self.endpoint,
            self.width,
            self.height,
            self.frame_rate_millihz,
            self.announced_at_unix_ms,
            self.valid_until_unix_ms,
        )
    }

    pub fn project_v2(
        &self,
        endpoint_reference: &str,
        generation: &str,
        producer: &str,
    ) -> Result<MediaStreamProjectionV2, ContractError> {
        self.validate()?;
        project_video_v2(
            &format!("video/{}/live", self.stream_id),
            &format!("video/{}", self.stream_id),
            "video.stream",
            &self.zone,
            &self.transport,
            self.transport_role,
            &self.codec,
            self.width,
            self.height,
            self.frame_rate_millihz,
            self.announced_at_unix_ms,
            self.valid_until_unix_ms,
            endpoint_reference,
            generation,
            producer,
        )
    }
}

#[allow(clippy::too_many_arguments)]
fn project_video_v2(
    stream_id: &str,
    source_resource: &str,
    semantic_kind: &str,
    zone: &str,
    transport: &VideoTransport,
    legacy_role: TransportRole,
    codec: &VideoCodec,
    width: u32,
    height: u32,
    frame_rate_millihz: u32,
    announced_at_unix_ms: u64,
    valid_until_unix_ms: u64,
    endpoint_reference: &str,
    generation: &str,
    producer: &str,
) -> Result<MediaStreamProjectionV2, ContractError> {
    let encoding = match codec {
        VideoCodec::H264 => "h264",
        VideoCodec::H265 => "h265",
    };
    let decode_capability = format!("media.video.decode.{encoding}");
    let (transport_name, profile, role) = match transport {
        VideoTransport::Rtsp => ("rtsp", "rtsp-1.0", ConnectionRole::Connector),
        VideoTransport::Srt => ("srt", "srt", opposite_connection_role(legacy_role)),
        VideoTransport::UdpMpegTs => ("udp-mpeg-ts", "mpeg-ts", ConnectionRole::Connectionless),
        VideoTransport::TcpMpegTs => (
            "tcp-mpeg-ts",
            "mpeg-ts",
            opposite_connection_role(legacy_role),
        ),
    };
    let maximum_age_ms = u64::from((2_000_000u32 / frame_rate_millihz).clamp(100, 5_000));
    let provenance = ProvenanceFacet {
        producer: producer.into(),
        calibration: None,
        algorithm: None,
        transform: Some("unibus-media/v1-projection".into()),
    };
    let descriptor = StreamDescriptorV2 {
        schema_version: V2_SCHEMA_VERSION,
        stream_id: stream_id.into(),
        source_resource: source_resource.into(),
        semantic: SemanticFacet {
            kind: semantic_kind.into(),
            dimensions: std::collections::BTreeMap::from([("zone".into(), zone.into())]),
        },
        payload: Some(PayloadContract {
            contract: VIDEO_FRAME_V2_CONTRACT.into(),
            maximum_bytes: None,
        }),
        observation: None,
        encoding: Some(EncodingFacet {
            kind: encoding.into(),
            parameters: std::collections::BTreeMap::new(),
        }),
        clock: Some(ClockFacet {
            kind: ClockKind::Media,
            rate_hz: Some(90_000),
            timestamp_bits: Some(32),
            synchronization: None,
        }),
        media: Some(MediaFacet {
            width: Some(width),
            height: Some(height),
            frame_rate_millihz: Some(frame_rate_millihz),
            sample_rate_hz: None,
            channels: None,
            frames_per_packet: None,
        }),
        delivery: DeliveryFacet {
            class: DeliveryClass::Stream,
            maximum_age_ms: Some(maximum_age_ms),
            coalescing: None,
        },
        required_consumer_capabilities: vec![decode_capability],
        provenance: provenance.clone(),
    };
    let endpoints = EndpointAnnouncementV2 {
        schema_version: V2_SCHEMA_VERSION,
        stream_id: stream_id.into(),
        generation: Some(generation.into()),
        bindings: vec![EndpointBindingV2 {
            transport: transport_name.into(),
            profile: profile.into(),
            role,
            endpoint_reference: endpoint_reference.into(),
            requires: format!("media.transport.{transport_name}"),
        }],
        observed_at_unix_ms: announced_at_unix_ms,
        valid_until_unix_ms,
        provenance,
    };
    descriptor.validate().map_err(ContractError)?;
    endpoints
        .validate(announced_at_unix_ms)
        .map_err(ContractError)?;
    Ok(MediaStreamProjectionV2 {
        descriptor,
        endpoints,
    })
}

fn opposite_connection_role(role: TransportRole) -> ConnectionRole {
    match role {
        TransportRole::Listen => ConnectionRole::Connector,
        TransportRole::Connect => ConnectionRole::Listener,
    }
}

#[allow(clippy::too_many_arguments)]
fn validate_video(
    schema: u32,
    id: &str,
    display_name: &str,
    zone: &str,
    endpoint: &str,
    width: u32,
    height: u32,
    fps: u32,
    announced: u64,
    valid_until: u64,
) -> Result<(), ContractError> {
    if schema != MEDIA_SCHEMA_VERSION || !slug(id) || !slug(zone) {
        return Err(ContractError("invalid video stream identity".into()));
    }
    if display_name.trim().is_empty() || display_name.len() > 128 {
        return Err(ContractError("invalid display_name".into()));
    }
    if endpoint.trim().is_empty() || endpoint.len() > 2048 {
        return Err(ContractError("invalid media endpoint".into()));
    }
    if !(16..=7680).contains(&width)
        || !(16..=4320).contains(&height)
        || !(1_000..=120_000).contains(&fps)
    {
        return Err(ContractError("invalid video geometry or frame rate".into()));
    }
    future(announced, valid_until, "valid_until_unix_ms")
}

impl CameraDetections {
    pub fn validate(&self) -> Result<(), ContractError> {
        if self.schema_version != MEDIA_SCHEMA_VERSION || !slug(&self.camera_id) {
            return Err(ContractError("invalid detection source".into()));
        }
        future(
            self.observed_at_unix_ms,
            self.valid_until_unix_ms,
            "valid_until_unix_ms",
        )?;
        if self.detections.len() > 256 {
            return Err(ContractError("too many detections".into()));
        }
        for detection in &self.detections {
            if detection.tracking_id.is_empty()
                || detection.tracking_id.len() > 128
                || !slug(&detection.label)
                || !detection.confidence.is_finite()
                || !(0.0..=1.0).contains(&detection.confidence)
                || !detection.bounds.validate()
            {
                return Err(ContractError("invalid detection".into()));
            }
        }
        Ok(())
    }
}

impl NormalizedBox {
    fn validate(self) -> bool {
        [self.x, self.y, self.width, self.height]
            .into_iter()
            .all(f32::is_finite)
            && self.x >= 0.0
            && self.y >= 0.0
            && self.width > 0.0
            && self.height > 0.0
            && self.x + self.width <= 1.0
            && self.y + self.height <= 1.0
    }
}

impl CameraPresentation {
    pub fn validate(&self) -> Result<(), ContractError> {
        if self.schema_version != MEDIA_SCHEMA_VERSION
            || !slug(&self.camera_id)
            || !slug(&self.target_screen)
            || self.reason.trim().is_empty()
            || self.reason.len() > 256
        {
            return Err(ContractError("invalid camera presentation".into()));
        }
        future(
            self.requested_at_unix_ms,
            self.visible_until_unix_ms,
            "visible_until_unix_ms",
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stream() -> CameraStream {
        CameraStream {
            schema_version: 1,
            camera_id: "front-door".into(),
            display_name: "Front Door".into(),
            zone: "door".into(),
            transport: VideoTransport::Rtsp,
            transport_role: TransportRole::Listen,
            endpoint: "rtsp://nvr.example/door".into(),
            codec: VideoCodec::H264,
            width: 704,
            height: 480,
            frame_rate_millihz: 5_000,
            announced_at_unix_ms: 100,
            valid_until_unix_ms: 5_100,
        }
    }

    #[test]
    fn audio_chunks_are_small_bounded_and_exact() {
        let chunk = AudioPlaybackChunk {
            schema_version: MEDIA_SCHEMA_VERSION,
            stream_id: "home-playback".into(),
            sequence: 7,
            captured_at_unix_ms: 100,
            sample_rate_hz: 48_000,
            channels: 2,
            frames: 480,
            format: PcmFormat::S16Le,
        };
        assert!(chunk.validate(1_920).is_ok());
        assert!(chunk.validate(1_918).is_err());
        let mut too_long = chunk.clone();
        too_long.frames = 4_801;
        assert!(too_long.validate(19_204).is_err());
    }

    #[test]
    fn stream_round_trips_and_validates() {
        let stream = stream();
        stream.validate().unwrap();
        let wire = serde_json::to_string(&stream).unwrap();
        assert_eq!(serde_json::from_str::<CameraStream>(&wire).unwrap(), stream);
    }

    #[test]
    fn camera_v1_projects_to_separate_v2_descriptor_and_lease() {
        let legacy = stream();
        let projection = legacy
            .project_v2("frigate/front-door", "boot-7", "frigate")
            .unwrap();
        projection.descriptor.validate().unwrap();
        projection.endpoints.validate(100).unwrap();
        assert_eq!(projection.descriptor.stream_id, "camera/front-door/live");
        assert_eq!(
            projection.descriptor.required_consumer_capabilities,
            ["media.video.decode.h264"]
        );
        let binding = &projection.endpoints.bindings[0];
        assert_eq!(binding.transport, "rtsp");
        assert_eq!(binding.profile, "rtsp-1.0");
        assert_eq!(binding.role, ConnectionRole::Connector);
        assert_eq!(binding.endpoint_reference, "frigate/front-door");

        let wire = serde_json::to_string(&projection).unwrap();
        assert!(!wire.contains(&legacy.endpoint));
        assert!(!wire.contains("rtsp://"));
    }

    #[test]
    fn projection_rejects_url_instead_of_leaking_legacy_endpoint() {
        let error = stream()
            .project_v2("rtsp://user:secret@camera/live", "boot-7", "frigate")
            .unwrap_err();
        assert!(error.to_string().contains("logical reference"));
    }

    #[test]
    fn transport_and_codec_projection_is_data_driven() {
        let cases = [
            (
                VideoTransport::Srt,
                TransportRole::Connect,
                VideoCodec::H265,
                "srt",
                ConnectionRole::Listener,
                "media.video.decode.h265",
            ),
            (
                VideoTransport::UdpMpegTs,
                TransportRole::Listen,
                VideoCodec::H264,
                "udp-mpeg-ts",
                ConnectionRole::Connectionless,
                "media.video.decode.h264",
            ),
            (
                VideoTransport::TcpMpegTs,
                TransportRole::Listen,
                VideoCodec::H264,
                "tcp-mpeg-ts",
                ConnectionRole::Connector,
                "media.video.decode.h264",
            ),
        ];
        for (transport, role, codec, expected_transport, expected_role, expected_decode) in cases {
            let mut legacy = stream();
            legacy.transport = transport;
            legacy.transport_role = role;
            legacy.codec = codec;
            let projection = legacy
                .project_v2("resolver/front-door", "boot-7", "frigate")
                .unwrap();
            let binding = &projection.endpoints.bindings[0];
            assert_eq!(binding.transport, expected_transport);
            assert_eq!(binding.role, expected_role);
            assert_eq!(
                projection.descriptor.required_consumer_capabilities,
                [expected_decode]
            );
        }
    }

    #[test]
    fn alpha_mask_round_trips_and_verifies_digest() {
        let alpha = [0, 0, 0, 128, 128, 255, 255, 255];
        let payload = encode_alpha_rle(&alpha).unwrap();
        let mask = VideoAlphaMask {
            schema_version: MEDIA_SCHEMA_VERSION,
            stream_id: "ghidra-mac".into(),
            width: 4,
            height: 2,
            generation: 7,
            payload_sha256: payload_sha256(&payload),
            observed_at_unix_ms: 100,
            valid_until_unix_ms: 1_100,
        };
        // Production geometry starts at 16 pixels; exercise the codec here
        // and the contract's geometry rejection separately.
        assert_eq!(decode_alpha_rle(&payload, 4, 2).unwrap(), alpha);
        assert!(mask.validate(&payload).is_err());
        let mut mask = mask;
        mask.width = 16;
        mask.height = 16;
        let payload = encode_alpha_rle(&vec![255; 256]).unwrap();
        mask.payload_sha256 = payload_sha256(&payload);
        mask.validate(&payload).unwrap();
        let mut corrupted = payload;
        corrupted[4] = 0;
        assert!(mask.validate(&corrupted).is_err());
    }

    #[test]
    fn alpha_rle_rejects_partial_and_overflowing_runs() {
        assert!(decode_alpha_rle(&[1, 0, 0, 0], 16, 16).is_err());
        assert!(decode_alpha_rle(&[1, 1, 0, 0, 255], 16, 16).is_err());
    }

    #[test]
    fn detections_require_normalized_finite_boxes() {
        let mut detections = CameraDetections {
            schema_version: 1,
            camera_id: "front-door".into(),
            observed_at_unix_ms: 100,
            valid_until_unix_ms: 1_100,
            detections: vec![Detection {
                tracking_id: "object-7".into(),
                label: "person".into(),
                confidence: 0.91,
                bounds: NormalizedBox {
                    x: 0.1,
                    y: 0.2,
                    width: 0.3,
                    height: 0.4,
                },
            }],
        };
        detections.validate().unwrap();
        detections.detections[0].bounds.width = 1.0;
        assert!(detections.validate().is_err());
    }

    #[test]
    fn presentation_is_bounded_and_contains_no_endpoint() {
        let request = CameraPresentation {
            schema_version: 1,
            camera_id: "front-door".into(),
            target_screen: "home-tv".into(),
            mode: PresentationMode::Focus,
            reason: "person detected".into(),
            requested_at_unix_ms: 100,
            visible_until_unix_ms: 10_100,
        };
        request.validate().unwrap();
        let wire = serde_json::to_string(&request).unwrap();
        assert!(!wire.contains("endpoint"));
    }
}
