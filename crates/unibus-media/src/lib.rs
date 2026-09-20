//! Typed L2 media contracts carried inside opaque Unibus envelopes.
//!
//! Video stays on a negotiated media plane (GStreamer on current edges).
//! Unibus carries discovery, expiring detection state, and bounded
//! presentation intent. Consumers never execute producer-supplied pipelines.

use serde::{Deserialize, Serialize};
use std::fmt;

pub const CAMERA_STREAM_CONTRACT: &str = "camera.stream.v1";
pub const CAMERA_DETECTIONS_CONTRACT: &str = "camera.detections.v1";
pub const CAMERA_PRESENT_CONTRACT: &str = "camera.present.v1";
pub const MEDIA_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VideoTransport {
    Rtsp,
    Srt,
    UdpMpegTs,
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
        if self.schema_version != MEDIA_SCHEMA_VERSION {
            return Err(ContractError("unsupported camera stream schema".into()));
        }
        if !slug(&self.camera_id) || !slug(&self.zone) {
            return Err(ContractError("camera_id and zone must be slugs".into()));
        }
        if self.display_name.trim().is_empty() || self.display_name.len() > 128 {
            return Err(ContractError("invalid display_name".into()));
        }
        if self.endpoint.trim().is_empty() || self.endpoint.len() > 2048 {
            return Err(ContractError("invalid media endpoint".into()));
        }
        if !(16..=7680).contains(&self.width)
            || !(16..=4320).contains(&self.height)
            || !(1_000..=120_000).contains(&self.frame_rate_millihz)
        {
            return Err(ContractError("invalid video geometry or frame rate".into()));
        }
        future(
            self.announced_at_unix_ms,
            self.valid_until_unix_ms,
            "valid_until_unix_ms",
        )
    }
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
