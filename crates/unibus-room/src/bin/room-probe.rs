//! Two-machine room probe.
//!
//! Usage:
//!   room-probe CLIENT.json ROOM [--name NAME] [--presenter canvas|holodeck]
//!              [--seconds N] [--list]
//!
//! The client config is the normal Unibus `ClientConfig` shape. It must claim
//! `room.member` and the matching node grant must allow it. `--list` asks the
//! service for the room directory instead of joining.

use std::{
    path::{Path, PathBuf},
    thread::sleep,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail, ensure};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use unibus::{
    Envelope, Message,
    client::{Client, ClientConfig},
    kinds,
};
use unibus_room::{
    BusClass, CAP_MEMBER, ERROR_KIND, JOIN_KIND, Join, LEAVE_KIND, LIST_KIND, LIST_REPLY_KIND,
    ListReply, MEMBER_KIND, MESSAGE_KIND, MemberChange, MemberEvent, Presence, Presenter,
    RoomMessage, WELCOME_KIND, Welcome,
};

struct Args {
    config: PathBuf,
    room: String,
    name: String,
    presenter: Presenter,
    seconds: u64,
    list: bool,
}

fn main() -> Result<()> {
    let args = parse_args()?;
    let client = Client::connect(client_config(&args.config)?)?;

    if args.list {
        return list(&client);
    }

    let join = Join {
        room: args.room.clone(),
        space: None,
        display_name: args.name.clone(),
        presenter: args.presenter,
        presence: Presence::Active,
        audio: BusClass::Speech,
    };
    send(&client, JOIN_KIND, serde_json::to_value(join)?)?;

    let deadline = Instant::now() + Duration::from_secs(args.seconds);
    let mut joined = false;
    while Instant::now() < deadline {
        match client.try_read() {
            Ok(Some((envelope, _))) => match envelope.message.kind.as_str() {
                WELCOME_KIND => {
                    let welcome: Welcome = decode(&envelope)?;
                    print_welcome(&welcome);
                    joined = true;
                }
                MEMBER_KIND => {
                    let event: MemberEvent = decode(&envelope)?;
                    print_event(&event);
                }
                MESSAGE_KIND => {
                    let message: RoomMessage = decode(&envelope)?;
                    println!("message {}: {}", message.display_name, message.text);
                }
                LIST_REPLY_KIND => {
                    let reply: ListReply = decode(&envelope)?;
                    print_list(&reply);
                }
                ERROR_KIND => {
                    let error: Value = decode(&envelope)?;
                    bail!("room error: {error}");
                }
                kinds::REPLY
                    if envelope.message.body.get("success") == Some(&Value::Bool(false)) =>
                {
                    eprintln!("hub rejected a room command: {:?}", envelope.message.body);
                }
                kinds::REPLY => {}
                _ => {}
            },
            Ok(None) => sleep(Duration::from_millis(20)),
            Err(error) => {
                eprintln!("connection lost: {error:#}");
                break;
            }
        }
    }
    if joined {
        let _ = send(&client, LEAVE_KIND, json!({}));
    }
    Ok(())
}

fn list(client: &Client) -> Result<()> {
    send(client, LIST_KIND, json!({}))?;
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if let Some((envelope, _)) = client.try_read()? {
            match envelope.message.kind.as_str() {
                LIST_REPLY_KIND => {
                    let reply: ListReply = decode(&envelope)?;
                    print_list(&reply);
                    return Ok(());
                }
                ERROR_KIND => {
                    let error: Value = decode(&envelope)?;
                    bail!("room error: {error}");
                }
                _ => {}
            }
        }
    }
    bail!("no room list reply")
}

fn send(client: &Client, kind: &str, body: Value) -> Result<()> {
    let envelope = client.envelope(None, Message::new(kind, body))?;
    client.send(&envelope, &[])
}

fn decode<T: DeserializeOwned>(envelope: &Envelope) -> Result<T> {
    serde_json::from_value(Value::Object(envelope.message.body.clone()))
        .with_context(|| format!("decode {}", envelope.message.kind))
}

fn print_welcome(welcome: &Welcome) {
    println!(
        "joined {} (space {}) revision {}",
        welcome.room, welcome.space, welcome.revision
    );
    for member in &welcome.members {
        println!(
            "  {}  {}  {:?}/{:?}  {:?}{}",
            member.id,
            member.display_name,
            member.role,
            member.presenter,
            member.presence,
            if member.speaking { "  speaking" } else { "" }
        );
    }
    for bus in &welcome.buses {
        println!(
            "  bus {:?}  {} ms  {} publishers{}",
            bus.class,
            bus.buffer_ms,
            bus.publishers,
            if bus.lossless { "  lossless" } else { "" }
        );
    }
}

fn print_event(event: &MemberEvent) {
    let change = match event.change {
        MemberChange::Joined => "joined",
        MemberChange::Left => "left",
        MemberChange::Updated => "updated",
    };
    println!(
        "member {change} {}  speaking={}  revision {}",
        event.member.id, event.member.speaking, event.revision
    );
}

fn print_list(reply: &ListReply) {
    if reply.rooms.is_empty() {
        println!("no rooms");
        return;
    }
    for room in &reply.rooms {
        println!(
            "{} (space {})  members={}  revision={}",
            room.room, room.space, room.members, room.revision
        );
    }
}

fn parse_args() -> Result<Args> {
    let mut argv = std::env::args().skip(1);
    let config = argv.next().context(
        "usage: room-probe CLIENT.json ROOM [--name NAME] [--presenter canvas|holodeck] [--seconds N] [--list]",
    )?;
    let room = argv.next().context("missing ROOM")?;
    let mut args = Args {
        config: config.into(),
        room,
        name: "probe".into(),
        presenter: Presenter::Canvas,
        seconds: 30,
        list: false,
    };
    while let Some(arg) = argv.next() {
        match arg.as_str() {
            "--name" => args.name = argv.next().context("--name needs a value")?,
            "--presenter" => {
                args.presenter = match argv.next().as_deref() {
                    Some("canvas") => Presenter::Canvas,
                    Some("holodeck") => Presenter::Holodeck,
                    _ => bail!("--presenter must be canvas or holodeck"),
                };
            }
            "--seconds" => {
                args.seconds = argv
                    .next()
                    .context("--seconds needs a value")?
                    .parse()
                    .context("--seconds must be a number")?;
            }
            "--list" => args.list = true,
            other => bail!("unknown argument {other}"),
        }
    }
    Ok(args)
}

fn client_config(path: &Path) -> Result<ClientConfig> {
    let bytes = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
    let mut config: ClientConfig =
        serde_json::from_slice(&bytes).with_context(|| format!("parse {}", path.display()))?;
    config.resolve_token_file(path);
    config.schema_version = unibus::SCHEMA_VERSION_V2;
    config.read_timeout_ms = 200;
    ensure!(
        !config.zones.is_empty(),
        "client config needs at least one zone"
    );
    ensure!(
        config
            .capabilities
            .iter()
            .any(|capability| capability == CAP_MEMBER),
        "client config must claim {CAP_MEMBER}"
    );
    Ok(config)
}
