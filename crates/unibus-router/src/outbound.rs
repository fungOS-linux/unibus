use std::collections::VecDeque;
use std::net::TcpStream;
use std::sync::{Arc, Condvar, Mutex};
use std::thread;

use anyhow::{Result, anyhow};
use unibus::envelope::{Envelope, now_ms};
use unibus::framing::write_frame;
use unibus::qos::Qos;

const MAX_QUEUED_FRAMES: usize = 128;
const MAX_QUEUED_BYTES: usize = 32 * 1024 * 1024;

#[derive(Clone)]
struct QueuedFrame {
    envelope: Envelope,
    payload: Vec<u8>,
    qos: Qos,
    bytes: usize,
}

impl QueuedFrame {
    fn new(mut envelope: Envelope, payload: &[u8]) -> Self {
        envelope.token.clear();
        let qos = Qos::of_kind(&envelope.message.kind);
        let bytes = payload.len() + serde_json::to_vec(&envelope).map_or(0, |json| json.len()) + 4;
        Self {
            envelope,
            payload: payload.to_vec(),
            qos,
            bytes,
        }
    }

    fn snapshot_key(&self) -> (&str, &str, Option<&str>, Option<&str>) {
        (
            &self.envelope.producer_id,
            &self.envelope.message.kind,
            self.envelope.destination.as_deref(),
            self.envelope.target_screen.as_deref(),
        )
    }

    fn stale(&self, now: u64) -> bool {
        let ttl = self
            .envelope
            .ttl_ms
            .unwrap_or(unibus::envelope::DEFAULT_TTL_MS);
        now.saturating_sub(self.envelope.issued_at_unix_ms) > ttl
    }
}

#[derive(Default)]
struct QueueCore {
    frames: VecDeque<QueuedFrame>,
    bytes: usize,
    closed: bool,
}

impl QueueCore {
    fn fits(&self, frame: &QueuedFrame) -> bool {
        self.frames.len() < MAX_QUEUED_FRAMES
            && self.bytes.saturating_add(frame.bytes) <= MAX_QUEUED_BYTES
    }

    fn remove_at(&mut self, index: usize) {
        if let Some(removed) = self.frames.remove(index) {
            self.bytes = self.bytes.saturating_sub(removed.bytes);
        }
    }

    fn evict_replaceable(&mut self) -> bool {
        let Some(index) = self
            .frames
            .iter()
            .position(|frame| frame.qos != Qos::Command)
        else {
            return false;
        };
        self.remove_at(index);
        true
    }

    fn enqueue(&mut self, frame: QueuedFrame) -> Result<()> {
        if self.closed {
            return Err(anyhow!("outbound connection is closed"));
        }
        if frame.bytes > MAX_QUEUED_BYTES {
            return Err(anyhow!("outbound frame exceeds queue byte limit"));
        }

        if frame.qos == Qos::Snapshot {
            let key = frame.snapshot_key();
            if let Some(index) = self
                .frames
                .iter()
                .position(|queued| queued.qos == Qos::Snapshot && queued.snapshot_key() == key)
            {
                self.remove_at(index);
            }
        }

        match frame.qos {
            Qos::Command if !self.fits(&frame) => {
                return Err(anyhow!("reliable outbound command queue is full"));
            }
            Qos::Stream | Qos::Snapshot => {
                while !self.fits(&frame) && self.evict_replaceable() {}
                if !self.fits(&frame) {
                    return Err(anyhow!("outbound QoS queue has no replaceable capacity"));
                }
            }
            Qos::Command => {}
        }
        self.bytes += frame.bytes;
        self.frames.push_back(frame);
        Ok(())
    }

    fn pop(&mut self) -> Option<QueuedFrame> {
        while let Some(frame) = self.frames.pop_front() {
            self.bytes = self.bytes.saturating_sub(frame.bytes);
            if frame.qos != Qos::Command && frame.stale(now_ms()) {
                continue;
            }
            return Some(frame);
        }
        None
    }
}

pub(crate) struct OutboundScheduler {
    state: Arc<(Mutex<QueueCore>, Condvar)>,
}

impl OutboundScheduler {
    pub(crate) fn spawn(mut stream: TcpStream) -> Result<Self> {
        let state = Arc::new((Mutex::new(QueueCore::default()), Condvar::new()));
        let worker_state = state.clone();
        thread::Builder::new()
            .name("unibus-hub-writer".into())
            .spawn(move || {
                loop {
                    let frame = {
                        let (lock, ready) = &*worker_state;
                        let mut queue = match lock.lock() {
                            Ok(queue) => queue,
                            Err(_) => return,
                        };
                        while queue.frames.is_empty() && !queue.closed {
                            queue = match ready.wait(queue) {
                                Ok(queue) => queue,
                                Err(_) => return,
                            };
                        }
                        if queue.closed {
                            return;
                        }
                        queue.pop()
                    };
                    let Some(frame) = frame else { continue };
                    if write_frame(&mut stream, &frame.envelope, &frame.payload).is_err() {
                        let (lock, ready) = &*worker_state;
                        if let Ok(mut queue) = lock.lock() {
                            queue.closed = true;
                            queue.frames.clear();
                            queue.bytes = 0;
                            ready.notify_all();
                        }
                        return;
                    }
                }
            })
            .map_err(|error| anyhow!("start outbound writer: {error}"))?;
        Ok(Self { state })
    }

    pub(crate) fn push(&self, envelope: &Envelope, payload: &[u8]) -> Result<()> {
        let frame = QueuedFrame::new(envelope.clone(), payload);
        let (lock, ready) = &*self.state;
        let mut queue = lock
            .lock()
            .map_err(|_| anyhow!("outbound queue mutex poisoned"))?;
        queue.enqueue(frame)?;
        ready.notify_one();
        Ok(())
    }
}

impl Drop for OutboundScheduler {
    fn drop(&mut self) {
        let (lock, ready) = &*self.state;
        if let Ok(mut queue) = lock.lock() {
            queue.closed = true;
            ready.notify_all();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use unibus::envelope::Message;

    fn frame(kind: &str, payload_bytes: usize) -> QueuedFrame {
        let mut envelope = Envelope::new(
            "home",
            "source",
            None,
            "secret",
            Message::new(kind, serde_json::Value::Null),
        );
        envelope.payload_bytes = payload_bytes;
        QueuedFrame::new(envelope, &vec![0; payload_bytes])
    }

    #[test]
    fn snapshots_replace_the_same_key() {
        let mut queue = QueueCore::default();
        queue.enqueue(frame("presence.snapshot", 0)).unwrap();
        queue.enqueue(frame("presence.snapshot", 0)).unwrap();
        assert_eq!(queue.frames.len(), 1);
    }

    #[test]
    fn streams_evict_replaceable_work_but_not_commands() {
        let mut queue = QueueCore::default();
        for _ in 0..MAX_QUEUED_FRAMES {
            queue.enqueue(frame("audio.playback_chunk", 0)).unwrap();
        }
        queue.enqueue(frame("audio.playback_chunk", 0)).unwrap();
        assert_eq!(queue.frames.len(), MAX_QUEUED_FRAMES);

        let mut commands = QueueCore::default();
        for _ in 0..MAX_QUEUED_FRAMES {
            commands.enqueue(frame("canvas.command", 0)).unwrap();
        }
        assert!(commands.enqueue(frame("canvas.command", 0)).is_err());
        assert!(commands.enqueue(frame("audio.playback_chunk", 0)).is_err());
    }

    #[test]
    fn credentials_are_removed_before_queueing() {
        let queued = frame("canvas.command", 0);
        assert!(queued.envelope.token.is_empty());
    }
}
