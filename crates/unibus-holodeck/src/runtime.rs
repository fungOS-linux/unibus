use crate::{MAX_CLIENTS, Reassembler, send_datagram};
use anyhow::{Context, Result, ensure};
use holodeck_discovery::{DISCOVERY_PORT, PROBE, decode_advert, decode_probe, encode_advert};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::UdpSocket,
};
use unibus_quic::{
    PeerCredentials, PemPeerCredentials, QuicEndpoint, certificate_key_binding,
    mutual_client_config, mutual_server_config,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Tls {
    pub ca: PathBuf,
    pub certificate: PathBuf,
    pub key: PathBuf,
    pub expected_peer_binding: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Export {
    pub room_id: String,
    pub label: String,
    pub target: SocketAddr,
    pub discovery: SocketAddr,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Import {
    pub room_id: String,
    pub remote: SocketAddr,
    pub server_name: String,
    pub lan_bind: SocketAddr,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum Config {
    Export {
        bind: SocketAddr,
        tls: Tls,
        rooms: Vec<Export>,
    },
    Import {
        lan_address: Ipv4Addr,
        #[serde(default = "default_discovery_port")]
        discovery_port: u16,
        #[serde(default)]
        allowed_clients: Vec<Ipv4Addr>,
        #[serde(default)]
        allowed_subnets: Vec<ipnet::Ipv4Net>,
        tls: Tls,
        rooms: Vec<Import>,
    },
}

fn default_discovery_port() -> u16 {
    DISCOVERY_PORT
}

#[derive(Clone)]
struct ClientPolicy {
    addresses: Vec<Ipv4Addr>,
    subnets: Vec<ipnet::Ipv4Net>,
}

impl ClientPolicy {
    fn allows(&self, address: IpAddr) -> bool {
        let IpAddr::V4(ip) = address else {
            return false;
        };
        self.addresses.contains(&ip) || self.subnets.iter().any(|net| net.contains(&ip))
    }
}

fn private_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            ip.is_private()
                || ip.is_loopback()
                || (ip.octets()[0] == 100 && (64..=127).contains(&ip.octets()[1]))
        }
        _ => false,
    }
}

impl Config {
    pub fn validate(&self) -> Result<()> {
        let (tls, ids): (&Tls, Vec<&str>) = match self {
            Self::Export { bind, tls, rooms } => {
                ensure!(
                    private_ip(bind.ip()) && bind.port() > 0,
                    "export must bind an explicit private IPv4 endpoint"
                );
                ensure!(
                    !rooms.is_empty() && rooms.len() <= 16,
                    "expected 1..16 exports"
                );
                for r in rooms {
                    ensure!(
                        private_ip(r.target.ip())
                            && r.target.port() > 0
                            && private_ip(r.discovery.ip())
                            && r.discovery.port() > 0,
                        "export target/discovery must be explicit private endpoints"
                    );
                    ensure!(
                        !r.label.is_empty() && r.label.len() <= 128,
                        "invalid room label"
                    );
                }
                (tls, rooms.iter().map(|r| r.room_id.as_str()).collect())
            }
            Self::Import {
                lan_address,
                discovery_port,
                allowed_clients,
                allowed_subnets,
                tls,
                rooms,
            } => {
                ensure!(*discovery_port > 0, "discovery port must be nonzero");
                ensure!(
                    private_ip((*lan_address).into()),
                    "LAN address must be explicit private IPv4"
                );
                ensure!(
                    (!allowed_clients.is_empty() || !allowed_subnets.is_empty())
                        && allowed_clients.len() <= 64
                        && allowed_subnets.len() <= 8,
                    "explicit bounded client or LAN subnet allowlist required"
                );
                ensure!(
                    allowed_clients.iter().all(|ip| private_ip((*ip).into())),
                    "clients must use private IPv4"
                );
                ensure!(
                    allowed_subnets.iter().all(|net| net.contains(lan_address)
                        && private_ip(net.network().into())
                        && private_ip(net.broadcast().into())),
                    "allowed subnet must be private and contain the selected LAN address"
                );
                ensure!(
                    !rooms.is_empty() && rooms.len() <= 16,
                    "expected 1..16 imports"
                );
                let mut ports = std::collections::BTreeSet::new();
                for r in rooms {
                    ensure!(
                        r.lan_bind.ip() == IpAddr::V4(*lan_address)
                            && r.lan_bind.port() > 0
                            && r.lan_bind.port() != *discovery_port,
                        "relay must bind selected LAN address and a distinct port"
                    );
                    ensure!(
                        private_ip(r.remote.ip())
                            && r.remote.port() > 0
                            && !r.server_name.is_empty(),
                        "invalid private remote"
                    );
                    ensure!(ports.insert(r.lan_bind.port()), "duplicate relay port");
                }
                (tls, rooms.iter().map(|r| r.room_id.as_str()).collect())
            }
        };
        ensure!(
            ids.iter().all(|id| !id.is_empty()
                && id.len() <= 128
                && id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_/.".contains(&b))),
            "invalid room ID"
        );
        ensure!(
            ids.iter().collect::<std::collections::BTreeSet<_>>().len() == ids.len(),
            "duplicate room ID"
        );
        ensure!(
            tls.expected_peer_binding.starts_with("sha256:")
                && tls.expected_peer_binding.len() == 71
                && tls.expected_peer_binding[7..]
                    .bytes()
                    .all(|b| b.is_ascii_hexdigit()),
            "expected explicit SHA256 peer certificate binding"
        );
        Ok(())
    }
}

fn material(tls: &Tls) -> Result<unibus_quic::PeerCredentialMaterial> {
    PemPeerCredentials::new(&tls.ca, &tls.certificate, &tls.key).load()
}

async fn host_ready(room: &Export) -> bool {
    let result: Result<bool> = async {
        let socket = UdpSocket::bind("0.0.0.0:0").await?;
        socket.connect(room.discovery).await?;
        socket.send(&PROBE).await?;
        let mut bytes = [0; 64];
        let size =
            tokio::time::timeout(Duration::from_millis(500), socket.recv(&mut bytes)).await??;
        Ok(decode_advert(&bytes[..size]) == Some(room.target.port()))
    }
    .await;
    result.unwrap_or(false)
}

async fn read_json<T: for<'a> Deserialize<'a>>(stream: &mut quinn::RecvStream) -> Result<T> {
    let length = stream.read_u32().await? as usize;
    ensure!(length > 0 && length <= 4096, "control frame too large");
    let mut bytes = vec![0; length];
    stream.read_exact(&mut bytes).await?;
    Ok(serde_json::from_slice(&bytes)?)
}
async fn write_json(stream: &mut quinn::SendStream, value: &impl Serialize) -> Result<()> {
    let bytes = serde_json::to_vec(value)?;
    ensure!(bytes.len() <= 4096, "control frame too large");
    stream.write_u32(bytes.len() as u32).await?;
    stream.write_all(&bytes).await?;
    Ok(())
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    version: u32,
    room_id: String,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Lease {
    version: u32,
    room_id: String,
    label: String,
    generation: String,
    ready: bool,
    ttl_ms: u32,
}

struct Client {
    socket: Arc<UdpSocket>,
    last_seen: Instant,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Client {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// One authenticated connection represents one authorized room-host export.
/// This transports the host endpoint, not an authorization boundary between
/// logical room names hosted by the same Holodeck server.
async fn serve(peer: unibus_quic::QuicPeer, rooms: Vec<Export>, expected: String) -> Result<()> {
    ensure!(
        certificate_key_binding(&peer.peer_certificate()?) == expected,
        "importer certificate binding denied"
    );
    let connection = peer.connection();
    let (mut tx, mut rx) =
        tokio::time::timeout(Duration::from_secs(5), connection.accept_bi()).await??;
    let request: Request =
        tokio::time::timeout(Duration::from_secs(5), read_json(&mut rx)).await??;
    ensure!(request.version == 1, "unsupported relay version");
    let room = rooms
        .into_iter()
        .find(|r| r.room_id == request.room_id)
        .context("room export not authorized")?;
    let generation = format!(
        "{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos()
    );
    let mut heartbeat = tokio::time::interval(Duration::from_secs(2));
    let mut ready = false;
    let mut clients: BTreeMap<u64, Client> = BTreeMap::new();
    let mut reassembler = Reassembler::default();
    let mut rate_start = Instant::now();
    let mut packets = 0usize;
    loop {
        tokio::select! {
            _ = heartbeat.tick() => {
                ready = host_ready(&room).await;
                if !ready { clients.clear(); }
                clients.retain(|_, c| c.last_seen.elapsed() < Duration::from_secs(60) && !c.task.is_finished());
                tokio::time::timeout(Duration::from_secs(2), write_json(&mut tx, &Lease { version: 1, room_id: room.room_id.clone(), label: room.label.clone(), generation: generation.clone(), ready, ttl_ms: 6000 })).await??;
            }
            incoming = connection.read_datagram() => {
                let packet = incoming?;
                if rate_start.elapsed() >= Duration::from_secs(1) { packets = 0; rate_start = Instant::now(); }
                packets += 1;
                if packets > 8000 || !ready { continue; }
                let assembled = match reassembler.accept(&packet, Instant::now()) { Ok(v) => v, Err(e) => { eprintln!("relay fragment rejected: {e}"); continue; } };
                let Some((id, bytes)) = assembled else { continue; };
                if !clients.contains_key(&id) {
                    if clients.len() >= MAX_CLIENTS { continue; }
                    let socket = Arc::new(UdpSocket::bind("0.0.0.0:0").await?);
                    socket.connect(room.target).await?;
                    let recv_socket = socket.clone();
                    let outbound = connection.clone();
                    let task = tokio::spawn(async move {
                        let mut bytes = vec![0; holodeck_discovery::MAX_ROOM_PACKET_BYTES + 1];
                        let mut sequence = 0u32;
                        loop {
                            match recv_socket.recv(&mut bytes).await {
                                Ok(n) if n <= holodeck_discovery::MAX_ROOM_PACKET_BYTES => {
                                    sequence = sequence.wrapping_add(1);
                                    if let Err(e) = send_datagram(&outbound, id, sequence, &bytes[..n]) { eprintln!("relay return failed: {e}"); break; }
                                }
                                Ok(_) => eprintln!("oversize upstream UDP datagram rejected"),
                                Err(e) => { eprintln!("upstream receive failed: {e}"); break; }
                            }
                        }
                    });
                    clients.insert(id, Client { socket, last_seen: Instant::now(), task });
                }
                let client = clients.get_mut(&id).expect("client exists");
                client.last_seen = Instant::now();
                client.socket.send(&bytes).await?;
            }
        }
    }
}

#[cfg(test)]
mod configuration_tests {
    use super::*;

    fn importer() -> Config {
        serde_json::from_str(include_str!("../../../config/holodeck-import.example.json")).unwrap()
    }

    #[test]
    fn examples_validate_without_opening_listeners() {
        importer().validate().unwrap();
        let exporter: Config =
            serde_json::from_str(include_str!("../../../config/holodeck-export.example.json"))
                .unwrap();
        exporter.validate().unwrap();
    }

    #[test]
    fn authorized_lan_allows_roaming_clients_not_other_sites() {
        let policy = ClientPolicy {
            addresses: Vec::new(),
            subnets: vec!["192.168.1.0/24".parse().unwrap()],
        };
        assert!(policy.allows("192.168.1.6".parse().unwrap()));
        assert!(policy.allows("192.168.1.200".parse().unwrap()));
        assert!(!policy.allows("192.168.2.6".parse().unwrap()));
        assert!(!policy.allows("100.83.7.116".parse().unwrap()));
        let mut config = importer();
        if let Config::Import {
            allowed_clients,
            allowed_subnets,
            ..
        } = &mut config
        {
            allowed_clients.clear();
            allowed_subnets.push("192.168.1.0/24".parse().unwrap());
        }
        assert!(config.validate().is_ok());
        if let Config::Import {
            allowed_subnets, ..
        } = &mut config
        {
            *allowed_subnets = vec!["0.0.0.0/0".parse().unwrap()];
        }
        assert!(config.validate().is_err());
        if let Config::Import {
            allowed_subnets, ..
        } = &mut config
        {
            *allowed_subnets = vec!["192.168.2.0/24".parse().unwrap()];
        }
        assert!(config.validate().is_err());
    }

    #[test]
    fn wildcard_clients_wrong_interface_and_missing_pin_rejected() {
        let mut config = importer();
        if let Config::Import {
            allowed_clients, ..
        } = &mut config
        {
            allowed_clients.clear();
        }
        assert!(config.validate().is_err());
        config = importer();
        if let Config::Import { lan_address, .. } = &mut config {
            *lan_address = Ipv4Addr::UNSPECIFIED;
        }
        assert!(config.validate().is_err());
        config = importer();
        if let Config::Import { rooms, .. } = &mut config {
            rooms[0].lan_bind = "192.168.2.10:6790".parse().unwrap();
        }
        assert!(config.validate().is_err());
        config = importer();
        if let Config::Import { tls, .. } = &mut config {
            tls.expected_peer_binding.clear();
        }
        assert!(config.validate().is_err());
    }
}

pub async fn run(config: Config) -> Result<()> {
    config.validate()?;
    match config {
        Config::Export { bind, tls, rooms } => {
            let m = material(&tls)?;
            let endpoint = QuicEndpoint::server(
                bind,
                mutual_server_config(m.certificates, m.private_key, m.trust_roots)?,
            )?;
            eprintln!(
                "holodeck export listening on {bind}; {} opted-in host endpoints",
                rooms.len()
            );
            let permits = Arc::new(tokio::sync::Semaphore::new(16));
            loop {
                match endpoint.accept().await {
                    Ok(peer) => {
                        let Ok(permit) = permits.clone().try_acquire_owned() else {
                            peer.connection().close(1u32.into(), b"capacity");
                            continue;
                        };
                        let rooms = rooms.clone();
                        let expected = tls.expected_peer_binding.clone();
                        tokio::spawn(async move {
                            let _permit = permit;
                            if let Err(e) = serve(peer, rooms, expected).await {
                                eprintln!("holodeck export session ended: {e:#}");
                            }
                        });
                    }
                    Err(e) => eprintln!("holodeck TLS admission failed: {e:#}"),
                }
            }
        }
        Config::Import {
            lan_address,
            discovery_port,
            allowed_clients,
            allowed_subnets,
            tls,
            rooms,
        } => {
            ensure!(
                cfg!(target_os = "linux") || lan_address.is_loopback(),
                "LAN broadcast republishing currently requires Linux; other platforms support loopback tests only"
            );
            let discovery = UdpSocket::bind((lan_address, discovery_port))
                .await
                .context("bind discovery (one responder owner per LAN address)")?;
            // Linux's unicast-address-bound socket does not receive limited
            // broadcast probes. A second, exact broadcast-address socket keeps
            // the LAN reply source stable without a wildcard bind or SO_REUSE.
            // The native loopback room responder can retain its own address.
            let broadcast = if cfg!(target_os = "linux") {
                Some(
                    UdpSocket::bind((Ipv4Addr::BROADCAST, discovery_port))
                        .await
                        .context(
                            "bind limited-broadcast discovery (one import process per host)",
                        )?,
                )
            } else {
                None
            };
            let mut advertisements = Vec::new();
            let client_policy = ClientPolicy {
                addresses: allowed_clients,
                subnets: allowed_subnets,
            };
            let mut workers = tokio::task::JoinSet::new();
            for room in rooms {
                let socket = Arc::new(UdpSocket::bind(room.lan_bind).await?);
                let ready = Arc::new(AtomicBool::new(false));
                advertisements.push((room.lan_bind.port(), ready.clone()));
                let tls = tls.clone();
                let clients = client_policy.clone();
                workers.spawn(async move {
                    loop {
                        if let Err(e) =
                            import_session(&room, &tls, &clients, socket.clone(), ready.clone())
                                .await
                        {
                            eprintln!("room {} disconnected: {e:#}", room.room_id);
                        }
                        ready.store(false, Ordering::Release);
                        tokio::time::sleep(Duration::from_secs(2)).await;
                    }
                });
            }
            eprintln!(
                "holodeck LAN responder {lan_address}:{discovery_port}; {} imports",
                advertisements.len()
            );
            let mut bytes = [0; 64];
            let mut broadcast_bytes = [0; 64];
            let mut count = 0;
            let mut since = Instant::now();
            loop {
                let (n, source, is_broadcast) = tokio::select! {
                    result = discovery.recv_from(&mut bytes) => {
                        let (n, source) = result?; (n, source, false)
                    }
                    result = async {
                        if let Some(socket) = &broadcast { socket.recv_from(&mut broadcast_bytes).await }
                        else { std::future::pending::<std::io::Result<(usize, SocketAddr)>>().await }
                    } => {
                        let (n, source) = result?; (n, source, true)
                    }
                    result = workers.join_next() => { anyhow::bail!("import worker unexpectedly exited: {result:?}"); }
                };
                let packet = if is_broadcast {
                    &broadcast_bytes[..n]
                } else {
                    &bytes[..n]
                };
                if since.elapsed() >= Duration::from_secs(1) {
                    count = 0;
                    since = Instant::now();
                }
                count += 1;
                if count > 64 || !client_policy.allows(source.ip()) || !decode_probe(packet) {
                    continue;
                }
                for (port, ready) in &advertisements {
                    if ready.load(Ordering::Acquire) {
                        discovery
                            .send_to(&encode_advert(*port).expect("validated port"), source)
                            .await?;
                    }
                }
            }
        }
    }
}

async fn import_session(
    room: &Import,
    tls: &Tls,
    allowed: &ClientPolicy,
    socket: Arc<UdpSocket>,
    ready: Arc<AtomicBool>,
) -> Result<()> {
    let m = material(tls)?;
    let endpoint = QuicEndpoint::client(
        "0.0.0.0:0".parse()?,
        mutual_client_config(m.certificates, m.private_key, m.trust_roots)?,
    )?;
    let peer = tokio::time::timeout(
        Duration::from_secs(8),
        endpoint.connect(room.remote, &room.server_name),
    )
    .await??;
    ensure!(
        certificate_key_binding(&peer.peer_certificate()?) == tls.expected_peer_binding,
        "exporter certificate binding denied"
    );
    let connection = peer.connection();
    let (mut tx, mut rx) = connection.open_bi().await?;
    write_json(
        &mut tx,
        &Request {
            version: 1,
            room_id: room.room_id.clone(),
        },
    )
    .await?;
    let mut clients: BTreeMap<SocketAddr, (u64, Instant)> = BTreeMap::new();
    let mut next_id = 1u64;
    let mut sequence = 0u32;
    let mut reassembler = Reassembler::default();
    let mut bytes = vec![0; holodeck_discovery::MAX_ROOM_PACKET_BYTES + 1];
    let mut lease_deadline = Instant::now() + Duration::from_secs(6);
    let mut generation = String::new();
    let mut control = Box::pin(read_json::<Lease>(&mut rx));
    let mut timer = tokio::time::interval(Duration::from_millis(100));
    let mut count = 0;
    let mut since = Instant::now();
    loop {
        tokio::select! {
            lease = &mut control => {
                let lease = lease?;
                ensure!(lease.version == 1 && lease.room_id == room.room_id && lease.ttl_ms > 0 && lease.ttl_ms <= 6000 && !lease.generation.is_empty(), "invalid room lease");
                if generation != lease.generation { clients.clear(); reassembler = Reassembler::default(); generation = lease.generation; }
                lease_deadline = Instant::now() + Duration::from_millis(lease.ttl_ms.into());
                ready.store(lease.ready, Ordering::Release);
                if !lease.ready { clients.clear(); }
                drop(control);
                control = Box::pin(read_json::<Lease>(&mut rx));
            }
            _ = timer.tick() => {
                if Instant::now() >= lease_deadline {
                    ready.store(false, Ordering::Release);
                    anyhow::bail!("room lease expired; reconnecting instead of waiting for transport idle timeout");
                }
                clients.retain(|_, (_, seen)| seen.elapsed() < Duration::from_secs(60));
            }
            local = socket.recv_from(&mut bytes) => {
                let (n, address) = local?;
                if !ready.load(Ordering::Acquire) || n == 0 || n > holodeck_discovery::MAX_ROOM_PACKET_BYTES || !allowed.allows(address.ip()) { continue; }
                if since.elapsed() >= Duration::from_secs(1) { count = 0; since = Instant::now(); }
                count += 1; if count > 4000 { continue; }
                if !clients.contains_key(&address) {
                    if clients.len() >= MAX_CLIENTS { continue; }
                    clients.insert(address, (next_id, Instant::now()));
                    next_id = next_id.checked_add(1).context("client ID exhausted")?;
                }
                let (id, seen) = clients.get_mut(&address).expect("client exists"); *seen = Instant::now();
                sequence = sequence.wrapping_add(1);
                send_datagram(connection, *id, sequence, &bytes[..n])?;
            }
            incoming = connection.read_datagram() => {
                let packet = incoming?;
                if !ready.load(Ordering::Acquire) { continue; }
                let assembled = match reassembler.accept(&packet, Instant::now()) { Ok(v) => v, Err(e) => { eprintln!("return fragment rejected: {e}"); continue; } };
                if let Some((id, bytes)) = assembled {
                    if let Some((address, _)) = clients.iter().find(|(_, (candidate, _))| *candidate == id) { socket.send_to(&bytes, address).await?; }
                }
            }
        }
    }
}
