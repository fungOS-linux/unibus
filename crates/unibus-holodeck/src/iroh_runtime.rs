//! Iroh cross-site leg for the existing bounded Holodeck LAN bridge.
use crate::{
    RoomConnection,
    room_directory::{ROOM_ALPN, selected_hosts},
    runtime::{self, ClientPolicy, Export, ImportTransport, LocalImport},
};
use anyhow::{Context, Result, ensure};
use iroh::{Endpoint, EndpointAddr, EndpointId, SecretKey, endpoint::presets};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    fs::{self, OpenOptions},
    io::{Read, Write},
    net::{Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
    sync::{Arc, atomic::AtomicBool},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::net::UdpSocket;
use unibus_mycelium::MyceliumServiceDirectory;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IrohRoom {
    pub room_id: String,
    pub lan_bind: SocketAddr,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum Config {
    Export {
        key_file: PathBuf,
        relay_url: Option<String>,
        relay_ca: Option<PathBuf>,
        mycelium_home: PathBuf,
        mycelium_socket: PathBuf,
        allowed_importers: Vec<String>,
        rooms: Vec<Export>,
    },
    Import {
        key_file: PathBuf,
        relay_url: Option<String>,
        relay_ca: Option<PathBuf>,
        mycelium_socket: PathBuf,
        remote_endpoint_id: String,
        lan_address: Ipv4Addr,
        discovery_port: u16,
        allowed_clients: Vec<Ipv4Addr>,
        allowed_subnets: Vec<ipnet::Ipv4Net>,
        rooms: Vec<IrohRoom>,
    },
}

fn key_id(value: &str) -> Result<EndpointId> {
    ensure!(
        value.len() == 64
            && value
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()),
        "invalid lowercase Iroh endpoint ID"
    );
    Ok(value.parse()?)
}

fn validate_relay(relay_url: Option<&str>, relay_ca: Option<&Path>) -> Result<()> {
    ensure!(
        relay_url.is_some() || relay_ca.is_none(),
        "relay CA requires a relay URL"
    );
    if let Some(relay) = relay_url {
        let url = url::Url::parse(relay)?;
        ensure!(
            url.scheme() == "https"
                && url.host_str().is_some()
                && url.username().is_empty()
                && url.password().is_none()
                && url.query().is_none()
                && url.fragment().is_none()
                && url.path() == "/",
            "relay must be credential-free HTTPS origin"
        );
    }
    Ok(())
}

pub fn read_key(path: &Path) -> Result<SecretKey> {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)?;
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file()
            && metadata.len() <= 256
            && metadata.mode() & 0o077 == 0
            && metadata.nlink() == 1,
        "Iroh key must be a private regular file"
    );
    ensure!(
        metadata.uid() == unsafe { libc::geteuid() },
        "Iroh key owner mismatch"
    );
    let mut encoded = String::new();
    file.take(257).read_to_string(&mut encoded)?;
    ensure!(encoded.len() <= 256, "Iroh key file too large");
    Ok(encoded.trim().parse()?)
}

async fn endpoint(
    key_file: &Path,
    relay_url: Option<&str>,
    relay_ca: Option<&Path>,
    accepting: bool,
) -> Result<Endpoint> {
    validate_relay(relay_url, relay_ca)?;
    let mut builder = Endpoint::builder(presets::Minimal).secret_key(read_key(key_file)?);
    builder = match relay_url {
        Some(relay) => builder.relay_mode(iroh::RelayMode::custom([relay.parse()?])),
        None => builder.relay_mode(iroh::RelayMode::Disabled),
    };
    if let Some(path) = relay_ca {
        let bytes = fs::read(path)?;
        let roots = rustls_pemfile::certs(&mut bytes.as_slice())
            .collect::<std::result::Result<Vec<_>, _>>()?;
        ensure!(!roots.is_empty(), "relay CA is empty");
        builder = builder.ca_tls_config(iroh::tls::CaTlsConfig::default().with_extra_roots(roots));
    }
    if accepting {
        builder = builder.alpns(vec![ROOM_ALPN.as_bytes().to_vec()]);
    }
    Ok(builder.bind().await?)
}

#[derive(Clone)]
pub(crate) struct Client {
    endpoint: Endpoint,
    directory: MyceliumServiceDirectory,
    remote: String,
}

impl Client {
    pub(crate) async fn import_session(
        &self,
        room_id: &str,
        allowed: &ClientPolicy,
        socket: Arc<UdpSocket>,
        ready: Arc<AtomicBool>,
    ) -> Result<()> {
        let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
        let hosts = selected_hosts(&self.directory, now, &BTreeSet::from([self.remote.clone()]))?;
        let host = hosts
            .into_iter()
            .find(|host| host.endpoint_id == self.remote)
            .context("pinned remote room endpoint not in fresh Mycelium catalog")?;
        let mut address = EndpointAddr::new(key_id(&host.endpoint_id)?);
        for direct in host.direct_addresses {
            address = address.with_ip_addr(direct);
        }
        for relay in host.relay_urls {
            address = address.with_relay_url(relay.parse()?);
        }
        let connection = tokio::time::timeout(
            Duration::from_secs(10),
            self.endpoint.connect(address, ROOM_ALPN.as_bytes()),
        )
        .await??;
        ensure!(
            connection.remote_id().to_string() == self.remote,
            "remote Iroh identity mismatch"
        );
        let mut watch = tokio::time::interval(Duration::from_secs(30));
        watch.tick().await;
        let session = runtime::import_connection(
            RoomConnection::Iroh(connection.clone()),
            room_id,
            allowed,
            socket,
            ready,
        );
        tokio::pin!(session);
        loop {
            tokio::select! {
                result = &mut session => return result,
                _ = watch.tick() => {
                    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
                    let hosts = selected_hosts(&self.directory, now, &BTreeSet::from([self.remote.clone()]))?;
                    if !hosts.iter().any(|host| host.endpoint_id == self.remote) {
                        connection.close(1u32.into(), b"room discovery lease expired");
                        anyhow::bail!("pinned remote room lease expired or was withdrawn");
                    }
                }
            }
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LeaseFile {
    schema_version: u32,
    endpoints: Vec<PublishedEndpoint>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PublishedEndpoint {
    id: String,
    endpoint_id: String,
    alpns: Vec<String>,
    direct_addresses: Vec<SocketAddr>,
    relay_urls: Vec<String>,
    observed_at: u64,
    expires_at: u64,
}

fn publish(home: &Path, endpoint: &Endpoint) -> Result<()> {
    use std::os::unix::{fs::OpenOptionsExt, io::AsRawFd};
    fs::create_dir_all(home)?;
    let lock_path = home.join("iroh-endpoints.lock");
    let lock = OpenOptions::new()
        .create(true)
        .write(true)
        .mode(0o600)
        .open(lock_path)?;
    ensure!(
        unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } == 0,
        "Iroh lease lock failed"
    );
    let path = home.join("iroh-endpoints.json");
    let mut doc = if path.exists() {
        serde_json::from_slice::<LeaseFile>(&fs::read(&path)?)?
    } else {
        LeaseFile {
            schema_version: 1,
            endpoints: Vec::new(),
        }
    };
    ensure!(doc.schema_version == 1, "unsupported Iroh lease schema");
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let addr = endpoint.addr();
    let direct_addresses = addr.ip_addrs().copied().collect::<Vec<_>>();
    let relay_urls = addr
        .relay_urls()
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    ensure!(
        !direct_addresses.is_empty() || !relay_urls.is_empty(),
        "Iroh endpoint has no advertised route"
    );
    let id = "holodeck-room";
    doc.endpoints
        .retain(|item| item.id != id && item.expires_at > now);
    ensure!(doc.endpoints.len() < 32, "Iroh endpoint catalog full");
    doc.endpoints.push(PublishedEndpoint {
        id: id.into(),
        endpoint_id: endpoint.id().to_string(),
        alpns: vec![ROOM_ALPN.into()],
        direct_addresses,
        relay_urls,
        observed_at: now,
        expires_at: now + 120,
    });
    let temp = home.join(format!(
        ".iroh-endpoints.{}.{}.tmp",
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
    ));
    let mut out = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temp)?;
    out.write_all(&serde_json::to_vec_pretty(&doc)?)?;
    out.sync_all()?;
    fs::rename(&temp, &path)?;
    Ok(())
}

async fn refresh_mycelium(socket: &Path, endpoint_id: &str) -> Result<()> {
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};
    let mut stream = tokio::net::UnixStream::connect(socket)
        .await
        .with_context(|| format!("connect Mycelium scan socket {}", socket.display()))?;
    stream.write_all(b"{\"method\":\"scan\"}\n").await?;
    let mut response = String::new();
    let size = tokio::time::timeout(
        Duration::from_secs(30),
        tokio::io::BufReader::new(stream.take(4 * 1024 * 1024)).read_line(&mut response),
    )
    .await??;
    ensure!(size > 0, "Mycelium scan returned no response");
    let value: serde_json::Value = serde_json::from_str(&response)?;
    ensure!(
        value.get("ok").and_then(serde_json::Value::as_bool) == Some(true),
        "Mycelium scan failed: {}",
        value.get("error").unwrap_or(&serde_json::Value::Null)
    );
    let warnings = value
        .get("result")
        .and_then(|result| result.get("warnings"));
    ensure!(
        !warnings.is_some_and(|warnings| warnings.as_array().is_some_and(|items| items
            .iter()
            .any(|item| item
                .as_str()
                .is_some_and(|text| text.starts_with("Iroh discovery:"))))),
        "Mycelium rejected the Iroh endpoint lease: {warnings:?}"
    );
    let directory = MyceliumServiceDirectory::new(socket.to_path_buf())?;
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    ensure!(
        selected_hosts(&directory, now, &BTreeSet::from([endpoint_id.to_owned()]))?
            .iter()
            .any(|host| host.endpoint_id == endpoint_id),
        "Mycelium scan did not project the published room endpoint"
    );
    Ok(())
}

pub fn validate(config: &Config) -> Result<()> {
    match config {
        Config::Export {
            key_file,
            relay_url,
            relay_ca,
            mycelium_home,
            mycelium_socket,
            allowed_importers,
            rooms,
        } => {
            validate_relay(relay_url.as_deref(), relay_ca.as_deref())?;
            ensure!(
                key_file.is_absolute()
                    && mycelium_home.is_absolute()
                    && mycelium_socket.is_absolute(),
                "Iroh paths must be absolute"
            );
            ensure!(
                !rooms.is_empty()
                    && rooms.len() <= 16
                    && !allowed_importers.is_empty()
                    && allowed_importers.len() <= 16,
                "bounded exports and importer allowlist required"
            );
            for id in allowed_importers {
                key_id(id)?;
            }
            ensure!(
                allowed_importers.iter().collect::<BTreeSet<_>>().len() == allowed_importers.len(),
                "duplicate importer endpoint ID"
            );
            runtime::validate_room_ids(rooms.iter().map(|room| room.room_id.as_str()))?;
            for room in rooms {
                ensure!(
                    !room.label.is_empty()
                        && room.target.port() > 0
                        && room.discovery.port() > 0
                        && runtime::private_ip(room.target.ip())
                        && runtime::private_ip(room.discovery.ip()),
                    "invalid room export"
                );
            }
        }
        Config::Import {
            key_file,
            relay_url,
            relay_ca,
            mycelium_socket,
            remote_endpoint_id,
            lan_address,
            discovery_port,
            allowed_clients,
            allowed_subnets,
            rooms,
        } => {
            validate_relay(relay_url.as_deref(), relay_ca.as_deref())?;
            ensure!(
                key_file.is_absolute() && mycelium_socket.is_absolute(),
                "Iroh paths must be absolute"
            );
            key_id(remote_endpoint_id)?;
            ensure!(
                *discovery_port > 0 && runtime::private_ip((*lan_address).into()),
                "invalid LAN discovery bind"
            );
            ensure!(
                (!allowed_clients.is_empty() || !allowed_subnets.is_empty())
                    && allowed_clients.len() <= 64
                    && allowed_subnets.len() <= 8,
                "bounded LAN client allowlist required"
            );
            ensure!(
                allowed_clients
                    .iter()
                    .all(|ip| runtime::private_ip((*ip).into()))
                    && allowed_subnets.iter().all(|net| net.contains(lan_address)
                        && runtime::private_ip(net.network().into())
                        && runtime::private_ip(net.broadcast().into())),
                "invalid LAN client allowlist"
            );
            ensure!(
                !rooms.is_empty() && rooms.len() <= 16,
                "expected 1..16 imported rooms"
            );
            let mut ports = BTreeSet::new();
            runtime::validate_room_ids(rooms.iter().map(|room| room.room_id.as_str()))?;
            for room in rooms {
                ensure!(
                    room.lan_bind.ip() == std::net::IpAddr::V4(*lan_address)
                        && room.lan_bind.port() > 0
                        && room.lan_bind.port() != *discovery_port
                        && ports.insert(room.lan_bind.port()),
                    "invalid imported room bind"
                );
            }
        }
    }
    Ok(())
}

pub async fn run(config: Config) -> Result<()> {
    validate(&config)?;
    match config {
        Config::Export {
            key_file,
            relay_url,
            relay_ca,
            mycelium_home,
            mycelium_socket,
            allowed_importers,
            rooms,
        } => {
            let endpoint =
                endpoint(&key_file, relay_url.as_deref(), relay_ca.as_deref(), true).await?;
            let allowed = allowed_importers
                .iter()
                .map(|id| key_id(id))
                .collect::<Result<BTreeSet<_>>>()?;
            let permits = Arc::new(tokio::sync::Semaphore::new(16));
            let mut timer = tokio::time::interval(Duration::from_secs(30));
            let mut scans = tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    _ = timer.tick() => {
                        if scans.is_empty() {
                            let endpoint = endpoint.clone();
                            let rooms = rooms.clone();
                            let home = mycelium_home.clone();
                            let socket = mycelium_socket.clone();
                            scans.spawn(async move {
                                let mut ready = false;
                                for room in &rooms { ready |= runtime::host_ready(room).await; }
                                if ready {
                                    for _ in 0..20 {
                                        let address = endpoint.addr();
                                        if address.ip_addrs().next().is_some() || address.relay_urls().next().is_some() { break; }
                                        tokio::time::sleep(Duration::from_millis(500)).await;
                                    }
                                    publish(&home, &endpoint)?;
                                    refresh_mycelium(&socket, &endpoint.id().to_string()).await?;
                                }
                                Ok::<_, anyhow::Error>(())
                            });
                        }
                    },
                    completed = scans.join_next(), if !scans.is_empty() => {
                        completed.context("Iroh lease scan task vanished")???;
                    },
                    incoming = endpoint.accept() => {
                        let incoming = incoming.context("Iroh endpoint closed")?;
                        let Ok(permit) = permits.clone().try_acquire_owned() else { incoming.refuse(); continue; };
                        let allowed = allowed.clone();
                        let rooms = rooms.clone();
                        tokio::spawn(async move {
                            let _permit = permit;
                            let result: Result<()> = async {
                                let connection = tokio::time::timeout(Duration::from_secs(10), incoming).await??;
                                ensure!(connection.alpn() == ROOM_ALPN.as_bytes(), "wrong room ALPN");
                                if !allowed.contains(&connection.remote_id()) { connection.close(1u32.into(), b"importer denied"); anyhow::bail!("importer endpoint not allowed"); }
                                runtime::serve_connection(RoomConnection::Iroh(connection), rooms).await
                            }.await;
                            if let Err(error) = result { eprintln!("Iroh room session ended: {error:#}"); }
                        });
                    }
                }
            }
        }
        Config::Import {
            key_file,
            relay_url,
            relay_ca,
            mycelium_socket,
            remote_endpoint_id,
            lan_address,
            discovery_port,
            allowed_clients,
            allowed_subnets,
            rooms,
        } => {
            let endpoint =
                endpoint(&key_file, relay_url.as_deref(), relay_ca.as_deref(), false).await?;
            let directory = MyceliumServiceDirectory::new(mycelium_socket)?;
            let client = Client {
                endpoint,
                directory,
                remote: remote_endpoint_id,
            };
            let rooms = rooms
                .into_iter()
                .map(|r| LocalImport {
                    room_id: r.room_id,
                    lan_bind: r.lan_bind,
                    quic: None,
                })
                .collect();
            runtime::run_import_lan(
                lan_address,
                discovery_port,
                allowed_clients,
                allowed_subnets,
                rooms,
                ImportTransport::Iroh(client),
            )
            .await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use holodeck_discovery::{PROBE, encode_advert};
    use std::{
        io::{BufRead, BufReader},
        os::unix::net::UnixListener,
        sync::atomic::Ordering,
    };

    #[test]
    fn import_rejects_broad_lan_and_non_https_relay() {
        let key = SecretKey::from_bytes(&[21; 32]).public().to_string();
        let mut config = Config::Import {
            key_file: "/tmp/room.key".into(),
            relay_url: Some("https://relay.example.net/".into()),
            relay_ca: None,
            mycelium_socket: "/tmp/mycelium.sock".into(),
            remote_endpoint_id: key,
            lan_address: Ipv4Addr::new(192, 168, 1, 10),
            discovery_port: 6781,
            allowed_clients: Vec::new(),
            allowed_subnets: vec!["0.0.0.0/0".parse().unwrap()],
            rooms: vec![IrohRoom {
                room_id: "friend/room".into(),
                lan_bind: "192.168.1.10:6790".parse().unwrap(),
            }],
        };
        assert!(validate(&config).is_err());
        if let Config::Import {
            allowed_subnets,
            relay_url,
            ..
        } = &mut config
        {
            *allowed_subnets = vec!["192.168.1.0/24".parse().unwrap()];
            *relay_url = Some("http://relay.example.net/".into());
        }
        assert!(validate(&config).is_err());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn exporter_refreshes_mycelium_after_publication() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("mycelium.sock");
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        let endpoint_id = SecretKey::from_bytes(&[19; 32]).public().to_string();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let catalog = serde_json::json!({"ok": true, "result": {"observations": [{
            "service_id": "service/home/iroh/holodeck-room",
            "observed_at": now, "expires_at": now + 120,
            "provenance": {"provider": "mycelium-iroh-owner-lease", "observer": "home"},
            "value": {"kind": "iroh", "endpoints": [format!("iroh://{endpoint_id}")], "attributes": {
                "node_id": "home", "endpoint_id": endpoint_id,
                "authentication": "application-owned", "alpns": serde_json::to_string(&vec![ROOM_ALPN]).unwrap(),
                "direct_addresses": "[\"127.0.0.1:4000\"]", "relay_urls": "[]"
            }}
        }]}});
        let server = tokio::spawn(async move {
            for (expected, response) in [
                (
                    "{\"method\":\"scan\"}\n",
                    "{\"ok\":true,\"result\":{\"warnings\":[]}}".to_owned(),
                ),
                ("{\"method\":\"services\"}\n", catalog.to_string()),
            ] {
                let (stream, _) = listener.accept().await.unwrap();
                let mut reader = tokio::io::BufReader::new(stream);
                let mut request = String::new();
                reader.read_line(&mut request).await.unwrap();
                assert_eq!(request, expected);
                reader
                    .get_mut()
                    .write_all(format!("{response}\n").as_bytes())
                    .await
                    .unwrap();
            }
        });
        refresh_mycelium(
            &path,
            &SecretKey::from_bytes(&[19; 32]).public().to_string(),
        )
        .await
        .unwrap();
        server.await.unwrap();
    }

    #[test]
    fn secret_key_requires_private_regular_owner_file() {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("key");
        let key = SecretKey::from_bytes(&[17; 32]);
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&path)
            .unwrap();
        for byte in key.to_bytes() {
            write!(file, "{byte:02x}").unwrap();
        }
        drop(file);
        assert_eq!(read_key(&path).unwrap().public(), key.public());
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(read_key(&path).is_err());
        let link = temp.path().join("link");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        assert!(read_key(&link).is_err());
    }

    #[tokio::test]
    async fn publication_preserves_other_application_lease() {
        let endpoint = Endpoint::builder(presets::Minimal)
            .secret_key(SecretKey::from_bytes(&[18; 32]))
            .relay_mode(iroh::RelayMode::Disabled)
            .alpns(vec![ROOM_ALPN.as_bytes().to_vec()])
            .bind_addr("127.0.0.1:0")
            .unwrap()
            .bind()
            .await
            .unwrap();
        let temp = tempfile::tempdir().unwrap();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let other = PublishedEndpoint {
            id: "other-app".into(),
            endpoint_id: "a".repeat(64),
            alpns: vec!["other/1".into()],
            direct_addresses: vec!["127.0.0.1:1000".parse().unwrap()],
            relay_urls: Vec::new(),
            observed_at: now,
            expires_at: now + 120,
        };
        fs::write(
            temp.path().join("iroh-endpoints.json"),
            serde_json::to_vec(&LeaseFile {
                schema_version: 1,
                endpoints: vec![other],
            })
            .unwrap(),
        )
        .unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if publish(temp.path(), &endpoint).is_ok() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        let doc: LeaseFile =
            serde_json::from_slice(&fs::read(temp.path().join("iroh-endpoints.json")).unwrap())
                .unwrap();
        assert_eq!(doc.endpoints.len(), 2);
        let room = doc
            .endpoints
            .iter()
            .find(|item| item.id == "holodeck-room")
            .unwrap();
        assert_eq!(room.endpoint_id, endpoint.id().to_string());
        assert_eq!(room.alpns, vec![ROOM_ALPN]);
        assert!(room.expires_at - room.observed_at <= 120);
    }

    #[tokio::test]
    async fn iroh_room_leg_forwards_enet_datagrams_to_selected_export() {
        let server_key = SecretKey::from_bytes(&[31; 32]);
        let client_key = SecretKey::from_bytes(&[32; 32]);
        let server = Endpoint::builder(presets::Minimal)
            .secret_key(server_key)
            .relay_mode(iroh::RelayMode::Disabled)
            .alpns(vec![ROOM_ALPN.as_bytes().to_vec()])
            .bind_addr("127.0.0.1:0")
            .unwrap()
            .bind()
            .await
            .unwrap();
        let client = Endpoint::builder(presets::Minimal)
            .secret_key(client_key)
            .relay_mode(iroh::RelayMode::Disabled)
            .bind_addr("127.0.0.1:0")
            .unwrap()
            .bind()
            .await
            .unwrap();
        let echo = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let target = echo.local_addr().unwrap();
        let echo_task = tokio::spawn(async move {
            let mut bytes = vec![0; 9000];
            loop {
                let (n, from) = echo.recv_from(&mut bytes).await.unwrap();
                echo.send_to(&bytes[..n], from).await.unwrap();
            }
        });
        let responder = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let discovery = responder.local_addr().unwrap();
        let responder_task = tokio::spawn(async move {
            let mut bytes = [0; 64];
            loop {
                let (n, from) = responder.recv_from(&mut bytes).await.unwrap();
                if bytes[..n] == PROBE {
                    responder
                        .send_to(&encode_advert(target.port()).unwrap(), from)
                        .await
                        .unwrap();
                }
            }
        });
        let export = Export {
            room_id: "friend/room".into(),
            label: "friend".into(),
            target,
            discovery,
        };
        let expected_client = client.id();
        let server_id = server.id();
        let server_socket = server
            .bound_sockets()
            .into_iter()
            .find(|addr| addr.is_ipv4())
            .unwrap();
        let server_task = tokio::spawn(async move {
            let incoming = server.accept().await.unwrap();
            let connection = incoming.await.unwrap();
            assert_eq!(connection.remote_id(), expected_client);
            runtime::serve_connection(RoomConnection::Iroh(connection), vec![export]).await
        });
        let local = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let imported = local.local_addr().unwrap();
        let ready = Arc::new(AtomicBool::new(false));
        let policy = ClientPolicy {
            addresses: vec![Ipv4Addr::LOCALHOST],
            subnets: Vec::new(),
        };
        let temp = tempfile::tempdir().unwrap();
        let socket_path = temp.path().join("mycelium.sock");
        let listener = UnixListener::bind(&socket_path).unwrap();
        let service = serde_json::json!({
            "service_id": "service/friend/iroh/holodeck-room",
            "observed_at": SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs(),
            "expires_at": SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() + 120,
            "provenance": {"provider": "mycelium-iroh-owner-lease", "observer": "friend"},
            "value": {
                "kind": "iroh", "endpoints": [format!("iroh://{server_id}")],
                "attributes": {
                    "node_id": "friend", "endpoint_id": server_id.to_string(),
                    "authentication": "application-owned", "alpns": serde_json::to_string(&vec![ROOM_ALPN]).unwrap(),
                    "direct_addresses": serde_json::to_string(&vec![server_socket]).unwrap(), "relay_urls": "[]"
                }
            }
        });
        let catalog = serde_json::json!({"ok": true, "result": {"observations": [service]}});
        let catalog_thread = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = String::new();
            BufReader::new(stream.try_clone().unwrap())
                .read_line(&mut request)
                .unwrap();
            assert_eq!(request, "{\"method\":\"services\"}\n");
            writeln!(stream, "{catalog}").unwrap();
        });
        let connector = Client {
            endpoint: client,
            directory: MyceliumServiceDirectory::new(socket_path).unwrap(),
            remote: server_id.to_string(),
        };
        let import_task = tokio::spawn({
            let ready = ready.clone();
            async move {
                connector
                    .import_session("friend/room", &policy, local, ready)
                    .await
            }
        });
        tokio::time::timeout(Duration::from_secs(5), async {
            while !ready.load(Ordering::Acquire) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let quest = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let packet = vec![7; 8192];
        quest.send_to(&packet, imported).await.unwrap();
        let mut returned = vec![0; 9000];
        let (n, _) = tokio::time::timeout(Duration::from_secs(3), quest.recv_from(&mut returned))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(&returned[..n], packet);
        import_task.abort();
        catalog_thread.join().unwrap();
        server_task.abort();
        echo_task.abort();
        responder_task.abort();
    }
}
