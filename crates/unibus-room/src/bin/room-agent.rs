//! Headless addressed agent participant.
//!
//! The agent joins as a room member whose grant carries `room.agent`. It does
//! not listen to the room's speech bus. It waits for `room.addressed.v1`, then
//! speaks that turn once through a direct Opus sender. A newer turn cancels the
//! previous one; only the latest requested answer plays.
//!
//! Media stays out of the room service: the service delivers text, and the
//! agent sends Opus to the peer endpoint configured on this host.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use unibus::{
    Envelope, Message,
    client::{Client, ClientConfig},
    kinds,
};
use unibus_room::{
    ADDRESSED_KIND, Addressed, BusClass, ERROR_KIND, JOIN_KIND, Join, LEAVE_KIND, Mic, Presence,
    Presenter, SELF_KIND, SelfState, WELCOME_KIND, Welcome,
};

struct Args {
    config: PathBuf,
    room: String,
    name: String,
    peer: String,
    bind: String,
    tts_bin: PathBuf,
    seconds: u64,
    verbose: bool,
}

fn main() -> Result<()> {
    let args = parse_args()?;
    let client = Client::connect(client_config(&args.config)?)?;
    send(
        &client,
        JOIN_KIND,
        serde_json::to_value(Join {
            room: args.room.clone(),
            space: None,
            display_name: args.name.clone(),
            presenter: Presenter::Canvas,
            presence: Presence::Active,
            audio: BusClass::Speech,
        })?,
    )?;

    let deadline = (args.seconds > 0).then(|| Instant::now() + Duration::from_secs(args.seconds));
    let mut speaking = None;
    let mut tts: Option<Child> = None;
    loop {
        while let Some((envelope, _)) = client.try_read()? {
            match envelope.message.kind.as_str() {
                WELCOME_KIND => {
                    let welcome: Welcome = decode(&envelope)?;
                    println!(
                        "room-agent: joined {} as {} ({:?})",
                        welcome.room, welcome.me.display_name, welcome.me.role
                    );
                    send_self(&client, false)?;
                    speaking = Some(false);
                }
                ADDRESSED_KIND => {
                    let addressed: Addressed = decode(&envelope)?;
                    println!(
                        "room-agent: addressed turn {} from {}: {}",
                        addressed.turn_id, addressed.from, addressed.text
                    );
                    if let Some(mut child) = tts.take() {
                        let _ = child.kill();
                        let _ = child.wait();
                    }
                    match start_tts(&args, &addressed.text) {
                        Ok(child) => {
                            tts = Some(child);
                            if speaking != Some(true) {
                                send_self(&client, true)?;
                                speaking = Some(true);
                            }
                        }
                        Err(error) => eprintln!("room-agent: TTS failed: {error:#}"),
                    }
                }
                ERROR_KIND => {
                    let error: Value = decode(&envelope)?;
                    eprintln!("room-agent: room error: {error}");
                }
                kinds::REPLY
                    if envelope.message.body.get("success") == Some(&Value::Bool(false)) =>
                {
                    eprintln!(
                        "room-agent: hub rejected command: {:?}",
                        envelope.message.body
                    );
                }
                _ => {}
            }
        }

        if let Some(child) = tts.as_mut()
            && let Some(status) = child.try_wait()?
        {
            if args.verbose {
                eprintln!("room-agent: TTS exited with {status}");
            }
            tts = None;
            if speaking != Some(false) {
                send_self(&client, false)?;
                speaking = Some(false);
            }
        }

        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }

    if let Some(mut child) = tts.take() {
        let _ = child.kill();
        let _ = child.wait();
    }
    let _ = send(&client, LEAVE_KIND, json!({}));
    Ok(())
}

fn start_tts(args: &Args, text: &str) -> Result<Child> {
    let child = Command::new(&args.tts_bin)
        .args([
            "--bind",
            &args.bind,
            "--peer",
            &args.peer,
            "--source",
            "tts",
            "--text",
            text,
            "--seconds",
            "0",
            "--no-receive",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .with_context(|| format!("start {} for addressed turn", args.tts_bin.display()))?;
    Ok(child)
}

fn send(client: &Client, kind: &str, body: Value) -> Result<()> {
    let envelope = client.envelope(None, Message::new(kind, body))?;
    client.send(&envelope, &[])?;
    Ok(())
}

fn send_self(client: &Client, speaking: bool) -> Result<()> {
    send(
        client,
        SELF_KIND,
        serde_json::to_value(SelfState {
            presence: Presence::Active,
            // The agent publishes TTS on the speech bus only while a turn is
            // speaking, so its microphone state tracks TTS lifecycle.
            mic: if speaking { Mic::On } else { Mic::Off },
            speaking,
            presenter: None,
        })?,
    )
}

fn decode<T: DeserializeOwned>(envelope: &Envelope) -> Result<T> {
    serde_json::from_value(Value::Object(envelope.message.body.clone()))
        .with_context(|| format!("decode {}", envelope.message.kind))
}

fn client_config(path: &Path) -> Result<ClientConfig> {
    let bytes = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
    let mut config: ClientConfig =
        serde_json::from_slice(&bytes).with_context(|| format!("parse {}", path.display()))?;
    config.resolve_token_file(path);
    config.schema_version = unibus::SCHEMA_VERSION_V2;
    config.read_timeout_ms = 200;
    ensure!(
        config
            .capabilities
            .iter()
            .any(|capability| capability == unibus_room::CAP_MEMBER),
        "client config must claim {}",
        unibus_room::CAP_MEMBER
    );
    ensure!(
        config
            .capabilities
            .iter()
            .any(|capability| capability == unibus_room::CAP_AGENT),
        "client config must claim {}",
        unibus_room::CAP_AGENT
    );
    Ok(config)
}

fn parse_args() -> Result<Args> {
    let mut argv = std::env::args().skip(1);
    let config = argv.next().context(
        "usage: room-agent CLIENT.json --room ROOM --peer HOST:PORT [--name JARVIS] [--seconds N]",
    )?;
    let mut args = Args {
        config: config.into(),
        room: "studio".into(),
        name: "JARVIS".into(),
        peer: "127.0.0.1:48000".into(),
        bind: "127.0.0.1:48010".into(),
        tts_bin: PathBuf::from("soil-opus-call"),
        seconds: 0,
        verbose: false,
    };
    while let Some(arg) = argv.next() {
        match arg.as_str() {
            "--room" => args.room = argv.next().context("--room needs a value")?,
            "--name" => args.name = argv.next().context("--name needs a value")?,
            "--peer" => args.peer = argv.next().context("--peer needs a value")?,
            "--bind" => args.bind = argv.next().context("--bind needs a value")?,
            "--tts-bin" => args.tts_bin = argv.next().context("--tts-bin needs a value")?.into(),
            "--seconds" => {
                args.seconds = argv
                    .next()
                    .context("--seconds needs a value")?
                    .parse()
                    .context("--seconds must be a number")?;
            }
            "--verbose" => args.verbose = true,
            other => bail!("unknown argument {other}"),
        }
    }
    Ok(args)
}
