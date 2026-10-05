//! Read-only OS MIDI inventory publisher. Never opens a MIDI port connection.
use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;
use std::time::{Duration, Instant};
use unibus::{
    Message,
    client::{Client, ClientConfig},
};
use unibus_midi::inventory::{
    MAX_INVENTORY_TTL_MS, MIDI_DISCOVERY_CAPABILITY, MIDI_PORTS_CONTRACT, enumerate,
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    bus: ClientConfig,
    destination: String,
    interval_ms: u64,
    ttl_ms: u64,
}

impl Config {
    fn validate(&self) -> Result<()> {
        ensure!(
            unibus_midi::valid_identifier(&self.destination),
            "invalid destination"
        );
        ensure!(
            self.interval_ms >= 250
                && self.interval_ms < self.ttl_ms
                && self.ttl_ms <= MAX_INVENTORY_TTL_MS,
            "require 250 <= interval_ms < ttl_ms <= 30000"
        );
        ensure!(
            self.bus.schema_version == unibus::SCHEMA_VERSION_V2,
            "discovery requires schema version 2"
        );
        ensure!(
            self.bus
                .capabilities
                .iter()
                .any(|c| c == MIDI_DISCOVERY_CAPABILITY),
            "missing midi.discovery capability"
        );
        Ok(())
    }
}

fn main() -> Result<()> {
    let path = std::env::args_os()
        .nth(1)
        .context("usage: unibus-midi-discovery <config.json>")?;
    let mut config: Config = serde_json::from_slice(&std::fs::read(path)?)?;
    config.validate()?;
    config.bus.read_timeout_ms = 25;
    // Fail visibly on disconnect or authorization rejection; the service manager
    // owns restart policy rather than hiding an indefinitely failing session.
    let client = Client::connect(config.bus.clone())?;
    let mut next = Instant::now();
    loop {
        if Instant::now() >= next {
            let inventory = enumerate(
                &config.bus.producer_id,
                unibus::envelope::now_ms(),
                config.ttl_ms,
            )?;
            let mut envelope = client.envelope(
                Some(&config.destination),
                Message::new(MIDI_PORTS_CONTRACT, serde_json::to_value(inventory)?),
            )?;
            envelope.ttl_ms = Some(config.ttl_ms);
            client.send(&envelope, &[])?;
            next = Instant::now() + Duration::from_millis(config.interval_ms);
        }
        if let Some((envelope, _)) = client.try_read()? {
            if envelope.message.kind == unibus::kinds::V1_REPLY
                && envelope
                    .message
                    .body
                    .get("success")
                    .and_then(serde_json::Value::as_bool)
                    == Some(false)
            {
                bail!(
                    "Unibus rejected MIDI inventory: {:?}",
                    envelope.message.body
                );
            }
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}
