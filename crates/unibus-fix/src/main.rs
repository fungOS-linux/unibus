use std::io::{self, Read};
use std::net::TcpStream;
use std::path::Path;

use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;
use unibus::Message;
use unibus::client::{Client, ClientConfig};
use unibus_fix::{FixMessage, FrameDecoder, MarketEvent, Normalizer, ORDER_BOOK_KIND, TRADE_KIND};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    #[serde(flatten)]
    bus: ClientConfig,
    source: Source,
    #[serde(default)]
    destination: Option<String>,
    default_venue: String,
    #[serde(default = "default_depth")]
    depth: usize,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
enum Source {
    Stdin,
    File { path: String },
    Tcp { endpoint: String },
}

fn default_depth() -> usize {
    10
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let path = std::env::args_os()
        .nth(1)
        .context("usage: unibus-fix <config.json>")?;
    let bytes = std::fs::read(&path)
        .with_context(|| format!("read config {}", Path::new(&path).display()))?;
    let config: Config = serde_json::from_slice(&bytes).context("parse FIX streamer config")?;
    ensure!(
        config.bus.schema_version == unibus::SCHEMA_VERSION_V2,
        "FIX streamer requires Unibus schema_version 2"
    );
    ensure!(
        config
            .bus
            .capabilities
            .iter()
            .any(|value| value == "market.fix"),
        "bus capabilities must include market.fix"
    );
    run(config)
}

fn run(config: Config) -> Result<()> {
    let mut normalizer = Normalizer::new(config.default_venue, config.depth)?;
    let bus = Client::connect(config.bus)?;
    match config.source {
        Source::Stdin => consume(
            io::stdin().lock(),
            &bus,
            &mut normalizer,
            config.destination.as_deref(),
        ),
        Source::File { path } => consume(
            std::fs::File::open(&path).with_context(|| format!("open FIX replay {path}"))?,
            &bus,
            &mut normalizer,
            config.destination.as_deref(),
        ),
        Source::Tcp { endpoint } => {
            consume(
                TcpStream::connect(&endpoint)
                    .with_context(|| format!("connect FIX source {endpoint}"))?,
                &bus,
                &mut normalizer,
                config.destination.as_deref(),
            )?;
            bail!("FIX TCP source disconnected")
        }
    }
}

fn consume(
    mut reader: impl Read,
    bus: &Client,
    normalizer: &mut Normalizer,
    destination: Option<&str>,
) -> Result<()> {
    let mut decoder = FrameDecoder::default();
    let mut buffer = [0_u8; 8192];
    loop {
        let count = reader.read(&mut buffer).context("read FIX source")?;
        if count == 0 {
            break;
        }
        for frame in decoder.push(&buffer[..count])? {
            let message = FixMessage::parse(&frame).context("parse FIX frame")?;
            for event in normalizer.apply(&message, unibus::envelope::now_ms())? {
                let (kind, body) = match event {
                    MarketEvent::Book(book) => (ORDER_BOOK_KIND, serde_json::to_value(book)?),
                    MarketEvent::Trade(trade) => (TRADE_KIND, serde_json::to_value(trade)?),
                };
                let envelope = bus.envelope(destination, Message::new(kind, body))?;
                bus.send(&envelope, &[])
                    .with_context(|| format!("publish {kind}"))?;
            }
        }
    }
    decoder.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn replay_publishes_normalized_events_through_the_real_router() {
        use std::{
            sync::Arc,
            time::{Duration, Instant},
        };
        let dir = tempfile::tempdir().unwrap();
        let token_file = dir.path().join("token");
        std::fs::write(&token_file, "0123456789abcdef0123456789abcdef").unwrap();
        let hub_config = serde_json::json!({
            "installation_id": "fix-test", "listen": "127.0.0.1:0",
            "routes": [
                {"kind": ORDER_BOOK_KIND, "target": {"mode": "capability", "name": "market.monitor"}},
                {"kind": TRADE_KIND, "target": {"mode": "capability", "name": "market.monitor"}}
            ],
            "nodes": [
                {"producer_id": "fix", "token_file": token_file, "zones": ["default"],
                 "capabilities": ["market.fix"], "publishes": [ORDER_BOOK_KIND, TRADE_KIND], "publish_zones": ["default"]},
                {"producer_id": "monitor", "token_file": token_file, "zones": ["default"], "capabilities": ["market.monitor"]}
            ]
        });
        let hub =
            Arc::new(unibus_router::Hub::new(serde_json::from_value(hub_config).unwrap()).unwrap());
        let address = hub.start().unwrap().to_string();
        let bus_config = serde_json::json!({
            "installation_id": "fix-test", "producer_id": "monitor", "hub_endpoint": address,
            "token_file": token_file, "zones": ["default"], "capabilities": ["market.monitor"],
            "schema_version": 2, "read_timeout_ms": 50
        });
        let monitor = Client::connect(serde_json::from_value(bus_config.clone()).unwrap()).unwrap();
        let frame_path = dir.path().join("capture.fix");
        let body = b"35=W\x0155=AAPL\x01207=XNAS\x01268=2\x01269=0\x01278=b1\x01270=100\x01271=2\x01269=2\x01270=100.5\x01271=1\x01";
        let mut frame = format!("8=FIX.4.4\x019={}\x01", body.len()).into_bytes();
        frame.extend_from_slice(body);
        let checksum = frame.iter().fold(0u8, |sum, byte| sum.wrapping_add(*byte));
        frame.extend_from_slice(format!("10={checksum:03}\x01").as_bytes());
        std::fs::write(&frame_path, frame).unwrap();
        let mut producer_config = bus_config;
        producer_config["producer_id"] = serde_json::json!("fix");
        producer_config["capabilities"] = serde_json::json!(["market.fix"]);
        producer_config["source"] = serde_json::json!({"mode": "file", "path": frame_path});
        producer_config["default_venue"] = serde_json::json!("UNKNOWN");
        run(serde_json::from_value(producer_config).unwrap()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut observed = std::collections::BTreeSet::new();
        while observed.len() < 2 {
            assert!(
                Instant::now() < deadline,
                "normalized events did not arrive"
            );
            if let Some((envelope, payload)) = monitor.try_read().unwrap() {
                if !matches!(envelope.message.kind.as_str(), ORDER_BOOK_KIND | TRADE_KIND) {
                    continue;
                }
                assert!(payload.is_empty());
                assert_eq!(envelope.message.body["symbol"], "AAPL");
                assert_eq!(envelope.message.body["venue"], "XNAS");
                if envelope.message.kind == ORDER_BOOK_KIND {
                    assert_eq!(envelope.message.body["bids"][0]["price"], 100.0);
                } else {
                    assert_eq!(envelope.message.body["price"], 100.5);
                }
                observed.insert(envelope.message.kind);
            }
        }
    }

    #[test]
    fn example_configuration_loads_and_rejects_unknown_fields() {
        let example = include_str!("../../../config/unibus-fix.example.json");
        let config: Config = serde_json::from_str(example).unwrap();
        assert_eq!(config.bus.schema_version, unibus::SCHEMA_VERSION_V2);
        assert!(matches!(config.source, Source::Stdin));
        let mut unknown: serde_json::Value = serde_json::from_str(example).unwrap();
        unknown["typo"] = serde_json::json!(true);
        assert!(serde_json::from_value::<Config>(unknown).is_err());
    }
}
