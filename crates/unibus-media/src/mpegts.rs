//! Compressed MPEG-TS payload contract carried by leased Unibus stream events.
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

pub const MPEGTS_CONTRACT: &str = "media.mpegts.chunk.v1";
pub const TS_PACKET_BYTES: usize = 188;
pub const MAX_CHUNK_BYTES: usize = TS_PACKET_BYTES * 32;
pub const CHUNK_TTL_MS: u64 = 1000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MpegTsHeader {
    pub generation: String,
    pub width: u32,
    pub height: u32,
    pub codec: String,
}

impl MpegTsHeader {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            !self.generation.is_empty()
                && self.generation.len() <= 64
                && self
                    .generation
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-'),
            "invalid video generation"
        );
        ensure!(
            (16..=8192).contains(&self.width) && (16..=8192).contains(&self.height),
            "invalid video geometry"
        );
        ensure!(self.codec == "h264", "unsupported reel codec");
        Ok(())
    }
}

pub fn validate_payload(header: &MpegTsHeader, payload: &[u8]) -> Result<()> {
    header.validate()?;
    ensure!(
        !payload.is_empty()
            && payload.len() <= MAX_CHUNK_BYTES
            && payload.len().is_multiple_of(TS_PACKET_BYTES),
        "invalid MPEG-TS chunk size"
    );
    ensure!(
        payload.chunks_exact(TS_PACKET_BYTES).all(|p| p[0] == 0x47),
        "MPEG-TS synchronization lost"
    );
    Ok(())
}

pub fn validate_delivery(
    observed: u64,
    now: u64,
    previous: Option<u64>,
    sequence: u64,
) -> Result<()> {
    ensure!(
        sequence > 0 && previous.is_none_or(|p| p.checked_add(1) == Some(sequence)),
        "video chunk gap/replay; rejoin at a decoder boundary"
    );
    ensure!(
        observed <= now.saturating_add(CHUNK_TTL_MS)
            && now.saturating_sub(observed) <= CHUNK_TTL_MS,
        "video chunk expired"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn payload_is_bounded_aligned_and_geometry_bound() {
        let mut h = MpegTsHeader {
            generation: "123".into(),
            width: 1080,
            height: 1920,
            codec: "h264".into(),
        };
        let mut p = vec![0; TS_PACKET_BYTES * 2];
        p[0] = 0x47;
        p[188] = 0x47;
        assert!(validate_payload(&h, &p).is_ok());
        assert!(validate_payload(&h, &p[..189]).is_err());
        p[188] = 0;
        assert!(validate_payload(&h, &p).is_err());
        assert!(validate_payload(&h, &vec![0; MAX_CHUNK_BYTES + 188]).is_err());
        h.width = 0;
        assert!(h.validate().is_err());
    }
    #[test]
    fn gaps_replays_and_late_video_fail_closed() {
        assert!(validate_delivery(1000, 1500, None, 42).is_ok());
        assert!(validate_delivery(1000, 1500, Some(41), 42).is_ok());
        assert!(validate_delivery(1000, 1500, Some(40), 42).is_err());
        assert!(validate_delivery(1000, 1500, Some(42), 42).is_err());
        assert!(validate_delivery(1000, 2001, None, 42).is_err());
        assert!(validate_delivery(3000, 1000, None, 42).is_err());
    }
}
