//! Source-neutral, demand-driven compressed-video adapter. Canvas is a consumer.
use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use std::{
    collections::BTreeMap,
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};
use unibus::{
    Message, STREAM_EVENT_KIND, STREAM_INTEREST_KIND, StreamEvent, StreamInterest,
    client::{Client, ClientConfig},
};
use unibus_media::mpegts::{
    CHUNK_TTL_MS, MAX_CHUNK_BYTES, MPEGTS_CONTRACT, MpegTsHeader, validate_delivery,
    validate_payload,
};

#[derive(Deserialize)]
struct Manifest {
    generation: String,
    expires_at_unix_ms: u64,
}

struct Encoder {
    child: Child,
    packets: mpsc::Receiver<(u64, Vec<u8>)>,
    header: MpegTsHeader,
}
impl Drop for Encoder {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn probe(path: &Path, generation: String) -> Result<MpegTsHeader> {
    let out = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=width,height,codec_name",
            "-of",
            "json",
        ])
        .arg(path)
        .output()?;
    ensure!(
        out.status.success() && out.stdout.len() <= 65536,
        "video probe failed"
    );
    let value: serde_json::Value = serde_json::from_slice(&out.stdout)?;
    let video = &value["streams"][0];
    let header = MpegTsHeader {
        generation,
        width: video["width"]
            .as_u64()
            .context("missing video width")?
            .try_into()?,
        height: video["height"]
            .as_u64()
            .context("missing video height")?
            .try_into()?,
        codec: video["codec_name"]
            .as_str()
            .context("missing video codec")?
            .into(),
    };
    header.validate()?;
    Ok(header)
}

fn encoder(directory: &Path, generation: String) -> Result<Encoder> {
    let input = directory.join("current.mp4");
    let header = probe(&input, generation)?;
    let mut child = Command::new("ffmpeg")
        .args([
            "-nostdin",
            "-hide_banner",
            "-loglevel",
            "warning",
            "-stream_loop",
            "-1",
            "-re",
            "-i",
        ])
        .arg(input)
        .args([
            "-map",
            "0:v:0",
            "-map",
            "0:a:0?",
            "-c:v",
            "copy",
            "-c:a",
            "aac",
            "-mpegts_flags",
            "+resend_headers",
            "-pat_period",
            "0.1",
            "-f",
            "mpegts",
            "pipe:1",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()?;
    let mut output = child.stdout.take().context("missing encoder output")?;
    let (tx, packets) = mpsc::sync_channel(8);
    thread::spawn(move || {
        loop {
            let mut payload = vec![0; MAX_CHUNK_BYTES];
            if output.read_exact(&mut payload).is_err() {
                break;
            }
            if tx.send((unibus::envelope::now_ms(), payload)).is_err() {
                break;
            }
        }
    });
    Ok(Encoder {
        child,
        packets,
        header,
    })
}

fn config(path: &str) -> Result<ClientConfig> {
    let mut config: ClientConfig = serde_json::from_slice(&std::fs::read(path)?)?;
    config.resolve_token_file(Path::new(path));
    config.read_timeout_ms = 100;
    Ok(config)
}

fn send(bus: &Client, kind: &str, body: serde_json::Value, payload: &[u8]) -> Result<()> {
    let mut envelope = bus.envelope(None, Message::new(kind, body))?;
    envelope.payload_bytes = payload.len();
    if !payload.is_empty() {
        envelope.ttl_ms = Some(CHUNK_TTL_MS);
    }
    bus.send(&envelope, payload)
}

fn publish(config_path: &str, directory: &Path, stream: &str) -> Result<()> {
    let bus = Arc::new(Client::connect(config(config_path)?)?);
    let interests = Arc::new(Mutex::new(BTreeMap::<String, StreamInterest>::new()));
    let alive = Arc::new(AtomicBool::new(true));
    let (read_bus, demands, live) = (bus.clone(), interests.clone(), alive.clone());
    let stream_name = stream.to_owned();
    thread::spawn(move || {
        let result = (|| -> Result<()> {
            loop {
                let Some((envelope, _)) = read_bus.try_read()? else {
                    continue;
                };
                ensure!(
                    envelope
                        .message
                        .body
                        .get("success")
                        .and_then(|v| v.as_bool())
                        != Some(false),
                    "video publication rejected"
                );
                if envelope.message.kind == STREAM_INTEREST_KIND {
                    let interest: StreamInterest =
                        serde_json::from_value(serde_json::Value::Object(envelope.message.body))?;
                    interest.validate()?;
                    if interest.stream_ids.iter().any(|s| s == &stream_name) {
                        demands
                            .lock()
                            .unwrap()
                            .insert(interest.lease_id.clone(), interest);
                    }
                }
            }
        })();
        if let Err(error) = result {
            eprintln!("video transport: {error:#}");
        }
        live.store(false, Ordering::Release);
    });
    let mut current: Option<Encoder> = None;
    let mut sequence = unibus::envelope::now_ms();
    while alive.load(Ordering::Acquire) {
        let now = unibus::envelope::now_ms();
        let needed = {
            let mut demand = interests.lock().unwrap();
            demand.retain(|_, i| i.active_at(now));
            !demand.is_empty()
        };
        if !needed {
            current = None;
            thread::sleep(Duration::from_millis(100));
            continue;
        }
        let manifest: Manifest =
            serde_json::from_slice(&std::fs::read(directory.join("current.json"))?)?;
        if manifest.expires_at_unix_ms <= now {
            current = None;
            thread::sleep(Duration::from_millis(100));
            continue;
        }
        if current
            .as_ref()
            .is_none_or(|c| c.header.generation != manifest.generation)
        {
            drop(current.take());
            current = Some(encoder(directory, manifest.generation)?);
        }
        let source = current.as_mut().context("missing source")?;
        match source.packets.recv_timeout(Duration::from_millis(100)) {
            Ok((observed, payload)) => {
                sequence = sequence
                    .checked_add(1)
                    .context("video sequence exhausted")?;
                if unibus::envelope::now_ms().saturating_sub(observed) > CHUNK_TTL_MS {
                    continue;
                }
                validate_payload(&source.header, &payload)?;
                let event = StreamEvent {
                    stream_id: stream.into(),
                    contract: MPEGTS_CONTRACT.into(),
                    sequence,
                    observed_at_unix_ms: observed,
                    value: serde_json::to_value(&source.header)?,
                };
                event.validate()?;
                send(
                    &bus,
                    STREAM_EVENT_KIND,
                    serde_json::to_value(event)?,
                    &payload,
                )?;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                anyhow::bail!("video encoder ended unexpectedly")
            }
        }
    }
    anyhow::bail!("video transport disconnected")
}

fn receive(
    config_path: &str,
    stream: &str,
    owner: &str,
    generation: Option<&str>,
    raw: bool,
) -> Result<()> {
    let bus = Client::connect(config(config_path)?)?;
    let mut renewed = Instant::now() - Duration::from_secs(10);
    let mut previous = None;
    let mut selected: Option<MpegTsHeader> = None;
    let mut last_packet = Instant::now();
    let mut output = std::io::stdout().lock();
    loop {
        if renewed.elapsed() >= Duration::from_secs(5) {
            let now = unibus::envelope::now_ms();
            let interest = StreamInterest {
                lease_id: format!("{}:video", bus.config().producer_id),
                consumer_id: bus.config().producer_id.clone(),
                stream_ids: vec![stream.into()],
                issued_at_unix_ms: now,
                expires_at_unix_ms: now + 15000,
            };
            interest.validate()?;
            send(
                &bus,
                STREAM_INTEREST_KIND,
                serde_json::to_value(interest)?,
                &[],
            )?;
            renewed = Instant::now();
        }
        ensure!(
            last_packet.elapsed() < Duration::from_secs(10),
            "video source unavailable for 10 seconds"
        );
        let Some((envelope, payload)) = bus.try_read()? else {
            continue;
        };
        ensure!(
            envelope
                .message
                .body
                .get("success")
                .and_then(|v| v.as_bool())
                != Some(false),
            "video subscription rejected"
        );
        if envelope.message.kind != STREAM_EVENT_KIND || envelope.producer_id != owner {
            continue;
        }
        let event: StreamEvent =
            serde_json::from_value(serde_json::Value::Object(envelope.message.body))?;
        event.validate()?;
        if event.stream_id != stream || event.contract != MPEGTS_CONTRACT {
            continue;
        }
        let header: MpegTsHeader = serde_json::from_value(event.value)?;
        validate_payload(&header, &payload)?;
        if generation.is_some_and(|g| g != header.generation) {
            continue;
        }
        validate_delivery(
            event.observed_at_unix_ms,
            unibus::envelope::now_ms(),
            previous,
            event.sequence,
        )?;
        if let Some(old) = &selected {
            ensure!(
                *old == header,
                "video generation/geometry changed; restart decoder"
            );
        } else {
            if !raw {
                serde_json::to_writer(&mut output, &header)?;
                output.write_all(b"\n")?;
                output.flush()?;
            }
            selected = Some(header);
        }
        previous = Some(event.sequence);
        last_packet = Instant::now();
        output.write_all(&payload)?;
        output.flush()?;
    }
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.as_slice() {
        [mode, config, directory, stream] if mode == "publish" => {
            publish(config, &PathBuf::from(directory), stream)
        }
        [mode, config, stream, owner, rest @ ..] if mode == "receive" && rest.len() <= 2 => {
            let raw = rest.iter().any(|s| s == "--raw");
            let generation = rest
                .iter()
                .find(|s| s.as_str() != "--raw")
                .map(String::as_str);
            receive(config, stream, owner, generation, raw)
        }
        _ => anyhow::bail!(
            "usage: unibus-video publish CLIENT.json SOURCE_DIR STREAM | receive CLIENT.json STREAM OWNER [GENERATION] [--raw]"
        ),
    }
}
