//! FIX market-data decoding and normalization.
//!
//! FIX stays at this edge. Downstream consumers receive stable, venue-neutral
//! order-book and trade contracts and never need to understand tag numbers.

use std::collections::{BTreeMap, HashMap};

use anyhow::{Context, Result, bail, ensure};
use serde::Serialize;

pub const ORDER_BOOK_KIND: &str = "market.order_book.v1";
pub const TRADE_KIND: &str = "market.trade.v1";

/// Incremental FIX wire decoder. It tolerates arbitrary TCP fragmentation and
/// yields complete messages only after the CheckSum(10) field arrives.
#[derive(Default)]
pub struct FrameDecoder {
    buffer: Vec<u8>,
}

impl FrameDecoder {
    pub fn finish(&self) -> Result<()> {
        ensure!(
            self.buffer.is_empty(),
            "truncated FIX frame at end of source"
        );
        Ok(())
    }

    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<Vec<u8>>> {
        self.buffer.extend_from_slice(bytes);
        ensure!(
            self.buffer.len() <= 16 * 1024 * 1024,
            "FIX receive buffer exceeded 16 MiB"
        );
        let mut frames = Vec::new();
        loop {
            let Some(start) = find_bytes(&self.buffer, b"8=FIX") else {
                if self.buffer.len() > 4 {
                    self.buffer.drain(..self.buffer.len() - 4);
                }
                break;
            };
            if start > 0 {
                self.buffer.drain(..start);
            }
            let delimiter = if self.buffer.iter().take(32).any(|byte| *byte == 1) {
                1
            } else {
                b'|'
            };
            let marker = [delimiter, b'1', b'0', b'='];
            let Some(checksum) = find_bytes(&self.buffer, &marker) else {
                break;
            };
            let Some(end_offset) = self.buffer[checksum + marker.len()..]
                .iter()
                .position(|byte| *byte == delimiter || *byte == b'\n')
            else {
                break;
            };
            let end = checksum + marker.len() + end_offset + 1;
            frames.push(self.buffer.drain(..end).collect());
            while matches!(self.buffer.first(), Some(b'\r' | b'\n')) {
                self.buffer.remove(0);
            }
        }
        Ok(frames)
    }
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

#[derive(Debug, Clone, PartialEq)]
pub struct FixMessage {
    fields: Vec<(u32, String)>,
}

impl FixMessage {
    pub fn parse(input: &[u8]) -> Result<Self> {
        let delimiter = if input.contains(&1) { 1 } else { b'|' };
        validate_checksum(input, delimiter)?;
        let mut fields = Vec::new();
        for raw in input.split(|byte| *byte == delimiter) {
            if raw.is_empty() || raw == b"\n" || raw == b"\r\n" {
                continue;
            }
            let raw = raw.strip_suffix(b"\r").unwrap_or(raw);
            let Some(split) = raw.iter().position(|byte| *byte == b'=') else {
                bail!("FIX field has no '=' separator")
            };
            let tag = std::str::from_utf8(&raw[..split])?.parse::<u32>()?;
            let value = std::str::from_utf8(&raw[split + 1..])?.to_owned();
            fields.push((tag, value));
        }
        ensure!(!fields.is_empty(), "empty FIX message");
        Ok(Self { fields })
    }

    pub fn get(&self, tag: u32) -> Option<&str> {
        self.fields
            .iter()
            .find(|(key, _)| *key == tag)
            .map(|(_, value)| value.as_str())
    }

    fn entries(&self) -> Vec<HashMap<u32, &str>> {
        // Incremental groups normally begin with action (279); snapshots
        // begin with type (269). Also accept type-first replay captures.
        let delimiter = if self.get(35) == Some("X") {
            self.fields
                .iter()
                .find(|(tag, _)| *tag == 269 || *tag == 279)
                .map(|(tag, _)| *tag)
                .unwrap_or(269)
        } else {
            269
        };
        let mut entries = Vec::new();
        let mut current = None::<HashMap<u32, &str>>;
        for (tag, value) in &self.fields {
            if *tag == delimiter {
                if let Some(entry) = current.take() {
                    entries.push(entry);
                }
                current = Some(HashMap::new());
            }
            if let Some(entry) = current.as_mut() {
                entry.insert(*tag, value.as_str());
            }
        }
        if let Some(entry) = current {
            entries.push(entry);
        }
        entries
    }
}

fn validate_checksum(input: &[u8], delimiter: u8) -> Result<()> {
    let marker = [delimiter, b'1', b'0', b'='];
    let Some(position) = find_bytes(input, &marker) else {
        return Ok(());
    };
    let value_start = position + marker.len();
    let value_end = input[value_start..]
        .iter()
        .position(|byte| *byte == delimiter || *byte == b'\n')
        .map(|offset| value_start + offset)
        .ok_or_else(|| anyhow::anyhow!("unterminated CheckSum(10)"))?;
    let declared = std::str::from_utf8(&input[value_start..value_end])?.parse::<u8>()?;
    let calculated = input[..=position]
        .iter()
        .fold(0_u8, |sum, byte| sum.wrapping_add(*byte));
    ensure!(
        declared == calculated,
        "FIX checksum mismatch: declared {declared:03}, calculated {calculated:03}"
    );
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BookLevel {
    pub price: f64,
    pub size: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub orders: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct OrderBook {
    pub symbol: String,
    pub venue: String,
    pub sequence: u64,
    pub emitted_at_ms: u64,
    pub bids: Vec<BookLevel>,
    pub asks: Vec<BookLevel>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Trade {
    pub symbol: String,
    pub venue: String,
    pub sequence: u64,
    pub emitted_at_ms: u64,
    pub time: u64,
    pub price: f64,
    pub size: f64,
    pub side: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum MarketEvent {
    Book(OrderBook),
    Trade(Trade),
}

#[derive(Debug, Clone)]
struct Entry {
    side: char,
    price: f64,
    size: f64,
    orders: Option<u64>,
}

#[derive(Default)]
pub struct Normalizer {
    books: HashMap<(String, String), HashMap<String, Entry>>,
    sequence: u64,
    depth: usize,
    default_venue: String,
}

impl Normalizer {
    pub fn new(default_venue: impl Into<String>, depth: usize) -> Result<Self> {
        ensure!(depth > 0 && depth <= 50, "book depth must be 1..=50");
        let default_venue = default_venue.into();
        ensure!(
            !default_venue.trim().is_empty(),
            "default venue must not be empty"
        );
        Ok(Self {
            books: HashMap::new(),
            sequence: 0,
            depth,
            default_venue,
        })
    }

    pub fn apply(&mut self, message: &FixMessage, emitted_at_ms: u64) -> Result<Vec<MarketEvent>> {
        let message_type = message
            .get(35)
            .ok_or_else(|| anyhow::anyhow!("FIX message missing MsgType(35)"))?;
        if message_type != "W" && message_type != "X" {
            return Ok(Vec::new());
        }
        let fallback_symbol = message.get(55).map(str::to_owned);
        let venue = message.get(207).unwrap_or(&self.default_venue).to_owned();
        let entries = message.entries();
        ensure!(
            !entries.is_empty(),
            "market-data message has no MDEntryType(269) groups"
        );
        if message_type == "W" {
            let symbol = fallback_symbol
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("snapshot missing Symbol(55)"))?;
            self.books.remove(&(venue.clone(), symbol.to_owned()));
        }
        let mut touched = BTreeMap::<String, ()>::new();
        let mut events = Vec::new();
        for (index, fields) in entries.into_iter().enumerate() {
            let symbol = fields
                .get(&55)
                .copied()
                .or(fallback_symbol.as_deref())
                .ok_or_else(|| anyhow::anyhow!("entry missing Symbol(55)"))?
                .to_owned();
            let kind = fields.get(&269).copied().unwrap_or_default();
            let action = fields.get(&279).copied().unwrap_or("0");
            ensure!(
                matches!(action, "0" | "1" | "2"),
                "unsupported MDUpdateAction(279): {action}"
            );
            if action == "2" {
                ensure!(
                    matches!(kind, "" | "0" | "1"),
                    "unsupported delete entry type {kind}"
                );
                let id = fields.get(&278).context("delete missing MDEntryID(278)")?;
                self.books
                    .entry((venue.clone(), symbol.clone()))
                    .or_default()
                    .remove(*id);
                touched.insert(symbol, ());
                continue;
            }
            if kind != "0" && kind != "1" && kind != "2" {
                continue;
            }
            let price = parse_number(&fields, 270)?;
            let size = parse_number(&fields, 271)?;
            ensure!(
                price > 0.0 && size >= 0.0,
                "price must be positive and size nonnegative"
            );
            if kind == "2" {
                self.sequence += 1;
                events.push(MarketEvent::Trade(Trade {
                    symbol,
                    venue: venue.clone(),
                    sequence: self.sequence,
                    emitted_at_ms,
                    time: emitted_at_ms,
                    price,
                    size,
                    side: fields
                        .get(&5797)
                        .copied()
                        .map(side_name)
                        .unwrap_or("unknown")
                        .to_owned(),
                }));
                continue;
            }
            if kind != "0" && kind != "1" {
                continue;
            }
            let id = fields
                .get(&278)
                .copied()
                .map(str::to_owned)
                .unwrap_or_else(|| format!("{kind}:{price}:{index}"));
            ensure!(
                message_type != "X" || fields.contains_key(&278),
                "incremental book update requires MDEntryID(278)"
            );
            let book = self
                .books
                .entry((venue.clone(), symbol.clone()))
                .or_default();
            let orders = fields
                .get(&346)
                .map(|value| value.parse::<u64>())
                .transpose()?;
            book.insert(
                id,
                Entry {
                    side: kind.chars().next().unwrap(),
                    price,
                    size,
                    orders,
                },
            );
            touched.insert(symbol, ());
        }
        for symbol in touched.keys() {
            self.sequence += 1;
            let entries = self
                .books
                .get(&(venue.clone(), symbol.clone()))
                .expect("touched book exists");
            let mut bids = levels(
                entries.values().filter(|entry| entry.side == '0'),
                true,
                self.depth,
            );
            let mut asks = levels(
                entries.values().filter(|entry| entry.side == '1'),
                false,
                self.depth,
            );
            // Keep output deterministic even for equal prices.
            bids.truncate(self.depth);
            asks.truncate(self.depth);
            events.push(MarketEvent::Book(OrderBook {
                symbol: symbol.clone(),
                venue: venue.clone(),
                sequence: self.sequence,
                emitted_at_ms,
                bids,
                asks,
            }));
        }
        Ok(events)
    }
}

fn parse_number(fields: &HashMap<u32, &str>, tag: u32) -> Result<f64> {
    let value: f64 = fields
        .get(&tag)
        .ok_or_else(|| anyhow::anyhow!("entry missing tag {tag}"))?
        .parse()?;
    ensure!(value.is_finite(), "non-finite numeric tag {tag}");
    Ok(value)
}

fn side_name(value: &str) -> &'static str {
    match value {
        "1" => "buy",
        "2" => "sell",
        _ => "unknown",
    }
}

fn levels<'a>(
    entries: impl Iterator<Item = &'a Entry>,
    descending: bool,
    depth: usize,
) -> Vec<BookLevel> {
    let mut out: Vec<_> = entries
        .map(|entry| BookLevel {
            price: entry.price,
            size: entry.size,
            orders: entry.orders,
        })
        .collect();
    out.sort_by(|a, b| {
        if descending {
            b.price
                .total_cmp(&a.price)
                .then_with(|| a.size.total_cmp(&b.size))
                .then(a.orders.cmp(&b.orders))
        } else {
            a.price
                .total_cmp(&b.price)
                .then_with(|| a.size.total_cmp(&b.size))
                .then(a.orders.cmp(&b.orders))
        }
    });
    out.truncate(depth);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn action_first_groups_and_sparse_deletes_work() {
        let mut normalizer = Normalizer::new("XNAS", 10).unwrap();
        normalizer
            .apply(
                &FixMessage::parse(b"35=W|55=AAPL|269=0|278=b1|270=100|271=2|").unwrap(),
                1,
            )
            .unwrap();
        let update =
            FixMessage::parse(b"35=X|55=AAPL|268=2|279=2|278=b1|279=0|269=1|278=a1|270=101|271=3|")
                .unwrap();
        let events = normalizer.apply(&update, 2).unwrap();
        let MarketEvent::Book(book) = &events[0] else {
            panic!("book")
        };
        assert!(book.bids.is_empty());
        assert_eq!(book.asks.len(), 1);
    }

    #[test]
    fn same_symbol_at_different_venues_is_not_merged() {
        let mut normalizer = Normalizer::new("UNKNOWN", 10).unwrap();
        for venue in ["XNAS", "XNYS"] {
            let message = format!("35=W|55=AAPL|207={venue}|269=0|278=b1|270=100|271=2|");
            normalizer
                .apply(&FixMessage::parse(message.as_bytes()).unwrap(), 1)
                .unwrap();
        }
        normalizer
            .apply(
                &FixMessage::parse(b"35=X|55=AAPL|207=XNAS|279=2|278=b1|").unwrap(),
                2,
            )
            .unwrap();
        let events = normalizer
            .apply(
                &FixMessage::parse(b"35=X|55=AAPL|207=XNYS|279=0|269=1|278=a1|270=101|271=3|")
                    .unwrap(),
                3,
            )
            .unwrap();
        let MarketEvent::Book(book) = &events[0] else {
            panic!("book")
        };
        assert_eq!(book.bids.len(), 1);
    }

    #[test]
    fn rejects_nonfinite_values_bad_orders_and_unknown_actions() {
        for frame in [
            "35=W|55=AAPL|269=0|270=NaN|271=2|",
            "35=W|55=AAPL|269=0|270=100|271=inf|",
            "35=W|55=AAPL|269=0|270=100|271=-1|",
            "35=W|55=AAPL|269=0|270=100|271=2|346=not-a-number|",
            "35=X|55=AAPL|279=9|269=0|278=b1|270=100|271=2|",
        ] {
            assert!(
                Normalizer::new("XNAS", 10)
                    .unwrap()
                    .apply(&FixMessage::parse(frame.as_bytes()).unwrap(), 1)
                    .is_err()
            );
        }
    }

    #[test]
    fn rejects_bad_checksum_and_reports_truncated_source() {
        let body = b"8=FIX.4.4\x0135=0\x01";
        let checksum = body.iter().fold(0u8, |sum, byte| sum.wrapping_add(*byte));
        let mut valid = body.to_vec();
        valid.extend_from_slice(format!("10={checksum:03}\x01").as_bytes());
        assert!(FixMessage::parse(&valid).is_ok());
        let mut invalid = body.to_vec();
        invalid.extend_from_slice(format!("10={:03}\x01", checksum.wrapping_add(1)).as_bytes());
        assert!(FixMessage::parse(&invalid).is_err());
        let mut decoder = FrameDecoder::default();
        decoder.push(&valid[..valid.len() - 1]).unwrap();
        assert!(decoder.finish().is_err());
        assert_eq!(decoder.push(&valid[valid.len() - 1..]).unwrap().len(), 1);
        decoder.finish().unwrap();
    }

    #[test]
    fn equal_price_order_is_deterministic_and_configuration_is_bounded() {
        let entries = [
            Entry {
                side: '0',
                price: 100.0,
                size: 3.0,
                orders: None,
            },
            Entry {
                side: '0',
                price: 100.0,
                size: 1.0,
                orders: Some(2),
            },
        ];
        assert_eq!(
            levels(entries.iter(), true, 10),
            levels(entries.iter().rev(), true, 10)
        );
        assert!(Normalizer::new("", 10).is_err());
        assert!(Normalizer::new("XNAS", 0).is_err());
        assert!(Normalizer::new("XNAS", 51).is_err());
    }

    #[test]
    fn snapshot_becomes_sorted_book_and_trade() {
        let fix = FixMessage::parse(b"8=FIX.4.4|35=W|55=BTC-USD|207=COINBASE|268=4|269=1|278=a1|270=101|271=2|269=0|278=b1|270=99|271=3|269=0|278=b2|270=100|271=1|269=2|270=100.5|271=.25|273=12:00:00|").unwrap();
        let events = Normalizer::new("UNKNOWN", 10)
            .unwrap()
            .apply(&fix, 42)
            .unwrap();
        let MarketEvent::Trade(trade) = &events[0] else {
            panic!("trade first")
        };
        assert_eq!(trade.price, 100.5);
        let MarketEvent::Book(book) = &events[1] else {
            panic!("book second")
        };
        assert_eq!(
            book.bids
                .iter()
                .map(|level| level.price)
                .collect::<Vec<_>>(),
            vec![100.0, 99.0]
        );
        assert_eq!(book.asks[0].price, 101.0);
    }

    #[test]
    fn incremental_delete_updates_book() {
        let mut normalizer = Normalizer::new("XNAS", 10).unwrap();
        normalizer
            .apply(
                &FixMessage::parse(
                    b"35=W|55=AAPL|268=2|269=0|278=b1|270=100|271=2|269=1|278=a1|270=101|271=3|",
                )
                .unwrap(),
                1,
            )
            .unwrap();
        let events = normalizer
            .apply(
                &FixMessage::parse(b"35=X|268=1|269=0|279=2|278=b1|55=AAPL|270=100|271=0|")
                    .unwrap(),
                2,
            )
            .unwrap();
        let MarketEvent::Book(book) = &events[0] else {
            panic!("book")
        };
        assert!(book.bids.is_empty());
    }

    #[test]
    fn decoder_handles_fragmented_tcp_messages() {
        let mut decoder = FrameDecoder::default();
        assert!(
            decoder
                .push(b"8=FIX.4.4\x019=20\x0135=W\x0155=BT")
                .unwrap()
                .is_empty()
        );
        let frames = decoder
            .push(b"C\x0110=001\x018=FIX.4.4\x019=5\x0135=0\x0110=002\x01")
            .unwrap();
        assert_eq!(frames.len(), 2);
        assert!(frames[0].ends_with(b"10=001\x01"));
        assert!(frames[1].ends_with(b"10=002\x01"));
    }
}
