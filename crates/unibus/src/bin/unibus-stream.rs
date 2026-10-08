//! Bounded JSON-lines adapter for existing leased Unibus streams.
use anyhow::{Context, Result, ensure};
use std::{
    io::{BufRead, Read, Write},
    time::{Duration, Instant},
};
use unibus::{
    Message, STREAM_EVENT_KIND, STREAM_INTEREST_KIND, StreamEvent, StreamInterest,
    client::{Client, ClientConfig},
};

const LIMIT: usize = 64 * 1024;

fn checked_event(
    event: &StreamEvent,
    producer: &str,
    expected: &str,
    stream: &str,
    contract: &str,
    previous: u64,
    now: u64,
) -> Result<()> {
    event.validate()?;
    ensure!(producer == expected, "unexpected stream producer");
    ensure!(
        event.stream_id == stream && event.contract == contract,
        "unexpected stream contract"
    );
    ensure!(event.sequence > previous, "replayed stream sequence");
    ensure!(
        event.observed_at_unix_ms <= now.saturating_add(1000)
            && now.saturating_sub(event.observed_at_unix_ms) <= 7000,
        "expired stream sample"
    );
    Ok(())
}

fn send(client: &Client, kind: &str, body: serde_json::Value) -> Result<()> {
    client.send(&client.envelope(None, Message::new(kind, body))?, &[])
}

fn run(args: &[String]) -> Result<()> {
    // Preserve the existing one-shot runbook interface.
    if args.len() == 5 && args[1] == "publish" {
        let path = std::path::Path::new(&args[0]);
        let mut config: ClientConfig = serde_json::from_slice(&std::fs::read(path)?)?;
        config.resolve_token_file(path);
        let client = Client::connect(config)?;
        let now = unibus::envelope::now_ms().max(1);
        let event = StreamEvent {
            stream_id: args[2].clone(),
            contract: args[3].clone(),
            sequence: now,
            observed_at_unix_ms: now,
            value: serde_json::from_str(&args[4]).context("parse VALUE.json")?,
        };
        event.validate()?;
        return send(&client, STREAM_EVENT_KIND, serde_json::to_value(event)?);
    }
    ensure!(
        args.len() == 4 || args.len() == 5,
        "usage: unibus-stream publish CONFIG STREAM CONTRACT | subscribe CONFIG STREAM CONTRACT EXPECTED_PRODUCER"
    );
    let mode = &args[0];
    ensure!(
        (mode == "publish" && args.len() == 4) || (mode == "subscribe" && args.len() == 5),
        "invalid stream mode"
    );
    let mut config: ClientConfig = serde_json::from_slice(&std::fs::read(&args[1])?)?;
    config.resolve_token_file(std::path::Path::new(&args[1]));
    config.read_timeout_ms = 100;
    let client = Client::connect(config)?;
    let (stream, contract) = (&args[2], &args[3]);
    if mode == "publish" {
        let mut input = std::io::stdin().lock();
        let mut sequence = unibus::envelope::now_ms();
        loop {
            let mut line = Vec::new();
            let count = input
                .by_ref()
                .take((LIMIT + 1) as u64)
                .read_until(b'\n', &mut line)?;
            if count == 0 {
                return Ok(());
            }
            ensure!(
                count <= LIMIT && line.ends_with(b"\n"),
                "oversized/incomplete stream JSON line"
            );
            sequence = sequence.checked_add(1).context("sequence exhausted")?;
            let event = StreamEvent {
                stream_id: stream.clone(),
                contract: contract.clone(),
                sequence,
                observed_at_unix_ms: unibus::envelope::now_ms(),
                value: serde_json::from_slice(&line)?,
            };
            event.validate()?;
            send(&client, STREAM_EVENT_KIND, serde_json::to_value(event)?)?;
            // Fail visibly on authorization errors instead of claiming publication succeeded.
            while let Some((reply, _)) = client.try_read()? {
                ensure!(
                    reply.message.body.get("success").and_then(|v| v.as_bool()) != Some(false),
                    "stream publication rejected"
                );
            }
        }
    }
    let mut renewed = Instant::now() - Duration::from_secs(10);
    let mut previous = 0;
    let mut output = std::io::stdout().lock();
    loop {
        if renewed.elapsed() >= Duration::from_secs(5) {
            let now = unibus::envelope::now_ms();
            let interest = StreamInterest {
                lease_id: format!("{}:stream", client.config().producer_id),
                consumer_id: client.config().producer_id.clone(),
                stream_ids: vec![stream.clone()],
                issued_at_unix_ms: now,
                expires_at_unix_ms: now + 15_000,
            };
            interest.validate()?;
            send(
                &client,
                STREAM_INTEREST_KIND,
                serde_json::to_value(interest)?,
            )?;
            renewed = Instant::now();
        }
        let Some((envelope, payload)) = client.try_read()? else {
            continue;
        };
        ensure!(
            envelope
                .message
                .body
                .get("success")
                .and_then(|v| v.as_bool())
                != Some(false),
            "stream subscription rejected"
        );
        if envelope.message.kind != STREAM_EVENT_KIND {
            continue;
        }
        ensure!(
            payload.is_empty(),
            "JSON stream must not carry binary payload"
        );
        let event: StreamEvent =
            serde_json::from_value(serde_json::Value::Object(envelope.message.body))?;
        if let Err(error) = checked_event(
            &event,
            &envelope.producer_id,
            &args[4],
            stream,
            contract,
            previous,
            unibus::envelope::now_ms(),
        ) {
            eprintln!("unibus-stream: rejected sample: {error}");
            continue;
        }
        previous = event.sequence;
        serde_json::to_writer(&mut output, &event.value)?;
        output.write_all(b"\n")?;
        output.flush()?;
    }
}

fn main() -> Result<()> {
    run(&std::env::args().skip(1).collect::<Vec<_>>())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn event() -> StreamEvent {
        StreamEvent {
            stream_id: "canvas/tv/presentation".into(),
            contract: "canvas.presentation.v1".into(),
            sequence: 10,
            observed_at_unix_ms: 1000,
            value: serde_json::json!({"schema_version":1}),
        }
    }
    #[test]
    fn requires_expected_owner_contract_and_fresh_monotonic_sample() {
        let check = |e: &StreamEvent, p: &str, previous, now| {
            checked_event(
                e,
                p,
                "tv",
                "canvas/tv/presentation",
                "canvas.presentation.v1",
                previous,
                now,
            )
        };
        assert!(check(&event(), "tv", 9, 2000).is_ok());
        assert!(check(&event(), "imposter", 9, 2000).is_err());
        assert!(check(&event(), "tv", 10, 2000).is_err());
        assert!(check(&event(), "tv", 9, 8001).is_err());
        let mut wrong = event();
        wrong.contract = "other.v1".into();
        assert!(check(&wrong, "tv", 9, 2000).is_err());
    }
    #[test]
    fn payload_bound_is_enforced() {
        let mut oversized = event();
        oversized.value = serde_json::json!({"blob":"x".repeat(LIMIT)});
        assert!(oversized.validate().is_err());
    }
}
