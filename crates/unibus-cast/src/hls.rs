//! Bounded in-memory live HLS window. Completed, codec-verified GOPs only.
use std::{collections::VecDeque, sync::Arc};

pub const MAX_SEGMENT_BYTES: usize = 2 * 1024 * 1024;
const WINDOW: usize = 6;
const PTS_WRAP: u64 = 1 << 33;

#[derive(Default)]
pub struct HlsWindow {
    segments: VecDeque<Segment>,
    next: u64,
}
struct Segment {
    sequence: u64,
    duration: f64,
    bytes: Arc<Vec<u8>>,
}
impl HlsWindow {
    pub fn clear(&mut self) {
        self.segments.clear();
    }
    pub fn push(&mut self, bytes: Vec<u8>, start_pts: u64, end_pts: u64) {
        let ticks = end_pts.wrapping_sub(start_pts) & (PTS_WRAP - 1);
        if bytes.is_empty() || bytes.len() > MAX_SEGMENT_BYTES || ticks == 0 || ticks > 900_000 {
            self.clear();
            return;
        }
        self.segments.push_back(Segment {
            sequence: self.next,
            duration: ticks as f64 / 90_000.0,
            bytes: Arc::new(bytes),
        });
        self.next += 1;
        while self.segments.len() > WINDOW {
            self.segments.pop_front();
        }
    }
    pub fn playlist(&self) -> Option<String> {
        if self.segments.len() < 2 {
            return None;
        }
        let target = self
            .segments
            .iter()
            .map(|s| s.duration.ceil() as u64)
            .max()?;
        let mut result = format!(
            "#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:{}\n#EXT-X-MEDIA-SEQUENCE:{}\n",
            target.max(1),
            self.segments.front()?.sequence
        );
        for segment in &self.segments {
            result.push_str(&format!(
                "#EXTINF:{:.6},\nsegments/{}.ts\n",
                segment.duration, segment.sequence
            ));
        }
        Some(result)
    }
    pub fn segment(&self, sequence: u64) -> Option<Arc<Vec<u8>>> {
        self.segments
            .iter()
            .find(|s| s.sequence == sequence)
            .map(|s| s.bytes.clone())
    }
}

/// PES PTS from a payload-unit-start TS packet, with marker-bit validation.
pub fn pes_pts(payload: &[u8]) -> Option<u64> {
    if payload.len() < 14 || payload[..3] != [0, 0, 1] || payload[7] & 0x80 == 0 || payload[8] < 5 {
        return None;
    }
    let p = &payload[9..14];
    if p[0] & 1 == 0 || p[2] & 1 == 0 || p[4] & 1 == 0 {
        return None;
    }
    Some(
        (u64::from(p[0] & 14) << 29)
            | (u64::from(p[1]) << 22)
            | (u64::from(p[2] & 254) << 14)
            | (u64::from(p[3]) << 7)
            | u64::from(p[4] >> 1),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn retains_bounded_completed_segments_and_monotonic_sequence() {
        let mut h = HlsWindow::default();
        assert!(h.playlist().is_none());
        for i in 0..8 {
            h.push(vec![i as u8; 188], i * 90_000, (i + 1) * 90_000);
        }
        let p = h.playlist().unwrap();
        assert!(p.contains("#EXT-X-MEDIA-SEQUENCE:2\n"));
        assert_eq!(p.matches("#EXTINF:").count(), 6);
        assert!(h.segment(1).is_none());
        assert_eq!(h.segment(7).unwrap().len(), 188);
        h.clear();
        assert!(h.segment(7).is_none());
        h.push(vec![0; 188], 0, 90_000);
        h.push(vec![0; 188], 90_000, 180_000);
        assert!(h.playlist().unwrap().contains("#EXT-X-MEDIA-SEQUENCE:8\n"));
    }
    #[test]
    fn pts_wrap_and_invalid_or_oversized_segment() {
        let mut h = HlsWindow::default();
        h.push(vec![0; 188], PTS_WRAP - 45_000, 45_000);
        h.push(vec![0; 188], 45_000, 135_000);
        assert!(h.playlist().unwrap().contains("#EXTINF:1.000000,"));
        h.push(vec![0; MAX_SEGMENT_BYTES + 1], 0, 90_000);
        assert!(h.playlist().is_none());
        h.push(vec![0; 188], 0, 0);
        assert!(h.playlist().is_none());
    }
    #[test]
    fn reads_pts_and_rejects_bad_markers_or_truncation() {
        let mut pes = vec![0, 0, 1, 0xe0, 0, 0, 0x80, 0x80, 5, 0x21, 0, 5, 0xbf, 0x21];
        assert_eq!(pes_pts(&pes), Some(90_000));
        pes[13] &= !1;
        assert!(pes_pts(&pes).is_none());
        assert!(pes_pts(&pes[..10]).is_none());
    }
}
