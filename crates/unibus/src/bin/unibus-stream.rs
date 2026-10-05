//! Small runbook-parity CLI for publishing one typed stream event.

use std::path::Path;

use anyhow::{Context, Result, bail};
use unibus::{
    Message, STREAM_EVENT_KIND, StreamEvent,
    client::{Client, ClientConfig},
};

fn main() -> Result<()> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    if args.len() != 5 || args[1] != "publish" {
        bail!("usage: unibus-stream CONFIG.json publish STREAM_ID CONTRACT VALUE.json");
    }
    let config_path = Path::new(&args[0]);
    let config: ClientConfig = serde_json::from_slice(
        &std::fs::read(config_path).with_context(|| format!("read {}", config_path.display()))?,
    )
    .with_context(|| format!("parse {}", config_path.display()))?;
    let observed_at_unix_ms = unibus::envelope::now_ms();
    let event = StreamEvent {
        stream_id: args[2].clone(),
        contract: args[3].clone(),
        sequence: observed_at_unix_ms.max(1),
        observed_at_unix_ms,
        value: serde_json::from_str(&args[4]).context("parse VALUE.json")?,
    };
    event.validate()?;
    let client = Client::connect(config)?;
    let envelope = client.envelope(
        None,
        Message::new(STREAM_EVENT_KIND, serde_json::to_value(event)?),
    )?;
    client.send(&envelope, &[])
}
