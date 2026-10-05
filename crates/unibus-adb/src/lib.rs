//! Ordered, bounded ADB smart-socket transport. ADB retains USB/auth ownership.
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
pub mod inventory;

pub const KIND: &str = "adb.socket.v1";
pub const MAX_CHUNK: usize = 16 * 1024;
pub const MAX_SESSIONS: usize = 16;
pub const QUEUE_DEPTH: usize = 32;
pub const WINDOW: u64 = 16;

#[derive(Default)]
pub struct SendWindow {
    sent: std::sync::atomic::AtomicU64,
    acknowledged: std::sync::atomic::AtomicU64,
}
impl SendWindow {
    pub fn drained(&self) -> bool {
        self.acknowledged.load(std::sync::atomic::Ordering::Acquire)
            == self.sent.load(std::sync::atomic::Ordering::Acquire)
    }
    pub fn full(&self) -> bool {
        let acknowledged = self.acknowledged.load(std::sync::atomic::Ordering::Acquire);
        self.sent.load(std::sync::atomic::Ordering::Acquire) - acknowledged >= WINDOW
    }
    pub fn sent(&self, sequence: u64) {
        self.sent
            .store(sequence, std::sync::atomic::Ordering::Release);
    }
    pub fn acknowledge(&self, sequence: u64) -> Result<()> {
        use std::sync::atomic::Ordering;
        ensure!(
            sequence <= self.sent.load(Ordering::Acquire)
                && sequence == self.acknowledged.load(Ordering::Acquire) + 1,
            "invalid ADB ACK"
        );
        self.acknowledged.store(sequence, Ordering::Release);
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Operation {
    Open,
    Data,
    Ack,
    Eof,
    Close,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Packet {
    pub session: String,
    pub operation: Operation,
    pub sequence: u64,
}

impl Packet {
    pub fn validate(&self, payload: &[u8]) -> Result<()> {
        ensure!(
            !self.session.is_empty()
                && self.session.len() <= 128
                && self
                    .session
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-'),
            "invalid ADB session id"
        );
        ensure!(payload.len() <= MAX_CHUNK, "ADB chunk too large");
        match self.operation {
            Operation::Open => ensure!(self.sequence == 0 && payload.is_empty(), "invalid open"),
            Operation::Data => ensure!(self.sequence > 0 && !payload.is_empty(), "invalid data"),
            Operation::Eof | Operation::Ack => {
                ensure!(self.sequence > 0 && payload.is_empty(), "invalid EOF/ACK")
            }
            Operation::Close => ensure!(payload.is_empty(), "invalid close"),
        }
        Ok(())
    }
}

/// Reject duplicates/gaps instead of replaying potentially destructive ADB bytes.
pub struct ReceiveOrder {
    next: u64,
    eof: bool,
}
impl Default for ReceiveOrder {
    fn default() -> Self {
        Self::new()
    }
}
impl ReceiveOrder {
    pub fn new() -> Self {
        Self {
            next: 1,
            eof: false,
        }
    }
    pub fn accept(&mut self, packet: &Packet) -> Result<()> {
        ensure!(
            !self.eof && packet.sequence == self.next,
            "ADB sequence gap/duplicate or data after EOF"
        );
        ensure!(
            matches!(packet.operation, Operation::Data | Operation::Eof),
            "unexpected ADB operation"
        );
        self.next = self
            .next
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("sequence overflow"))?;
        self.eof = packet.operation == Operation::Eof;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn send_window_is_bounded_and_acknowledgements_cannot_skip_or_advance_unsent_data() {
        let window = SendWindow::default();
        assert!(window.drained());
        window.sent(WINDOW);
        assert!(window.full());
        assert!(window.acknowledge(2).is_err());
        window.acknowledge(1).unwrap();
        assert!(!window.full());
        assert!(window.acknowledge(1).is_err());
        for sequence in 2..=WINDOW {
            window.acknowledge(sequence).unwrap();
        }
        assert!(window.drained());
        assert!(window.acknowledge(WINDOW + 1).is_err());
    }
    #[test]
    fn validates_frames_and_bounds() {
        let mut p = Packet {
            session: "session-1".into(),
            operation: Operation::Open,
            sequence: 0,
        };
        p.validate(&[]).unwrap();
        assert!(p.validate(&[1]).is_err());
        p.operation = Operation::Data;
        p.sequence = 1;
        p.validate(&[1]).unwrap();
        assert!(p.validate(&[]).is_err());
        assert!(p.validate(&vec![0; MAX_CHUNK + 1]).is_err());
        p.session = "../escape".into();
        assert!(p.validate(&[1]).is_err());
    }
    #[test]
    fn sequence_and_half_close_are_strict() {
        let mut order = ReceiveOrder::new();
        let mut p = Packet {
            session: "s".into(),
            operation: Operation::Data,
            sequence: 1,
        };
        order.accept(&p).unwrap();
        assert!(order.accept(&p).is_err());
        p.sequence = 3;
        assert!(order.accept(&p).is_err());
        p.sequence = 2;
        p.operation = Operation::Eof;
        order.accept(&p).unwrap();
        p.sequence = 3;
        p.operation = Operation::Data;
        assert!(order.accept(&p).is_err());
    }
}
