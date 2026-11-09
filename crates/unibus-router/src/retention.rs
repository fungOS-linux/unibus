//! A short retained window per kind, and the sink for what falls out of it.
//!
//! A hub that drops a frame when no live node holds its kind loses it. This
//! keeps a bounded window per kind so a consumer that reconnects - or
//! restarts - can ask for what it missed, and records the frames that fall
//! out of that window without ever reaching anyone.
//!
//! Deliberately small: no acks, no sessions, no per-consumer state. Every
//! consumer passes its own cursor, so one shared ring serves all of them, and
//! anything slower than the TTL is told `stale` rather than quietly skipped.

use std::collections::{BTreeMap, VecDeque};
use std::io::Write as _;
use std::path::{Path, PathBuf};

use unibus::Envelope;

/// Frames kept per kind before the oldest is evicted.
pub const DEFAULT_PER_KIND: usize = 512;

/// How long a retained frame stays replayable.
pub const DEFAULT_TTL_MS: u64 = 5 * 60 * 1000;

/// Most frames one replay may return.
///
/// Bounded by the control body ceiling (64 KiB) rather than by the ring size: a
/// page is a reply envelope, not a stream. A consumer that needs more asks
/// again with the cursor it was given.
pub const MAX_REPLAY: usize = 32;

/// Why a frame left the window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Evicted {
    /// The ring was full and this frame was the oldest.
    OverCapacity,
    /// The frame aged out of the TTL window.
    Expired,
}

impl Evicted {
    pub fn reason(self) -> &'static str {
        match self {
            Self::OverCapacity => "over_capacity",
            Self::Expired => "expired",
        }
    }
}

/// One frame still inside the window.
#[derive(Clone, Debug)]
pub struct Retained {
    pub sequence: u64,
    pub kind: String,
    pub envelope: Envelope,
    pub payload: Vec<u8>,
    pub retained_at_ms: u64,
}

/// The answer to a replay request.
#[derive(Clone, Debug, Default)]
pub struct Page {
    pub frames: Vec<Retained>,
    /// Cursor to send next time. `None` when nothing matched at all.
    pub next: Option<String>,
    /// The window rolled past the requested cursor: frames were lost.
    pub stale: bool,
}

/// Bounded, time-limited frames per kind.
#[derive(Debug)]
pub struct Retention {
    per_kind: usize,
    ttl_ms: u64,
    next_sequence: u64,
    rings: BTreeMap<String, VecDeque<Retained>>,
}

impl Default for Retention {
    fn default() -> Self {
        Self::new(DEFAULT_PER_KIND, DEFAULT_TTL_MS)
    }
}

impl Retention {
    pub fn new(per_kind: usize, ttl_ms: u64) -> Self {
        Self {
            per_kind: per_kind.max(1),
            ttl_ms: ttl_ms.max(1),
            next_sequence: 0,
            rings: BTreeMap::new(),
        }
    }

    /// Keep one frame that reached nobody, and report what fell out.
    pub fn retain(
        &mut self,
        envelope: &Envelope,
        payload: &[u8],
        now_ms: u64,
    ) -> Vec<(Retained, Evicted)> {
        let sequence = self.next_sequence;
        self.next_sequence = self.next_sequence.wrapping_add(1);
        let frame = Retained {
            sequence,
            kind: envelope.message.kind.clone(),
            envelope: envelope.clone(),
            payload: payload.to_vec(),
            retained_at_ms: now_ms,
        };
        let ring = self.rings.entry(frame.kind.clone()).or_default();
        ring.push_back(frame);

        let mut evicted = Vec::new();
        while ring.len() > self.per_kind {
            if let Some(oldest) = ring.pop_front() {
                evicted.push((oldest, Evicted::OverCapacity));
            }
        }
        self.sweep(now_ms, Some(&mut evicted), &envelope.message.kind);
        self.drop_empty_rings();
        evicted
    }

    /// Frames published after `cursor` for `kinds`, oldest first.
    pub fn replay(
        &mut self,
        kinds: &[String],
        cursor: Option<&str>,
        limit: usize,
        now_ms: u64,
    ) -> Page {
        let mut evicted = Vec::new();
        self.sweep(now_ms, Some(&mut evicted), "");
        self.drop_empty_rings();

        let from = cursor.and_then(|cursor| cursor.parse::<u64>().ok());
        let limit = limit.clamp(1, MAX_REPLAY);
        let mut frames = Vec::new();
        let mut stale = false;
        for kind in kinds {
            let Some(ring) = self.rings.get(kind) else {
                continue;
            };
            // A cursor older than the oldest frame we still hold means frames
            // were evicted between the two calls: say so rather than skip them.
            if let (Some(from), Some(oldest)) = (from, ring.front())
                && from.wrapping_add(1) < oldest.sequence
            {
                stale = true;
            }
            for frame in ring
                .iter()
                .filter(|frame| from.is_none_or(|from| frame.sequence > from))
            {
                frames.push(frame.clone());
            }
        }
        frames.sort_by_key(|frame| frame.sequence);
        frames.truncate(limit);
        let next = frames
            .last()
            .map(|frame| frame.sequence.to_string())
            .or_else(|| cursor.map(str::to_owned));
        Page {
            frames,
            next,
            stale,
        }
    }

    /// Drop frames older than the TTL.
    fn sweep(
        &mut self,
        now_ms: u64,
        mut evicted: Option<&mut Vec<(Retained, Evicted)>>,
        only: &str,
    ) {
        let ttl = self.ttl_ms;
        for (kind, ring) in self.rings.iter_mut() {
            if !only.is_empty() && kind != only {
                continue;
            }
            while ring
                .front()
                .is_some_and(|frame| now_ms.saturating_sub(frame.retained_at_ms) > ttl)
            {
                if let Some(frame) = ring.pop_front()
                    && let Some(evicted) = evicted.as_deref_mut()
                {
                    evicted.push((frame, Evicted::Expired));
                }
            }
        }
    }

    fn drop_empty_rings(&mut self) {
        self.rings.retain(|_, ring| !ring.is_empty());
    }

    pub fn depth(&self, kind: &str) -> usize {
        self.rings.get(kind).map(VecDeque::len).unwrap_or(0)
    }
}

/// Hex-encode a payload for a replay reply.
///
/// Control-plane frames carry no payload; data-plane frames can, and a replay
/// that dropped the bytes would be worse than no replay at all.
pub fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// Append-only record of frames that left the window without reaching anyone.
///
/// Not a queue: nothing reads it back into routing. It exists so `matched no
/// live node` stops being a log line and becomes a record an operator can
/// count, and so a crash of this process cannot lose the record of loss - the
/// line is written and flushed before the frame is dropped.
pub struct DeadLetters {
    path: Option<PathBuf>,
}

impl DeadLetters {
    /// `dir` is the hub's state directory; `None` keeps no record.
    pub fn new(dir: Option<&Path>) -> Self {
        Self {
            path: dir.map(|dir| dir.join("dead-letters.jsonl")),
        }
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// Record one frame. Failure is logged, never fatal: routing must not stop
    /// because the sink is unwritable.
    pub fn record(&self, frame: &Retained, evicted: Evicted, now_ms: u64) {
        let Some(path) = &self.path else {
            return;
        };
        let line = serde_json::json!({
            "recorded_at_ms": now_ms,
            "retained_at_ms": frame.retained_at_ms,
            "sequence": frame.sequence,
            "kind": frame.kind,
            "reason": evicted.reason(),
            "ttl_ms": frame.envelope.ttl_ms,
            "producer_id": frame.envelope.producer_id,
            "request_id": frame.envelope.request_id,
            "payload_bytes": frame.payload.len(),
            "envelope": frame.envelope,
        });
        let written = (|| -> std::io::Result<()> {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)?;
            writeln!(file, "{line}")?;
            file.flush()
        })();
        if let Err(error) = written {
            tracing::warn!(path = %path.display(), "could not record a dead letter: {error}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use unibus::{Envelope, Message};

    fn envelope(kind: &str) -> Envelope {
        Envelope::new(
            "installation",
            "producer",
            None,
            "token",
            Message::new(kind, serde_json::json!({})),
        )
    }

    #[test]
    fn a_frame_that_reached_nobody_is_replayable_by_cursor() {
        let mut retention = Retention::new(8, 60_000);
        let evicted = retention.retain(&envelope("confluence.v1"), &[], 1_000);
        assert!(evicted.is_empty(), "nothing is evicted yet");
        let evicted = retention.retain(&envelope("unrelated.v1"), &[], 1_001);
        assert!(evicted.is_empty());

        let page = retention.replay(&["confluence.v1".into()], None, 10, 1_002);
        assert_eq!(page.frames.len(), 1, "only the kinds asked for");
        assert_eq!(page.frames[0].envelope.message.kind, "confluence.v1");
        assert_eq!(page.next.as_deref(), Some("0"));
        assert!(!page.stale);

        let page = retention.replay(&["confluence.v1".into()], page.next.as_deref(), 10, 1_003);
        assert!(page.frames.is_empty(), "a cursor consumes what it returned");
    }

    #[test]
    fn the_ring_is_bounded_and_says_when_it_lost_the_thread() {
        let mut retention = Retention::new(2, 60_000);
        for _ in 0..4 {
            retention.retain(&envelope("k.v1"), &[], 1_000);
        }
        assert_eq!(retention.depth("k.v1"), 2);
        // Replaying from the very beginning must admit the loss, not skip it.
        let page = retention.replay(&["k.v1".into()], Some("0"), 10, 1_000);
        assert!(page.stale, "frames 0..2 fell out of the window");
        assert_eq!(page.frames.len(), 2, "the window still holds two frames");
        assert_eq!(page.frames[0].sequence, 2);
        assert_eq!(page.frames[1].sequence, 3);
    }

    #[test]
    fn an_aged_frame_is_evicted_and_reported() {
        let mut retention = Retention::new(8, 1_000);
        retention.retain(&envelope("k.v1"), &[], 1_000);
        let evicted = retention.retain(&envelope("k.v1"), &[], 3_000);
        assert_eq!(evicted.len(), 1);
        assert_eq!(evicted[0].1, Evicted::Expired);
        assert_eq!(retention.depth("k.v1"), 1);
        let page = retention.replay(&["k.v1".into()], None, 10, 3_001);
        assert_eq!(page.frames.len(), 1);
        assert_eq!(page.frames[0].sequence, 1);
    }

    #[test]
    fn capacity_eviction_is_reported_in_order() {
        let mut retention = Retention::new(1, 60_000);
        retention.retain(&envelope("k.v1"), &[], 1_000);
        let evicted = retention.retain(&envelope("k.v1"), &[], 1_001);
        assert_eq!(evicted.len(), 1);
        assert_eq!(evicted[0].0.sequence, 0);
        assert_eq!(evicted[0].1, Evicted::OverCapacity);
    }

    #[test]
    fn a_replay_page_is_bounded() {
        let mut retention = Retention::new(64, 60_000);
        for _ in 0..10 {
            retention.retain(&envelope("k.v1"), &[], 1_000);
        }
        let page = retention.replay(&["k.v1".into()], None, 3, 1_001);
        assert_eq!(page.frames.len(), 3);
        assert_eq!(page.next.as_deref(), Some("2"));
    }

    #[test]
    fn dead_letters_are_append_only_lines() {
        let dir = std::env::temp_dir().join(format!(
            "unibus-dead-letters-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_nanos())
                .unwrap_or_default()
        ));
        let sink = DeadLetters::new(Some(&dir));
        let mut retention = Retention::new(1, 60_000);
        retention.retain(&envelope("k.v1"), b"one", 1_000);
        for (frame, evicted) in retention.retain(&envelope("k.v1"), b"two", 1_001) {
            sink.record(&frame, evicted, 1_002);
        }
        sink.record(
            &retention.replay(&["k.v1".into()], None, 1, 1_003).frames[0],
            Evicted::Expired,
            1_004,
        );

        let body = std::fs::read_to_string(sink.path().expect("a sink path")).expect("read");
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(lines.len(), 2, "one line per frame, appended");
        let first: serde_json::Value = serde_json::from_str(lines[0]).expect("json line");
        assert_eq!(first["kind"], "k.v1");
        assert_eq!(first["reason"], "over_capacity");
        assert_eq!(
            first["payload_bytes"], 3,
            "the evicted frame was the one with a payload"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn no_state_directory_records_nothing() {
        let sink = DeadLetters::new(None);
        assert!(sink.path().is_none());
        // Recording must be a no-op rather than a panic or a stray file.
        let mut retention = Retention::new(1, 60_000);
        retention.retain(&envelope("k.v1"), &[], 1_000);
        for (frame, evicted) in retention.retain(&envelope("k.v1"), &[], 1_001) {
            sink.record(&frame, evicted, 1_002);
        }
    }
}
