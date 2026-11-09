//! Browser presenter for the Rooms Spore.
//!
//! Unibus owns membership. This process joins one room as the machine's
//! participant, renders the same `ui/rooms.lua` Canvas renders into a scene
//! document, and serves the browser media mesh. Media never crosses the room
//! service: WebRTC carries audio between pages, and this process only relays
//! offers, answers and ICE candidates.
//!
//! ```sh
//! rooms-serve --config room-client.json --room studio --name Avery \
//!   --spore-root ../../spores/spores/rooms
//! ```

mod web;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, ensure};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use unibus::{
    Envelope, Message,
    client::{Client, ClientConfig},
    kinds,
};
use unibus_room::{
    ADDRESS_KIND, Address, BusClass, ERROR_KIND, JOIN_KIND, Join, LEAVE_KIND, LIST_KIND,
    LIST_REPLY_KIND, MEMBER_KIND, Mic, Presence, Presenter, SELF_KIND, SelfState, WELCOME_KIND,
    Welcome, host::Projection,
};

const DEFAULT_BIND: &str = "127.0.0.1:8793";

const USAGE: &str = "\
rooms-serve — serve the Rooms presenter in a browser

  --config <file>      Unibus client JSON (required)
  --room <name>        room to join (required)
  --name <name>        display name (required)
  --bind <addr:port>   listen address (default 127.0.0.1:8793)
  --spore-root <dir>   Rooms Spore directory (default ./spores/rooms)
  --renderer <file>    scene renderer (default <spore-root>/tools/render_rooms.lua)
  --ui <file>          fungos.ui library (default <spore-root>/../../sdk/ui/ui.lua)
  --address-text <s>   text sent by the Ask JARVIS control
  --transcript <file>  append addressed-turn records as JSONL
  --verbose            log room events and snapshots
";

pub enum Command {
    Action {
        id: String,
        text: Option<String>,
    },
    /// A browser changed its call state; one flag is enough to wake the room
    /// loop, which reads the aggregate from the web state.
    Call,
}

struct Args {
    config: PathBuf,
    room: String,
    name: String,
    bind: String,
    spore_root: PathBuf,
    renderer: PathBuf,
    ui: PathBuf,
    address_text: String,
    transcript: Option<PathBuf>,
    verbose: bool,
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("rooms-serve: {error:#}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<()> {
    let args = parse_args()?;
    ensure!(
        args.config.is_file(),
        "{} does not exist",
        args.config.display()
    );
    for path in [
        &args.renderer,
        &args.ui,
        &args.spore_root.join("ui/rooms.lua"),
    ] {
        ensure!(path.is_file(), "{} does not exist", path.display());
    }

    let client = Client::connect(client_config(&args.config)?)?;
    let (commands, receiver) = mpsc::channel();
    let state = Arc::new(web::State::new());
    let paths = Arc::new(web::Paths {
        view: args.spore_root.join("ui/rooms.lua"),
        ui: args.ui.clone(),
        renderer: args.renderer.clone(),
    });
    web::listen(&args.bind, Arc::clone(&state), paths, commands).map_err(anyhow::Error::msg)?;

    let mut projection = Projection::default();
    let mut selection = Selection {
        room: Some(args.room.clone()),
        ..Selection::default()
    };
    projection.set_selected(selection.value());
    let mut last_self: Option<(Mic, bool)> = None;
    let mut dirty = true;
    let mut next_list = Instant::now() + Duration::from_secs(5);

    send(&client, LIST_KIND, json!({}))?;
    join(&client, &args.room, &args.name)?;

    loop {
        while let Some((envelope, _)) = client.try_read()? {
            match envelope.message.kind.as_str() {
                WELCOME_KIND => {
                    let welcome: Welcome = decode(&envelope)?;
                    if args.verbose {
                        eprintln!(
                            "rooms-serve: welcome {} ({:?})",
                            welcome.room, welcome.me.role
                        );
                    }
                    selection.room = Some(welcome.room.clone());
                    projection.set_selected(selection.value());
                    projection.apply_welcome(welcome);
                    // A rejoin is idempotent in the service and keeps the old
                    // presenter. The browser is always the flat Room view, so
                    // say that once explicitly instead of inheriting a
                    // Holodeck flag from the previous connection.
                    send_self(&client, Mic::Off, false, Some(Presenter::Canvas))?;
                    projection.set_self(Mic::Off, false, Some(Presenter::Canvas));
                    last_self = Some((Mic::Off, false));
                    dirty = true;
                }
                MEMBER_KIND => {
                    projection.apply_member(decode(&envelope)?);
                    dirty = true;
                }
                LIST_REPLY_KIND => {
                    projection.apply_list(decode(&envelope)?);
                    dirty = true;
                }
                ERROR_KIND => {
                    let error: Value = decode(&envelope)?;
                    let code = error["code"].as_str().unwrap_or("error");
                    let message = error["message"].as_str().unwrap_or("room command failed");
                    projection.set_error(Some(format!("{code}: {message}")));
                    dirty = true;
                }
                kinds::REPLY
                    if envelope.message.body.get("success") == Some(&Value::Bool(false)) =>
                {
                    projection.set_error(Some(format!(
                        "hub rejected room command: {:?}",
                        envelope.message.body
                    )));
                    dirty = true;
                }
                _ => {}
            }
        }

        while let Ok(command) = receiver.try_recv() {
            match command {
                Command::Action { id, text } => {
                    if args.verbose {
                        eprintln!("rooms-serve: action {id}");
                    }
                    apply_action(
                        &id,
                        text.as_deref(),
                        &client,
                        &args,
                        &mut projection,
                        &mut selection,
                    )?;
                    dirty = true;
                }
                Command::Call => dirty = true,
            }
        }

        if state.expire_calls() {
            dirty = true;
        }
        let call = state.aggregate_call();
        let mic = if is_live_call(&call) && call["muted"] != true {
            Mic::On
        } else {
            Mic::Off
        };
        let speaking = is_live_call(&call)
            && call["muted"] != true
            && call["mic_rms"].as_f64().unwrap_or(0.0) > 0.012;
        if projection.welcome().is_some() && last_self != Some((mic, speaking)) {
            send_self(&client, mic, speaking, None)?;
            projection.set_self(mic, speaking, None);
            last_self = Some((mic, speaking));
            dirty = true;
        }

        if Instant::now() >= next_list {
            next_list = Instant::now() + Duration::from_secs(5);
            let _ = send(&client, LIST_KIND, json!({}));
        }

        if dirty {
            projection.set_selected(selection.value());
            let snapshot = projection.snapshot("FPL Rooms", &args.name, true, call.clone());
            if args.verbose {
                eprintln!("rooms-serve: snapshot {}", snapshot);
            }
            state.set_snapshot(snapshot);
            dirty = false;
        }

        thread::sleep(Duration::from_millis(20));
    }
}

#[derive(Debug, Default, Clone)]
struct Selection {
    room: Option<String>,
    focus: Option<String>,
}

impl Selection {
    fn value(&self) -> Option<String> {
        let mut parts = Vec::new();
        if let Some(room) = &self.room {
            parts.push(format!("room:{room}"));
        }
        if let Some(focus) = &self.focus {
            parts.push(format!("focus:{focus}"));
        }
        (!parts.is_empty()).then(|| parts.join(";"))
    }
}

fn apply_action(
    id: &str,
    message: Option<&str>,
    client: &Client,
    args: &Args,
    projection: &mut Projection,
    selection: &mut Selection,
) -> Result<()> {
    let message = message.map(str::trim).filter(|text| !text.is_empty());
    if id == "send-message" {
        if let (Some(text), Some(welcome)) = (message, projection.welcome()) {
            let room = welcome.room.clone();
            let speaker_id = welcome.me.id.clone();
            let speaker_name = welcome.me.display_name.clone();
            let segment_id = format!("{speaker_id}-{}", now_ms());
            projection.add_message(&room, &speaker_name, text, true, &segment_id);
        } else if message.is_none() {
            projection.set_error(Some("a room message cannot be empty".into()));
        }
        return Ok(());
    }
    if id == "message-room" {
        projection.set_error(Some(
            "message input is available in rooms.myc; this presenter has no text field yet".into(),
        ));
        return Ok(());
    }
    // Call controls are handled by the page before they reach this endpoint:
    // the browser owns the microphone and the peer connections. If JavaScript
    // is unavailable, there is no media session to start here.
    if matches!(
        id,
        "join-call" | "leave-call" | "toggle-mic" | "request-agent"
    ) {
        return Ok(());
    }
    if id == "new-room" {
        projection.set_error(Some(
            "creating a room needs a name and a host; use room-probe or the Canvas host for now"
                .into(),
        ));
        return Ok(());
    }
    if let Some(room) = id.strip_prefix("fullscreen-") {
        selection.focus = Some(room.to_owned());
        return Ok(());
    }
    if id == "exit-fullscreen" {
        selection.focus = None;
        return Ok(());
    }
    if id == "leave-room" {
        if projection.welcome().is_some() {
            send(client, LEAVE_KIND, json!({}))?;
            projection.clear_room();
        }
        selection.room = None;
        selection.focus = None;
        return Ok(());
    }
    if id == "enter-holodeck" {
        send_self(client, Mic::On, false, Some(Presenter::Holodeck))?;
        projection.set_self(Mic::On, false, Some(Presenter::Holodeck));
        return Ok(());
    }
    if id == "address-agent" {
        let text = message.unwrap_or(&args.address_text);
        send(
            client,
            ADDRESS_KIND,
            serde_json::to_value(Address {
                target: "jarvis".into(),
                text: text.to_owned(),
            })?,
        )?;
        if let Some(welcome) = projection.welcome() {
            let room = welcome.room.clone();
            let speaker_id = welcome.me.id.clone();
            let speaker_name = welcome.me.display_name.clone();
            let now = now_ms();
            let segment_id = format!("{speaker_id}-{now}");
            if let Some(path) = &args.transcript {
                append_transcript(
                    path,
                    room.clone(),
                    speaker_id.clone(),
                    speaker_name.clone(),
                    text.to_owned(),
                    segment_id.clone(),
                )?;
            }
            projection.add_message(&room, &speaker_name, text, true, &segment_id);
        }
        return Ok(());
    }
    if id == "share-surface" {
        projection.set_error(Some(
            "screen sharing needs a host capture session; this web presenter does not capture pixels yet".into(),
        ));
        return Ok(());
    }
    if let Some(room) = id.strip_prefix("join-") {
        if projection.room() == Some(room) {
            selection.room = Some(room.to_owned());
            return Ok(());
        }
        join(client, room, &args.name)?;
        selection.room = Some(room.to_owned());
        selection.focus = None;
        return Ok(());
    }
    Ok(())
}

fn is_live_call(call: &Value) -> bool {
    matches!(
        call["state"].as_str(),
        Some("starting" | "connected" | "muted")
    )
}

fn send(client: &Client, kind: &str, body: Value) -> Result<()> {
    let envelope = client.envelope(None, Message::new(kind, body))?;
    client.send(&envelope, &[])?;
    Ok(())
}

fn join(client: &Client, room: &str, name: &str) -> Result<()> {
    send(
        client,
        JOIN_KIND,
        serde_json::to_value(Join {
            room: room.to_owned(),
            space: None,
            display_name: name.to_owned(),
            presenter: Presenter::Canvas,
            presence: Presence::Active,
            audio: BusClass::Speech,
        })?,
    )
}

fn send_self(
    client: &Client,
    mic: Mic,
    speaking: bool,
    presenter: Option<Presenter>,
) -> Result<()> {
    send(
        client,
        SELF_KIND,
        serde_json::to_value(SelfState {
            presence: Presence::Active,
            mic,
            speaking,
            presenter,
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
        "{} does not grant {}",
        path.display(),
        unibus_room::CAP_MEMBER
    );
    Ok(config)
}

fn append_transcript(
    path: &Path,
    room: String,
    speaker_id: String,
    speaker_name: String,
    text: String,
    segment_id: String,
) -> Result<()> {
    let now = now_ms();
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("open transcript {}", path.display()))?;
    writeln!(
        file,
        "{}",
        json!({
            "schema_version": 1,
            "kind": "room.transcript.v1",
            "room": room,
            "segment_id": segment_id,
            "speaker_id": speaker_id,
            "speaker_name": speaker_name,
            "start_ms": now,
            "end_ms": now,
            "text": text,
            "confidence": 1.0,
            "language": "en",
            "model": "addressed-text",
            "final": true
        })
    )?;
    Ok(())
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as u64)
}

fn parse_args() -> Result<Args> {
    let mut argv = std::env::args().skip(1);
    if argv.any(|arg| arg == "--help" || arg == "-h") {
        println!("{USAGE}");
        std::process::exit(0);
    }
    let mut argv = std::env::args().skip(1);
    let mut config = None;
    let mut room = None;
    let mut name = None;
    let mut bind = DEFAULT_BIND.to_owned();
    let mut spore_root = PathBuf::from("./spores/rooms");
    let mut renderer = None;
    let mut ui = None;
    let mut address_text = "JARVIS, report the room status.".to_owned();
    let mut transcript = None;
    let mut verbose = false;
    while let Some(arg) = argv.next() {
        match arg.as_str() {
            "--config" => {
                config = Some(PathBuf::from(
                    argv.next().context("--config needs a value")?,
                ))
            }
            "--room" => room = Some(argv.next().context("--room needs a value")?),
            "--name" => name = Some(argv.next().context("--name needs a value")?),
            "--bind" => bind = argv.next().context("--bind needs a value")?,
            "--spore-root" => {
                spore_root = PathBuf::from(argv.next().context("--spore-root needs a value")?)
            }
            "--renderer" => {
                renderer = Some(PathBuf::from(
                    argv.next().context("--renderer needs a value")?,
                ))
            }
            "--ui" => ui = Some(PathBuf::from(argv.next().context("--ui needs a value")?)),
            "--address-text" => {
                address_text = argv.next().context("--address-text needs a value")?
            }
            "--transcript" => {
                transcript = Some(PathBuf::from(
                    argv.next().context("--transcript needs a value")?,
                ))
            }
            "--verbose" => verbose = true,
            other => anyhow::bail!("unknown argument {other:?}\n{USAGE}"),
        }
    }
    Ok(Args {
        config: config.context("--config is required")?,
        room: room.context("--room is required")?,
        name: name.context("--name is required")?,
        renderer: renderer.unwrap_or_else(|| spore_root.join("tools/render_rooms.lua")),
        ui: ui.unwrap_or_else(|| spore_root.join("../../sdk/ui/ui.lua")),
        bind,
        spore_root,
        address_text,
        transcript,
        verbose,
    })
}
