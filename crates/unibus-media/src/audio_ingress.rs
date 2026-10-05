//! Deterministic admission at the actual PCM sink boundary.
use crate::{AudioPlaybackChunk, ContractError};

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct AudioIngressMetrics {
    pub accepted: u64,
    pub expired: u64,
    pub reordered: u64,
    pub missing_chunks: u64,
    pub local_drops: u64,
}

pub struct AudioIngress {
    stream_id: String,
    maximum_age_ms: u64,
    geometry: Option<(u32, u16)>,
    newest: Option<u64>,
    metrics: AudioIngressMetrics,
}

impl AudioIngress {
    pub fn new(stream_id: String, maximum_age_ms: u64) -> Result<Self, ContractError> {
        if maximum_age_ms == 0 || maximum_age_ms > 1_000 {
            return Err(ContractError("audio freshness must be 1..=1000 ms".into()));
        }
        Ok(Self {
            stream_id,
            maximum_age_ms,
            geometry: None,
            newest: None,
            metrics: Default::default(),
        })
    }

    pub fn admit(
        &mut self,
        chunk: &AudioPlaybackChunk,
        payload_bytes: usize,
        now_ms: u64,
    ) -> Result<bool, ContractError> {
        chunk.validate(payload_bytes)?;
        if chunk.stream_id != self.stream_id {
            return Ok(false);
        }
        if chunk.captured_at_unix_ms > now_ms
            || now_ms - chunk.captured_at_unix_ms > self.maximum_age_ms
        {
            self.metrics.expired = self.metrics.expired.saturating_add(1);
            return Ok(false);
        }
        let geometry = (chunk.sample_rate_hz, chunk.channels);
        if self.geometry.is_some_and(|current| current != geometry) {
            return Err(ContractError(
                "PCM geometry changed within a sink session".into(),
            ));
        }
        if let Some(newest) = self.newest {
            let distance = chunk.sequence.wrapping_sub(newest);
            if distance == 0 || distance >= (1_u64 << 63) {
                self.metrics.reordered = self.metrics.reordered.saturating_add(1);
                return Ok(false);
            }
            self.metrics.missing_chunks = self.metrics.missing_chunks.saturating_add(distance - 1);
        }
        self.geometry = Some(geometry);
        self.newest = Some(chunk.sequence);
        self.metrics.accepted = self.metrics.accepted.saturating_add(1);
        Ok(true)
    }

    pub fn local_drop(&mut self) {
        self.metrics.local_drops = self.metrics.local_drops.saturating_add(1);
    }
    pub fn metrics(&self) -> AudioIngressMetrics {
        self.metrics
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PcmFormat;
    fn chunk(sequence: u64) -> AudioPlaybackChunk {
        AudioPlaybackChunk {
            schema_version: 1,
            stream_id: "program".into(),
            sequence,
            captured_at_unix_ms: 1000,
            sample_rate_hz: 48000,
            channels: 2,
            frames: 480,
            format: PcmFormat::S16Le,
        }
    }
    #[test]
    fn sink_admission_rejects_stale_duplicate_and_changed_geometry() {
        let mut ingress = AudioIngress::new("program".into(), 40).unwrap();
        assert!(!ingress.admit(&chunk(0), 1920, 1041).unwrap());
        assert!(ingress.admit(&chunk(0), 1920, 1020).unwrap());
        assert!(!ingress.admit(&chunk(0), 1920, 1020).unwrap());
        assert!(ingress.admit(&chunk(2), 1920, 1020).unwrap());
        let mut changed = chunk(3);
        changed.sample_rate_hz = 44100;
        assert!(ingress.admit(&changed, 1920, 1020).is_err());
        ingress.local_drop();
        assert_eq!(
            ingress.metrics(),
            AudioIngressMetrics {
                accepted: 2,
                expired: 1,
                reordered: 1,
                missing_chunks: 1,
                local_drops: 1
            }
        );
    }
    #[test]
    fn sequence_wrap_is_forward_progress() {
        let mut ingress = AudioIngress::new("program".into(), 40).unwrap();
        assert!(ingress.admit(&chunk(u64::MAX), 1920, 1020).unwrap());
        assert!(ingress.admit(&chunk(0), 1920, 1020).unwrap());
        assert_eq!(ingress.metrics().missing_chunks, 0);
    }
}
