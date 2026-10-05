//! Safe end-to-end check: empty held state only, never types or clicks.
use anyhow::{Context, Result, ensure};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use unibus::{Message, client::Client};
use unibus_input::{
    DESKTOP_STATE_CONTRACT, DesktopInputState,
    desktop::{ACK_CONTRACT, Ack, CaptureConfig},
};

fn main() -> Result<()> {
    let path = std::env::args()
        .nth(1)
        .context("usage: unibus-desktop-probe capture.json")?;
    let mut config: CaptureConfig = serde_json::from_slice(&std::fs::read(path)?)?;
    config.bus.read_timeout_ms = 10;
    let client = Client::connect(config.bus.clone())?;
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?;
    let input = DesktopInputState {
        schema_version: 1,
        session_id: config.session_id,
        controller_id: config.controller_id,
        target_id: config.target_id,
        sequence: now.as_micros() as u64,
        sampled_at_unix_ms: now.as_millis() as u64,
        valid_until_unix_ms: now.as_millis() as u64 + 250,
        keys: vec![],
        pointer_x: 32768,
        pointer_y: 32768,
        pointer_buttons: 0,
        scroll_x: 0,
        scroll_y: 0,
    };
    input.validate().map_err(anyhow::Error::msg)?;
    let started = Instant::now();
    client.send(
        &client.envelope(
            Some(&config.destination),
            Message::new(DESKTOP_STATE_CONTRACT, serde_json::to_value(&input)?),
        )?,
        &[],
    )?;
    while started.elapsed() < Duration::from_secs(2) {
        if let Some((envelope, payload)) = client.try_read()? {
            ensure!(payload.is_empty(), "unexpected acknowledgement payload");
            if envelope.message.kind != ACK_CONTRACT || envelope.producer_id != config.destination {
                continue;
            }
            let ack: Ack =
                serde_json::from_value(serde_json::Value::Object(envelope.message.body))?;
            ensure!(
                ack.session_id == input.session_id
                    && ack.controller_id == input.controller_id
                    && ack.target_id == input.target_id
                    && ack.sequence == input.sequence,
                "incorrect acknowledgement"
            );
            println!(
                "TV compositor acknowledged empty input in {} ms",
                started.elapsed().as_millis()
            );
            return Ok(());
        }
    }
    anyhow::bail!("TV compositor did not acknowledge input within two seconds")
}
