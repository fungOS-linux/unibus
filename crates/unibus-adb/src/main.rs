use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;
use std::{
    collections::BTreeMap,
    io::{Read, Write},
    net::{Shutdown, SocketAddr, TcpListener, TcpStream},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, SyncSender},
    },
    thread,
    time::{Duration, Instant},
};
use unibus::{
    Message,
    client::{Client, ClientConfig},
};
use unibus_adb::{
    KIND, MAX_CHUNK, MAX_SESSIONS, Operation, Packet, QUEUE_DEPTH, ReceiveOrder, SendWindow,
};
mod discovery;
mod setup;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    bus: ClientConfig,
    mode: Mode,
}
#[derive(Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
enum Mode {
    Serve {
        adb_endpoint: SocketAddr,
        allowed_clients: Vec<String>,
        #[serde(default)]
        adb_binary: Option<std::path::PathBuf>,
        #[serde(default = "enabled")]
        publish_inventory: bool,
        #[serde(default)]
        publish_services: bool,
    },
    Attach {
        listen: SocketAddr,
        destination: String,
    },
}
struct Session {
    socket: TcpStream,
    queue: SyncSender<(Packet, Vec<u8>)>,
    stopped: Arc<AtomicBool>,
    read_done: Arc<AtomicBool>,
    write_done: Arc<AtomicBool>,
    activity: Arc<std::sync::Mutex<Instant>>,
    order: ReceiveOrder,
    window: Arc<SendWindow>,
}
fn enabled() -> bool {
    true
}
impl Drop for Session {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Release);
        let _ = self.socket.shutdown(Shutdown::Both);
    }
}
fn send(bus: &Client, peer: &str, packet: &Packet, payload: &[u8]) -> Result<()> {
    packet.validate(payload)?;
    let mut envelope = bus.envelope(
        Some(peer),
        Message::new(KIND, serde_json::to_value(packet)?),
    )?;
    envelope.ttl_ms = Some(10_000);
    envelope.payload_bytes = payload.len();
    bus.send(&envelope, payload)
}

fn publish_inventory_message(
    bus: &Client,
    pending: &std::sync::Mutex<std::collections::BTreeSet<String>>,
    destination: Option<&str>,
    kind: &str,
    value: serde_json::Value,
) -> Result<()> {
    let envelope = bus.envelope(destination, Message::new(kind, value))?;
    let mut guard = pending
        .lock()
        .map_err(|_| anyhow::anyhow!("inventory receipt mutex poisoned"))?;
    ensure!(guard.len() < 128, "inventory receipt backlog full");
    guard.insert(envelope.request_id.clone());
    drop(guard);
    bus.send(&envelope, &[])
}
fn start_session(bus: Arc<Client>, peer: String, id: String, socket: TcpStream) -> Result<Session> {
    socket.set_nodelay(true)?;
    socket.set_read_timeout(Some(Duration::from_millis(250)))?;
    socket.set_write_timeout(Some(Duration::from_secs(5)))?;
    let stopped = Arc::new(AtomicBool::new(false));
    let read_done = Arc::new(AtomicBool::new(false));
    let write_done = Arc::new(AtomicBool::new(false));
    let activity = Arc::new(std::sync::Mutex::new(Instant::now()));
    let (queue, incoming) = mpsc::sync_channel::<(Packet, Vec<u8>)>(QUEUE_DEPTH);
    let mut reader = socket.try_clone()?;
    let mut writer = socket.try_clone()?;
    let read_bus = bus.clone();
    let read_peer = peer.clone();
    let read_id = id.clone();
    let read_stop = stopped.clone();
    let done = read_done.clone();
    let active = activity.clone();
    let window = Arc::new(SendWindow::default());
    let read_window = window.clone();
    thread::Builder::new()
        .name("adb-read".into())
        .spawn(move || {
            let result = (|| -> Result<()> {
                let mut buffer = [0; MAX_CHUNK];
                let mut sequence = 1;
                while !read_stop.load(Ordering::Acquire) {
                    let waiting = Instant::now();
                    while read_window.full() && !read_stop.load(Ordering::Acquire) {
                        ensure!(
                            waiting.elapsed() < Duration::from_secs(30),
                            "ADB credit timeout"
                        );
                        thread::sleep(Duration::from_millis(2));
                    }
                    if read_stop.load(Ordering::Acquire) {
                        break;
                    }
                    let n = match reader.read(&mut buffer) {
                        Ok(n) => n,
                        Err(e)
                            if matches!(
                                e.kind(),
                                std::io::ErrorKind::TimedOut
                                    | std::io::ErrorKind::WouldBlock
                                    | std::io::ErrorKind::Interrupted
                            ) =>
                        {
                            continue;
                        }
                        Err(e) => return Err(e.into()),
                    };
                    *active.lock().unwrap() = Instant::now();
                    if n > 0 {
                        read_window.sent(sequence);
                    }
                    send(
                        &read_bus,
                        &read_peer,
                        &Packet {
                            session: read_id.clone(),
                            operation: if n == 0 {
                                Operation::Eof
                            } else {
                                Operation::Data
                            },
                            sequence,
                        },
                        &buffer[..n],
                    )?;
                    if n == 0 {
                        break;
                    }
                    sequence += 1;
                }
                Ok(())
            })();
            done.store(true, Ordering::Release);
            if let Err(error) = result {
                eprintln!("ADB read failed: {error:#}");
                read_stop.store(true, Ordering::Release);
                let _ = send(
                    &read_bus,
                    &read_peer,
                    &Packet {
                        session: read_id,
                        operation: Operation::Close,
                        sequence: 0,
                    },
                    &[],
                );
                let _ = reader.shutdown(Shutdown::Both);
            }
        })?;
    let write_stop = stopped.clone();
    let done = write_done.clone();
    thread::Builder::new()
        .name("adb-write".into())
        .spawn(move || {
            let result = (|| -> Result<()> {
                while !write_stop.load(Ordering::Acquire) {
                    match incoming.recv_timeout(Duration::from_millis(250)) {
                        Ok((packet, payload)) => {
                            if packet.operation == Operation::Eof {
                                writer.shutdown(Shutdown::Write)?;
                                break;
                            }
                            writer.write_all(&payload)?;
                            send(
                                &bus,
                                &peer,
                                &Packet {
                                    operation: Operation::Ack,
                                    ..packet
                                },
                                &[],
                            )?;
                        }
                        Err(mpsc::RecvTimeoutError::Timeout) => continue,
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    }
                }
                Ok(())
            })();
            done.store(true, Ordering::Release);
            if let Err(error) = result {
                eprintln!("ADB write failed: {error:#}");
                write_stop.store(true, Ordering::Release);
                let _ = send(
                    &bus,
                    &peer,
                    &Packet {
                        session: id,
                        operation: Operation::Close,
                        sequence: 0,
                    },
                    &[],
                );
                let _ = writer.shutdown(Shutdown::Both);
            }
        })?;
    Ok(Session {
        socket,
        queue,
        stopped,
        read_done,
        write_done,
        activity,
        order: ReceiveOrder::new(),
        window,
    })
}
fn main() -> Result<()> {
    match std::env::args().nth(1).as_deref() {
        Some("--help" | "-h" | "help") => {
            println!(
                "unibus-adb init NEW_DIRECTORY [--server-id ID] [--client-id ID] [--installation-id ID] [--adb-binary ABSOLUTE_PATH]\nunibus-adb hub ROUTER.json\nunibus-adb CONFIG.json\nunibus-adb inventory [127.0.0.1:5037]\nunibus-adb discover\nunibus-adb tunnel MYCELIUM_TARGET\nunibus-adb tunnel --ssh USER@HOST\n\nTokens are relative to the config file. Init does not overwrite existing directories.\nStart adb manually unless --adb-binary is supplied. Mycelium is optional."
            );
            return Ok(());
        }
        Some("discover") => return discovery::discover(),
        Some("tunnel") => {
            if std::env::args().nth(2).as_deref() == Some("--ssh") {
                return discovery::static_tunnel(
                    &std::env::args()
                        .nth(3)
                        .context("tunnel --ssh requires USER@HOST")?,
                );
            }
            return discovery::tunnel(
                &std::env::args()
                    .nth(2)
                    .context("tunnel requires a Mycelium target")?,
            );
        }
        Some("init") => {
            return setup::init_with_options(
                std::path::Path::new(
                    &std::env::args()
                        .nth(2)
                        .context("init requires a new directory")?,
                ),
                &setup::Options::parse(std::env::args_os().skip(3))?,
            );
        }
        Some("hub") => {
            let path = std::env::args()
                .nth(2)
                .context("hub requires router.json")?;
            return Arc::new(unibus_router::Hub::new(
                unibus_router::config::HubConfig::read(std::path::Path::new(&path))?,
            )?)
            .serve();
        }
        Some("inventory") => {
            let endpoint = std::env::args()
                .nth(2)
                .unwrap_or_else(|| "127.0.0.1:5037".into())
                .parse()?;
            println!(
                "{}",
                serde_json::to_string_pretty(&unibus_adb::inventory::enumerate(endpoint)?)?
            );
            return Ok(());
        }
        _ => {}
    }
    let path = std::path::PathBuf::from(
        std::env::args_os()
            .nth(1)
            .context("usage: unibus-adb <config.json>")?,
    );
    let mut config: Config = serde_json::from_slice(&std::fs::read(&path)?)?;
    // Copying attach.json and its one token is enough on another machine.
    config.bus.resolve_token_file(&path);
    ensure!(
        config.bus.schema_version == unibus::SCHEMA_VERSION_V2,
        "ADB requires schema version 2"
    );
    ensure!(
        config
            .bus
            .hub_endpoint
            .parse::<SocketAddr>()
            .is_ok_and(|address| address.ip().is_loopback()),
        "ADB byte transport requires a loopback hub endpoint; use an authenticated tunnel for remote access"
    );
    let (accept_tx, accepts) = mpsc::sync_channel(MAX_SESSIONS);
    match &config.mode {
        Mode::Serve {
            adb_endpoint,
            allowed_clients,
            adb_binary,
            publish_inventory,
            publish_services,
            ..
        } => {
            ensure!(
                !publish_services || (*publish_inventory && config.bus.schema_version == 2),
                "service publication requires inventory polling and schema v2 grants"
            );
            ensure!(
                adb_endpoint.ip().is_loopback() && !allowed_clients.is_empty(),
                "ADB backend must be loopback and clients explicitly allowed"
            );
            if let Some(binary) = adb_binary {
                ensure!(
                    binary.is_absolute(),
                    "ADB binary must be an absolute trusted path"
                );
                ensure!(
                    std::process::Command::new(binary)
                        .args(["-L", &format!("tcp:{adb_endpoint}"), "start-server"])
                        .status()?
                        .success(),
                    "failed to start local ADB server"
                );
            }
            eprintln!(
                "Serving local ADB backend {adb_endpoint}; {} allowed clients",
                allowed_clients.len()
            );
        }
        Mode::Attach {
            listen,
            destination,
        } => {
            ensure!(
                listen.ip().is_loopback() && !destination.is_empty(),
                "local ADB listener must be loopback with an explicit destination"
            );
            let listener = TcpListener::bind(listen)?;
            eprintln!(
                "ADB available at {listen}; use adb -H {} -P {}",
                listen.ip(),
                listen.port()
            );
            thread::Builder::new()
                .name("adb-accept".into())
                .spawn(move || {
                    for socket in listener.incoming() {
                        match socket {
                            Ok(socket) => {
                                if accept_tx.try_send(socket).is_err() {
                                    eprintln!("ADB accept queue full/disconnected");
                                }
                            }
                            Err(error) => {
                                eprintln!("ADB accept failed: {error}");
                                break;
                            }
                        }
                    }
                })?;
        }
    }
    config.bus.read_timeout_ms = 10;
    let bus = Arc::new(Client::connect(config.bus)?);
    eprintln!("ADB bridge registered: {}", bus.config().producer_id);
    let inventory_pending = Arc::new(std::sync::Mutex::new(
        std::collections::BTreeSet::<String>::new(),
    ));
    if let Mode::Serve {
        adb_endpoint,
        allowed_clients,
        publish_inventory: true,
        publish_services,
        ..
    } = &config.mode
    {
        let bus = bus.clone();
        let endpoint = *adb_endpoint;
        let clients = allowed_clients.clone();
        let pending = inventory_pending.clone();
        let services = *publish_services;
        thread::Builder::new().name("adb-inventory".into()).spawn(move || {
            loop {
                let now = unibus::envelope::now_ms();
                let value = match unibus_adb::inventory::enumerate(endpoint) {
                    Ok(devices) => serde_json::json!({"schema_version":1,"producer_id":bus.config().producer_id,
                        "generation":now,"observed_at_unix_ms":now,"valid_until_unix_ms":now+6000,
                        "state":"available","devices":devices}),
                    Err(error) => { eprintln!("ADB inventory unavailable: {error:#}"); serde_json::json!({"schema_version":1,
                        "producer_id":bus.config().producer_id,"generation":now,"observed_at_unix_ms":now,
                        "valid_until_unix_ms":now+6000,"state":"unavailable","devices":[]}) }
                };
                for peer in &clients {
                    let result = publish_inventory_message(&bus, &pending, Some(peer), unibus_adb::inventory::INVENTORY_KIND, value.clone());
                    if let Err(error) = result { eprintln!("ADB inventory publish failed: {error:#}"); return; }
                }
                if services {
                    let result = (|| -> Result<()> {
                        let inventory: unibus_adb::inventory::Inventory = serde_json::from_value(value)?;
                        let (catalog, health) = inventory.services()?;
                        publish_inventory_message(&bus, &pending, None, stream_descriptors::service::SERVICE_CATALOG_KIND, serde_json::to_value(catalog)?)?;
                        publish_inventory_message(&bus, &pending, None, stream_descriptors::service::SERVICE_HEALTH_KIND, serde_json::to_value(health)?)
                    })();
                    if let Err(error) = result { eprintln!("ADB service publication failed: {error:#}"); return; }
                }
                thread::sleep(Duration::from_secs(2));
            }
        })?;
    }
    let mut sessions: BTreeMap<(String, String), Session> = BTreeMap::new();
    let mut last_inventory = None;
    loop {
        sessions.retain(|_, session| {
            !session.stopped.load(Ordering::Acquire)
                && !(session.read_done.load(Ordering::Acquire)
                    && session.write_done.load(Ordering::Acquire)
                    && session.window.drained())
                && session.activity.lock().unwrap().elapsed() < Duration::from_secs(300)
        });
        if let Mode::Attach { destination, .. } = &config.mode {
            while let Ok(socket) = accepts.try_recv() {
                if sessions.len() >= MAX_SESSIONS {
                    eprintln!("ADB session limit reached");
                    continue;
                }
                let id = bus
                    .envelope(
                        Some(destination),
                        Message::new(KIND, serde_json::Value::Null),
                    )?
                    .request_id;
                send(
                    &bus,
                    destination,
                    &Packet {
                        session: id.clone(),
                        operation: Operation::Open,
                        sequence: 0,
                    },
                    &[],
                )?;
                let session = start_session(bus.clone(), destination.clone(), id.clone(), socket)?;
                sessions.insert((destination.clone(), id), session);
            }
        }
        let Some((envelope, payload)) = bus.try_read()? else {
            continue;
        };
        if envelope.message.kind == unibus::kinds::V1_REPLY {
            if let Some(id) = envelope
                .message
                .body
                .get("request_id")
                .and_then(|v| v.as_str())
                && inventory_pending.lock().unwrap().remove(id)
            {
                if envelope
                    .message
                    .body
                    .get("success")
                    .and_then(|v| v.as_bool())
                    == Some(false)
                {
                    eprintln!(
                        "ADB inventory observer unavailable: {:?}",
                        envelope.message.body
                    );
                }
                continue;
            }
            if envelope
                .message
                .body
                .get("success")
                .and_then(|v| v.as_bool())
                == Some(false)
            {
                bail!("Unibus rejected ADB message: {:?}", envelope.message.body);
            }
            continue;
        }
        if envelope.message.kind == unibus_adb::inventory::INVENTORY_KIND {
            if let Mode::Attach { destination, .. } = &config.mode
                && &envelope.producer_id == destination
            {
                let inventory = serde_json::from_value::<unibus_adb::inventory::Inventory>(
                    serde_json::Value::Object(envelope.message.body.clone()),
                );
                if !inventory
                    .is_ok_and(|i| i.validate(destination, unibus::envelope::now_ms()).is_ok())
                {
                    eprintln!("Rejected malformed or expired ADB inventory");
                    continue;
                }
                let signature = (
                    envelope.message.body.get("state").cloned(),
                    envelope.message.body.get("devices").cloned(),
                );
                if last_inventory.as_ref() != Some(&signature) {
                    eprintln!(
                        "ADB hotplug inventory: {}",
                        serde_json::Value::Object(envelope.message.body)
                    );
                    last_inventory = Some(signature);
                }
            }
            continue;
        }
        if envelope.message.kind != KIND {
            continue;
        }
        let peer = envelope.producer_id;
        let allowed = match &config.mode {
            Mode::Serve {
                allowed_clients, ..
            } => allowed_clients.contains(&peer),
            Mode::Attach { destination, .. } => destination == &peer,
        };
        if !allowed {
            eprintln!("Rejected ADB frame from unapproved producer {peer}");
            continue;
        }
        let packet = match serde_json::from_value::<Packet>(serde_json::Value::Object(
            envelope.message.body,
        )) {
            Ok(p) if p.validate(&payload).is_ok() => p,
            _ => {
                eprintln!("Rejected malformed ADB frame");
                continue;
            }
        };
        let key = (peer.clone(), packet.session.clone());
        if packet.operation == Operation::Close {
            sessions.remove(&key);
            continue;
        }
        if packet.operation == Operation::Open {
            if let Mode::Serve { adb_endpoint, .. } = &config.mode
                && sessions.len() < MAX_SESSIONS
                && !sessions.contains_key(&key)
            {
                match TcpStream::connect_timeout(adb_endpoint, Duration::from_secs(1))
                    .map_err(anyhow::Error::from)
                    .and_then(|s| {
                        start_session(bus.clone(), peer.clone(), packet.session.clone(), s)
                    }) {
                    Ok(session) => {
                        sessions.insert(key, session);
                        continue;
                    }
                    Err(error) => eprintln!("ADB backend unavailable: {error:#}"),
                }
            }
            send(
                &bus,
                &peer,
                &Packet {
                    operation: Operation::Close,
                    ..packet
                },
                &[],
            )?;
            continue;
        }
        let accepted = sessions.get_mut(&key).is_some_and(|session| {
            if packet.operation == Operation::Ack {
                return session.window.acknowledge(packet.sequence).is_ok();
            }
            if session.order.accept(&packet).is_err() {
                return false;
            }
            *session.activity.lock().unwrap() = Instant::now();
            session.queue.try_send((packet.clone(), payload)).is_ok()
        });
        if !accepted {
            eprintln!("Closing ADB session: missing, out of order, or backpressure limit");
            sessions.remove(&key);
            send(
                &bus,
                &peer,
                &Packet {
                    operation: Operation::Close,
                    ..packet
                },
                &[],
            )?;
        }
    }
}
