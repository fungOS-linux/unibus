//! Typed L2 media contracts carried inside opaque Unibus envelopes.
//!
//! Video stays on a negotiated media plane (GStreamer on current edges).
//! Unibus carries discovery, expiring detection state, and bounded
//! presentation intent. Consumers never execute producer-supplied pipelines.

use serde::{Deserialize, Serialize};
use std::fmt;

pub const CAMERA_STREAM_CONTRACT: &str = "camera.stream.v1";
pub const VIDEO_STREAM_CONTRACT: &str = "media.video_stream.v1";
pub const CAMERA_DETECTIONS_CONTRACT: &str = "camera.detections.v1";
pub const CAMERA_PRESENT_CONTRACT: &str = "camera.present.v1";
pub const AUDIO_PLAYBACK_CHUNK_CONTRACT: &str = "audio.playback_chunk";
pub const MEDIA_SCHEMA_VERSION: u32 = 1;

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
