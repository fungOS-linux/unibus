//! Bounded datagram framing for private Holodeck room relays.
//! ENet remains owned by Holodeck. No media payload enters a retained bus stream.
use anyhow::{Result, ensure};
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

#[cfg(feature = "iroh")]
pub mod iroh_runtime;
pub mod room_directory;
pub mod runtime;

#[derive(Clone)]
pub enum RoomConnection {
    Quic(quinn::Connection),
    #[cfg(feature = "iroh")]
    Iroh(iroh::endpoint::Connection),
}

impl RoomConnection {
    fn max_datagram_size(&self) -> Option<usize> {
        match self {
            Self::Quic(c) => c.max_datagram_size(),
            #[cfg(feature = "iroh")]
            Self::Iroh(c) => c.max_datagram_size(),
        }
    }
    fn send_datagram(&self, bytes: Vec<u8>) -> Result<()> {
        match self {
            Self::Quic(c) => c.send_datagram(bytes.into())?,
            #[cfg(feature = "iroh")]
            Self::Iroh(c) => c.send_datagram(bytes.into())?,
        }
        Ok(())
    }
    pub async fn read_datagram(&self) -> Result<bytes::Bytes> {
        Ok(match self {
            Self::Quic(c) => c.read_datagram().await?,
            #[cfg(feature = "iroh")]
            Self::Iroh(c) => c.read_datagram().await?,
        })
    }
    pub async fn open_bi(
        &self,
    ) -> Result<(
        Box<dyn tokio::io::AsyncWrite + Unpin + Send>,
        Box<dyn tokio::io::AsyncRead + Unpin + Send>,
    )> {
        match self {
            Self::Quic(c) => {
                let (tx, rx) = c.open_bi().await?;
                Ok((Box::new(tx), Box::new(rx)))
            }
            #[cfg(feature = "iroh")]
            Self::Iroh(c) => {
                let (tx, rx) = c.open_bi().await?;
                Ok((Box::new(tx), Box::new(rx)))
            }
        }
    }
    pub async fn accept_bi(
        &self,
    ) -> Result<(
        Box<dyn tokio::io::AsyncWrite + Unpin + Send>,
        Box<dyn tokio::io::AsyncRead + Unpin + Send>,
    )> {
        match self {
            Self::Quic(c) => {
                let (tx, rx) = c.accept_bi().await?;
                Ok((Box::new(tx), Box::new(rx)))
            }
            #[cfg(feature = "iroh")]
            Self::Iroh(c) => {
                let (tx, rx) = c.accept_bi().await?;
                Ok((Box::new(tx), Box::new(rx)))
            }
        }
    }
}

pub const MAX_CLIENTS: usize = 32;
const CHUNK: usize = 900;
const HEADER: usize = 16;
const MAX_PENDING: usize = 128;
const MAX_BYTES: usize = holodeck_discovery::MAX_ROOM_PACKET_BYTES;
const REASSEMBLY_TTL: Duration = Duration::from_millis(250);

pub fn fragments(client: u64, sequence: u32, bytes: &[u8]) -> Result<Vec<Vec<u8>>> {
    ensure!(
        client != 0 && !bytes.is_empty() && bytes.len() <= MAX_BYTES,
        "invalid relay datagram"
    );
    let count = bytes.len().div_ceil(CHUNK);
    Ok(bytes
        .chunks(CHUNK)
        .enumerate()
        .map(|(index, part)| {
            let mut packet = Vec::with_capacity(HEADER + part.len());
            packet.extend_from_slice(&client.to_be_bytes());
            packet.extend_from_slice(&sequence.to_be_bytes());
            packet.push(index as u8);
            packet.push(count as u8);
            packet.extend_from_slice(&(bytes.len() as u16).to_be_bytes());
            packet.extend_from_slice(part);
            packet
        })
        .collect())
}

struct Pending {
    started: Instant,
    length: usize,
    parts: Vec<Option<Vec<u8>>>,
}

#[derive(Default)]
pub struct Reassembler {
    pending: BTreeMap<(u64, u32), Pending>,
    completed: BTreeMap<(u64, u32), Instant>,
}

impl Reassembler {
    pub fn accept(&mut self, packet: &[u8], now: Instant) -> Result<Option<(u64, Vec<u8>)>> {
        self.pending
            .retain(|_, p| now.duration_since(p.started) < REASSEMBLY_TTL);
        self.completed
            .retain(|_, t| now.duration_since(*t) < REASSEMBLY_TTL);
        ensure!(
            packet.len() > HEADER && packet.len() <= HEADER + CHUNK,
            "bad fragment length"
        );
        let client = u64::from_be_bytes(packet[..8].try_into()?);
        let seq = u32::from_be_bytes(packet[8..12].try_into()?);
        let index = packet[12] as usize;
        let count = packet[13] as usize;
        let length = u16::from_be_bytes(packet[14..16].try_into()?) as usize;
        ensure!(
            client > 0
                && length > 0
                && length <= MAX_BYTES
                && count == length.div_ceil(CHUNK)
                && index < count,
            "bad fragment header"
        );
        let expected = if index + 1 == count {
            length - index * CHUNK
        } else {
            CHUNK
        };
        ensure!(packet.len() - HEADER == expected, "bad fragment payload");
        let key = (client, seq);
        if self.completed.contains_key(&key) {
            return Ok(None);
        }
        ensure!(
            self.pending.contains_key(&key) || self.pending.len() < MAX_PENDING,
            "reassembly capacity reached"
        );
        let p = self.pending.entry(key).or_insert_with(|| Pending {
            started: now,
            length,
            parts: (0..count).map(|_| None).collect(),
        });
        ensure!(
            p.length == length && p.parts.len() == count,
            "conflicting fragment metadata"
        );
        if let Some(old) = &p.parts[index] {
            ensure!(
                old.as_slice() == &packet[HEADER..],
                "conflicting fragment contents"
            );
        }
        p.parts[index] = Some(packet[HEADER..].to_vec());
        if p.parts.iter().any(Option::is_none) {
            return Ok(None);
        }
        let p = self.pending.remove(&key).expect("entry exists");
        if self.completed.len() >= MAX_PENDING {
            self.completed.clear();
        }
        self.completed.insert(key, now);
        Ok(Some((
            client,
            p.parts.into_iter().flatten().flatten().collect(),
        )))
    }
}

pub fn send_datagram(
    connection: &RoomConnection,
    client: u64,
    sequence: u32,
    bytes: &[u8],
) -> Result<()> {
    for part in fragments(client, sequence, bytes)? {
        ensure!(
            connection
                .max_datagram_size()
                .is_some_and(|max| part.len() <= max),
            "QUIC datagram MTU unavailable or too small"
        );
        connection.send_datagram(part)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reordered_duplicate_fragments_preserve_datagram() {
        let payload = vec![19; MAX_BYTES];
        let parts = fragments(7, 1, &payload).unwrap();
        let mut r = Reassembler::default();
        let now = Instant::now();
        assert!(r.accept(&parts[0], now).unwrap().is_none());
        assert!(r.accept(&parts[0], now).unwrap().is_none());
        let mut result = None;
        for p in parts.iter().rev() {
            if let Some(v) = r.accept(p, now).unwrap() {
                result = Some(v);
            }
        }
        assert_eq!(result, Some((7, payload)));
        assert!(r.accept(&parts[0], now).unwrap().is_none());
    }
    #[test]
    fn expiry_and_malformed_fragments_fail_closed() {
        let parts = fragments(1, 2, &vec![0; 1000]).unwrap();
        let now = Instant::now();
        let mut r = Reassembler::default();
        r.accept(&parts[0], now).unwrap();
        assert!(
            r.accept(&parts[1], now + Duration::from_secs(1))
                .unwrap()
                .is_none()
        );
        assert!(fragments(0, 0, b"x").is_err());
        assert!(fragments(1, 0, &vec![0; MAX_BYTES + 1]).is_err());
        assert!(r.accept(b"bad", now + Duration::from_secs(1)).is_err());
    }
}
