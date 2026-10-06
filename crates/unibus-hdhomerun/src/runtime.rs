use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, ensure};
use axum::{
    Router,
    body::Body,
    extract::{ConnectInfo, State},
    http::{HeaderMap, Method, Uri, Version},
    response::Response,
    routing::any,
};
use ipnet::Ipv4Net;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;
use tokio::{
    net::{TcpListener, UdpSocket},
    sync::{Mutex, OwnedSemaphorePermit, RwLock, Semaphore},
    time::timeout,
};
use unibus_quic::{
    PemPeerCredentials, QuicEndpoint, QuicPeer, certificate_key_binding, mutual_client_config_from,
    mutual_server_config_from,
};

use crate::{
    CONTRACT, Catalog, LEGACY_CONTRACT, MAX_HEADER, MAX_METADATA, Operation, Reply, Request,
    discovery, is_protected, lineup_xml, playlist,
};

const DEADLINE: Duration = Duration::from_millis(crate::IO_DEADLINE_MS);
const STALE: Duration = Duration::from_secs(75);

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Identity {
    pub ca_file: PathBuf,
    pub cert_file: PathBuf,
    pub key_file: PathBuf,
    /// SHA-256 certificate bindings, as produced by unibus-quic.
    pub allowed_peer_bindings: Vec<String>,
}

impl Identity {
    fn credentials(&self) -> Result<PemPeerCredentials> {
        ensure!(
            !self.allowed_peer_bindings.is_empty() && self.allowed_peer_bindings.len() <= 16,
            "explicit peer allowlist required"
        );
        for binding in &self.allowed_peer_bindings {
            ensure!(
                binding.starts_with("sha256:")
                    && binding.len() == 71
                    && binding[7..].bytes().all(|b| b.is_ascii_hexdigit()),
                "invalid certificate binding"
            );
        }
        Ok(PemPeerCredentials::new(
            &self.ca_file,
            &self.cert_file,
            &self.key_file,
        ))
    }
    fn authorize(&self, peer: &QuicPeer) -> Result<()> {
        let binding = certificate_key_binding(&peer.peer_certificate()?);
        ensure!(
            self.allowed_peer_bindings.contains(&binding),
            "peer certificate is not authorized for this tuner"
        );
        Ok(())
    }
}

#[derive(Clone, Deserialize)]
#[serde(tag = "role", rename_all = "snake_case", deny_unknown_fields)]
pub enum Config {
    Owner {
        listen: SocketAddr,
        tuner_ip: Ipv4Addr,
        #[serde(default = "metadata_port")]
        metadata_port: u16,
        #[serde(default = "media_port")]
        media_port: u16,
        identity: Identity,
    },
    Proxy {
        owner: SocketAddr,
        owner_server_name: String,
        lan_ip: Ipv4Addr,
        allowed_lan: Ipv4Net,
        http_port: u16,
        stream_port: u16,
        #[serde(default)]
        share_device_auth: bool,
        #[serde(default)]
        web_origins: Vec<String>,
        identity: Identity,
    },
}

fn metadata_port() -> u16 {
    80
}
fn media_port() -> u16 {
    5004
}

fn private(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            ip.is_private() || ip.is_loopback() || (u32::from(ip) & 0xffc00000 == 0x64400000)
        }
        IpAddr::V6(ip) => ip.is_loopback() || ip.is_unique_local(),
    }
}

impl Config {
    pub fn validate(&self) -> Result<()> {
        match self {
            Self::Owner {
                listen,
                tuner_ip,
                identity,
                metadata_port,
                media_port,
            } => {
                ensure!(
                    private(listen.ip()) && listen.port() != 0,
                    "owner must bind an explicit private address"
                );
                ensure!(
                    tuner_ip.is_private() || tuner_ip.is_loopback(),
                    "tuner must be local/private"
                );
                identity.credentials()?;
                ensure!(
                    *metadata_port != 0 && *media_port != 0,
                    "tuner ports required"
                );
            }
            Self::Proxy {
                owner,
                owner_server_name,
                lan_ip,
                allowed_lan,
                http_port,
                stream_port,
                identity,
                web_origins,
                ..
            } => {
                ensure!(
                    private(owner.ip()) && owner.port() != 0,
                    "owner must have a private endpoint"
                );
                ensure!(
                    !owner_server_name.is_empty() && owner_server_name.len() <= 253,
                    "owner TLS name required"
                );
                ensure!(
                    lan_ip.is_private() || lan_ip.is_loopback(),
                    "LAN must be private"
                );
                ensure!(
                    allowed_lan.contains(lan_ip) && allowed_lan.prefix_len() >= 16,
                    "explicit, bounded LAN allowlist required"
                );
                ensure!(*http_port != 0 && *stream_port != 0, "HTTP ports required");
                ensure!(web_origins.len() <= 16, "too many web origins");
                for origin in web_origins {
                    ensure!(
                        valid_origin(origin),
                        "web origins must be exact HTTP(S) origins or null; wildcards are not allowed"
                    );
                }
                identity.credentials()?;
            }
        }
        Ok(())
    }
}

fn valid_origin(origin: &str) -> bool {
    if origin == "null" {
        return true;
    }
    if origin.len() > 256 || origin.contains('*') {
        return false;
    }
    reqwest::Url::parse(origin).is_ok_and(|url| {
        matches!(url.scheme(), "http" | "https") && url.origin().ascii_serialization() == origin
    })
}

fn cors_origin<'a>(headers: &'a HeaderMap, allowed: &[String]) -> Option<&'a str> {
    let origin = headers.get("Origin")?.to_str().ok()?;
    allowed
        .iter()
        .any(|allowed| allowed == origin)
        .then_some(origin)
}

fn apply_cors(response: &mut Response, headers: &HeaderMap, allowed: &[String]) {
    response
        .headers_mut()
        .insert("Vary", "Origin".parse().expect("static header"));
    if let Some(origin) = cors_origin(headers, allowed) {
        let target = response.headers_mut();
        target.insert(
            "Access-Control-Allow-Origin",
            origin.parse().expect("validated request header"),
        );
        target.insert(
            "Access-Control-Allow-Methods",
            "GET, HEAD, OPTIONS".parse().expect("static header"),
        );
        target.insert(
            "Access-Control-Allow-Headers",
            "Accept, Content-Type, Range, Cache-Control"
                .parse()
                .expect("static header"),
        );
        target.insert(
            "Access-Control-Expose-Headers",
            "X-HDHomeRun-Error, Content-Type"
                .parse()
                .expect("static header"),
        );
        if headers
            .get("Access-Control-Request-Private-Network")
            .is_some_and(|v| v == "true")
        {
            target.insert(
                "Access-Control-Allow-Private-Network",
                "true".parse().expect("static header"),
            );
        }
    }
}

pub async fn run(config: Config) -> Result<()> {
    config.validate()?;
    match config {
        Config::Owner {
            listen,
            tuner_ip,
            metadata_port,
            media_port,
            identity,
        } => {
            run_owner(
                listen,
                SocketAddr::from((tuner_ip, metadata_port)),
                SocketAddr::from((tuner_ip, media_port)),
                identity,
            )
            .await
        }
        proxy @ Config::Proxy { .. } => run_proxy(proxy).await,
    }
}

async fn write_json<T: Serialize>(send: &mut quinn::SendStream, value: &T) -> Result<()> {
    let bytes = serde_json::to_vec(value)?;
    ensure!(bytes.len() <= MAX_HEADER, "wire header too large");
    send.write_all(&(bytes.len() as u32).to_be_bytes()).await?;
    send.write_all(&bytes).await?;
    Ok(())
}

async fn read_json<T: DeserializeOwned>(recv: &mut quinn::RecvStream) -> Result<T> {
    let mut length = [0; 4];
    recv.read_exact(&mut length).await?;
    let length = u32::from_be_bytes(length) as usize;
    ensure!(length <= MAX_HEADER, "wire header too large");
    let mut bytes = vec![0; length];
    recv.read_exact(&mut bytes).await?;
    Ok(serde_json::from_slice(&bytes)?)
}

fn http_client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(3))
        .read_timeout(DEADLINE)
        .build()?)
}

async fn limited_body(mut response: reqwest::Response, maximum: usize) -> Result<Vec<u8>> {
    ensure!(
        response
            .content_length()
            .is_none_or(|length| length <= maximum as u64),
        "metadata too large"
    );
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        ensure!(body.len() + chunk.len() <= maximum, "metadata too large");
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

async fn fetch_catalog(client: &reqwest::Client, ip: SocketAddr) -> Result<Catalog> {
    timeout(DEADLINE, async {
        let device = client
            .get(format!("http://{ip}/discover.json"))
            .send()
            .await?
            .error_for_status()?;
        let channels = client
            .get(format!("http://{ip}/lineup.json"))
            .send()
            .await?
            .error_for_status()?;
        let catalog = Catalog {
            device: serde_json::from_slice(&limited_body(device, MAX_METADATA).await?)?,
            channels: serde_json::from_slice(&limited_body(channels, MAX_METADATA).await?)?,
        };
        catalog.identity()?;
        Ok(catalog)
    })
    .await
    .context("tuner metadata timed out")?
}

async fn send_reply(
    send: &mut quinn::SendStream,
    status: u16,
    content_type: &str,
    error: Option<String>,
    body: &[u8],
) -> Result<()> {
    write_json(
        send,
        &Reply {
            status,
            content_type: content_type.into(),
            tuner_error: error,
            resource: None,
        },
    )
    .await?;
    send.write_all(body).await?;
    send.finish()?;
    Ok(())
}

struct Owner {
    metadata: SocketAddr,
    media: SocketAddr,
    client: reqwest::Client,
    tuners: u8,
    streams: Arc<Semaphore>,
}

async fn owner_request(
    owner: Arc<Owner>,
    mut send: quinn::SendStream,
    mut recv: quinn::RecvStream,
) -> Result<()> {
    let request: Request = timeout(DEADLINE, read_json(&mut recv)).await??;
    ensure!(
        request.contract == CONTRACT || request.contract == LEGACY_CONTRACT,
        "unknown request contract"
    );
    let legacy = request.contract == LEGACY_CONTRACT;
    // A request is one header plus FIN, never a client-supplied body.
    ensure!(
        timeout(DEADLINE, recv.read_to_end(0)).await??.is_empty(),
        "request contains trailing data"
    );
    match request.operation {
        Operation::Catalog => {
            let catalog = fetch_catalog(&owner.client, owner.metadata).await?;
            let body = serde_json::to_vec(&catalog)?;
            ensure!(body.len() <= MAX_METADATA, "catalog too large");
            send_reply(&mut send, 200, "application/json", None, &body).await
        }
        Operation::LineupStatus => {
            let response = timeout(
                DEADLINE,
                owner
                    .client
                    .get(format!("http://{}/lineup_status.json", owner.metadata))
                    .send(),
            )
            .await??;
            let status = response.status().as_u16();
            let body = timeout(DEADLINE, limited_body(response, MAX_HEADER)).await??;
            send_reply(&mut send, status, "application/json", None, &body).await
        }
        operation @ Operation::Stream { .. } => {
            let path = operation.stream_path(owner.tuners)?;
            let permit = match owner.streams.clone().try_acquire_owned() {
                Ok(permit) => permit,
                Err(_) => {
                    return send_reply(
                        &mut send,
                        503,
                        "text/plain",
                        Some("805 All Tuners In Use".into()),
                        b"All tuners in use\n",
                    )
                    .await;
                }
            };
            let catalog = fetch_catalog(&owner.client, owner.metadata).await?;
            let Operation::Stream { channel, .. } = &operation else {
                unreachable!()
            };
            let found = catalog
                .channels
                .iter()
                .find(|c| c["GuideNumber"].as_str() == Some(channel));
            if found.is_none() {
                return send_reply(
                    &mut send,
                    404,
                    "text/plain",
                    Some("801 Unknown Channel".into()),
                    b"Unknown channel\n",
                )
                .await;
            }
            if is_protected(found.expect("checked channel")) {
                return send_reply(
                    &mut send,
                    503,
                    "text/plain",
                    Some("811 Content Protection Required".into()),
                    b"Protected channels are not relayed\n",
                )
                .await;
            }
            // Cancel even while waiting for the tuner: disconnect must release it.
            let stopped = send.stopped();
            tokio::select! {
                _ = stopped => Ok(()),
                result = async {
                    let mut response = timeout(DEADLINE, owner.client.get(format!("http://{}{path}", owner.media)).send()).await??;
                    let status = response.status().as_u16();
                    let error = response.headers().get("X-HDHomeRun-Error").and_then(|v| v.to_str().ok()).map(str::to_owned);
                    if status != 200 {
                        let body = limited_body(response, MAX_HEADER).await?;
                        return send_reply(&mut send, status, "text/plain", error, &body).await;
                    }
                    let resource = response.headers().get("X-HDHomeRun-Resource").and_then(|v| v.to_str().ok())
                        .filter(|value| value.strip_prefix("tuner").and_then(|v| v.parse::<u8>().ok()).is_some_and(|n| n < owner.tuners))
                        .map(str::to_owned);
                    let resource = if legacy { None } else { resource };
                    write_json(&mut send, &Reply { status, content_type:"video/mpeg".into(), tuner_error: error, resource }).await?;
                    while let Some(chunk) = response.chunk().await? {
                        // One upstream chunk at a time; QUIC's send window provides backpressure.
                        timeout(DEADLINE, send.write_all(&chunk)).await??;
                    }
                    send.finish()?;
                    Ok(())
                } => result,
            }
            // The lease spans the complete upstream response, including cancellation.
            .inspect(|_| drop(permit))
        }
    }
}

pub async fn run_owner(
    listen: SocketAddr,
    metadata: SocketAddr,
    media: SocketAddr,
    identity: Identity,
) -> Result<()> {
    let client = http_client()?;
    let catalog = fetch_catalog(&client, metadata)
        .await
        .context("probe physical tuner before serving")?;
    let (_, tuners) = catalog.identity()?;
    let owner = Arc::new(Owner {
        metadata,
        media,
        client,
        tuners,
        streams: Arc::new(Semaphore::new(tuners.into())),
    });
    let mut tls = mutual_server_config_from(&identity.credentials()?)?;
    let mut transport = quinn::TransportConfig::default();
    transport
        .max_concurrent_bidi_streams(16u32.into())
        .max_concurrent_uni_streams(0u32.into())
        .send_window(512 * 1024)
        .receive_window((256 * 1024u32).into())
        .stream_receive_window((64 * 1024u32).into())
        .max_idle_timeout(Some(Duration::from_secs(30).try_into()?));
    tls.transport_config(Arc::new(transport));
    let endpoint = Arc::new(QuicEndpoint::server(listen, tls)?);
    let connections = Arc::new(Semaphore::new(8));
    let requests = Arc::new(Semaphore::new(32));
    eprintln!("HDHomeRun owner listening at {listen}; physical tuners={tuners}");
    loop {
        let permit = connections.clone().acquire_owned().await?;
        let endpoint = endpoint.clone();
        let identity = identity.clone();
        let owner = owner.clone();
        let requests = requests.clone();
        let peer = match timeout(DEADLINE, endpoint.accept()).await {
            Ok(Ok(peer)) => peer,
            Ok(Err(error)) => {
                eprintln!("HDHomeRun handshake failed: {error:#}");
                continue;
            }
            Err(_) => continue,
        };
        tokio::spawn(async move {
            let _permit = permit;
            if let Err(error) = identity.authorize(&peer) {
                peer.connection()
                    .close(1u32.into(), b"unauthorized tuner peer");
                eprintln!("{error:#}");
                return;
            }
            while let Ok((mut send, recv)) = peer.connection().accept_bi().await {
                let Ok(permit) = requests.clone().try_acquire_owned() else {
                    let _ = send.reset(2u32.into());
                    continue;
                };
                let owner = owner.clone();
                tokio::spawn(async move {
                    let _permit = permit;
                    if let Err(error) = owner_request(owner, send, recv).await {
                        eprintln!("HDHomeRun request failed: {error:#}");
                    }
                });
            }
        });
    }
}

struct Snapshot {
    catalog: Catalog,
    observed: Instant,
    observed_at_unix_ms: u64,
}

fn unix_ms() -> Result<u64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)?
        .as_millis()
        .try_into()?)
}

pub struct Proxy {
    endpoint: QuicEndpoint,
    peer: Mutex<Option<QuicPeer>>,
    owner: SocketAddr,
    server_name: String,
    identity: Identity,
    lan: Ipv4Net,
    base: String,
    stream_base: String,
    share_auth: bool,
    web_origins: Vec<String>,
    snapshot: RwLock<Option<Snapshot>>,
    requests: Arc<Semaphore>,
}

impl Proxy {
    async fn peer(&self) -> Result<QuicPeer> {
        let mut peer = self.peer.lock().await;
        if peer
            .as_ref()
            .is_some_and(|peer| peer.connection().close_reason().is_none())
        {
            return Ok(peer.as_ref().expect("checked peer").clone());
        }
        let connected = timeout(
            DEADLINE,
            self.endpoint.connect(self.owner, &self.server_name),
        )
        .await??;
        self.identity.authorize(&connected)?;
        *peer = Some(connected.clone());
        Ok(connected)
    }

    async fn request(&self, operation: Operation) -> Result<(Reply, quinn::RecvStream)> {
        timeout(DEADLINE, async {
            let peer = self.peer().await?;
            let (mut send, mut recv) = peer.connection().open_bi().await?;
            write_json(
                &mut send,
                &Request {
                    contract: CONTRACT.into(),
                    operation,
                },
            )
            .await?;
            send.finish()?;
            let reply: Reply = read_json(&mut recv).await?;
            ensure!(
                (100..=599).contains(&reply.status),
                "invalid response status"
            );
            Ok((reply, recv))
        })
        .await
        .context("private tuner request timed out")?
    }

    async fn refresh(&self) -> Result<()> {
        let (reply, mut recv) = self.request(Operation::Catalog).await?;
        ensure!(reply.status == 200, "owner catalog unavailable");
        let body = timeout(DEADLINE, recv.read_to_end(MAX_METADATA)).await??;
        let catalog: Catalog = serde_json::from_slice(&body)?;
        let catalog = catalog.project(&self.base, &self.stream_base, self.share_auth)?;
        *self.snapshot.write().await = Some(Snapshot {
            catalog,
            observed: Instant::now(),
            observed_at_unix_ms: unix_ms()?,
        });
        Ok(())
    }

    async fn catalog(&self) -> Result<Catalog> {
        let snapshot = self.snapshot.read().await;
        let snapshot = snapshot.as_ref().context("no tuner catalog")?;
        ensure!(
            snapshot.observed.elapsed() < STALE,
            "tuner catalog is stale"
        );
        Ok(snapshot.catalog.clone())
    }
}

fn response(status: u16, content_type: &str, body: impl Into<Body>) -> Result<Response> {
    Ok(Response::builder()
        .status(status)
        .header("Content-Type", content_type)
        .header("Cache-Control", "no-store")
        .body(body.into())?)
}

fn service_response(status: u16, document: &impl Serialize) -> Result<Response> {
    let body = serde_json::to_vec(document)?;
    ensure!(
        body.len() <= MAX_METADATA,
        "service projection exceeds metadata budget"
    );
    response(status, "application/json", body)
}

fn native_media_response(response: &mut Response) {
    // Hyper chunks unknown-length HTTP/1.1 bodies even with Connection: close.
    // HTTP/1.0 explicitly selects close-delimited framing without a fake length
    // or manual socket writes. SDK clients receive raw MPEG-TS immediately
    // after the headers, matching the device's body format.
    *response.version_mut() = Version::HTTP_10;
    let headers = response.headers_mut();
    headers.insert("Connection", "close".parse().expect("static header"));
    headers.insert("Content-Type", "video/mpeg".parse().expect("static header"));
    headers.insert(
        "transferMode.dlna.org",
        "Streaming".parse().expect("static header"),
    );
    headers.remove("Transfer-Encoding");
}

// Explicit STOP_SENDING when the HTTP body is dropped releases upstream tuning.
struct MediaBody {
    recv: quinn::RecvStream,
    _permit: OwnedSemaphorePermit,
}
impl Drop for MediaBody {
    fn drop(&mut self) {
        let _ = self.recv.stop(0u32.into());
    }
}

async fn handle(
    State(proxy): State<Arc<Proxy>>,
    ConnectInfo(client): ConnectInfo<SocketAddr>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
) -> Response {
    let result = async {
        ensure!(matches!(client.ip(), IpAddr::V4(ip) if proxy.lan.contains(&ip)), "client outside permitted LAN");
        if method == Method::OPTIONS {
            if cors_origin(&headers, &proxy.web_origins).is_none() ||
                !headers.get("Access-Control-Request-Method").is_some_and(|method| method == "GET" || method == "HEAD") {
                return response(403, "text/plain", "Origin or method is not permitted\n");
            }
            return response(204, "text/plain", Body::empty());
        }
        if method != Method::GET && method != Method::HEAD { return response(405, "text/plain", "Read-only tuner proxy\n"); }
        let permit = match proxy.requests.clone().try_acquire_owned() {
            Ok(permit) => permit,
            Err(_) => return response(503, "text/plain", "Proxy request limit reached\n"),
        };
        let path = uri.path();
        if matches!(path, "/v1/catalog" | "/v1/health" | "/v1/streams") {
            use stream_descriptors::service::{HealthState, SERVICE_HEALTH_KIND, ServiceHealth};
            ensure!(uri.query().is_none(), "unsupported service query");
            let now = unix_ms()?;
            let snapshot = proxy.snapshot.read().await;
            let Some(snapshot) = snapshot.as_ref() else {
                return response(503, "text/plain", "No authenticated tuner observation\n");
            };
            let (device, _) = snapshot.catalog.identity()?;
            let service_id = format!("hdhomerun.{device:08X}");
            let observed = snapshot.observed_at_unix_ms;
            let until = observed.checked_add(STALE.as_millis().try_into()?).context("lease overflow")?;
            let stale = snapshot.observed.elapsed() >= STALE || now < observed || now >= until;
            if path == "/v1/health" {
                let health = ServiceHealth {
                    contract: SERVICE_HEALTH_KIND.into(), service_id,
                    state: if stale { HealthState::Stale } else { HealthState::Healthy },
                    reason: stale.then(|| "catalog_expired".into()),
                    observed_at_unix_ms: observed, valid_until_unix_ms: until,
                };
                health.validate().map_err(anyhow::Error::msg)?;
                return service_response(if stale {503} else {200}, &health);
            }
            // Never renew stale evidence merely because a consumer reads it.
            if stale { return response(503, "text/plain", "Tuner catalog is stale\n"); }
            if path == "/v1/streams" {
                let streams = snapshot.catalog.stream_descriptors(observed, until)?;
                let documents: Vec<_> = streams.into_iter().map(|(descriptor, endpoints)| {
                    serde_json::json!({"descriptor": descriptor, "endpoints": endpoints})
                }).collect();
                return service_response(200, &documents);
            }
            let catalog = snapshot.catalog.service_catalog(
                &format!("tls/{}", proxy.server_name),
                &format!("lan/{}", proxy.base.trim_start_matches("http://").replace(':', "/")),
                observed, until,
            )?;
            return service_response(200, &catalog);
        }
        if path == "/healthz" {
            let snapshot = proxy.snapshot.read().await;
            let age = snapshot.as_ref().map(|s| s.observed.elapsed().as_secs());
            let healthy = age.is_some_and(|age| age < STALE.as_secs());
            return response(if healthy {200} else {503}, "application/json", serde_json::json!({
                "adapter":"hdhomerun", "state":if healthy {"ready"} else {"degraded"}, "catalog_age_seconds":age,
                "private_transport":"quic-mtls", "stock_app_verified":false
            }).to_string());
        }
        let catalog = proxy.catalog().await?;
        if path == "/discover.json" { return response(200, "application/json", serde_json::to_vec(&catalog.device)?); }
        if path == "/lineup.json" { return response(200, "application/json", serde_json::to_vec(&catalog.channels)?); }
        if path == "/lineup.m3u" { return response(200, "application/x-mpegurl", playlist(&catalog)); }
        if path == "/lineup.xml" { return response(200, "application/xml", lineup_xml(&catalog)); }
        if path == "/" {
            return response(200, "text/html", "<!doctype html><title>Unibus HDHomeRun</title><h1>Private HDHomeRun proxy</h1><p><a href='/lineup.m3u'>Channel playlist</a> · <a href='/healthz'>Health</a></p>");
        }
        let operation = if path == "/lineup_status.json" {
            ensure!(uri.query().is_none(), "unsupported status query");
            Operation::LineupStatus
        } else {
            let Some((selector, channel)) = path.strip_prefix('/').and_then(|p| p.split_once("/v")) else {
                return response(404, "text/plain", "Unknown endpoint\n");
            };
            let tuner = if selector == "auto" { None } else {
                Some(selector.strip_prefix("tuner").context("invalid tuner selector")?.parse::<u8>()?)
            };
            let duration = match uri.query() {
                None => None,
                Some(query) => Some(query.strip_prefix("duration=").context("only duration is supported")?.parse::<u32>()?),
            };
            let operation = Operation::Stream { channel: channel.into(), tuner, duration };
            operation.stream_path(catalog.identity()?.1)?;
            ensure!(catalog.channels.iter().any(|c| c["GuideNumber"].as_str() == Some(channel)), "channel unavailable");
            if method == Method::HEAD {
                let mut result = response(200, "video/mpeg", Body::empty())?;
                native_media_response(&mut result);
                return Ok(result);
            }
            operation
        };
        let media = matches!(operation, Operation::Stream { .. });
        let (reply, recv) = proxy.request(operation).await?;
        let stream = futures_util::stream::try_unfold(MediaBody { recv, _permit: permit }, |mut state| async move {
            let chunk = timeout(DEADLINE, state.recv.read_chunk(64 * 1024, true)).await??;
            Ok::<_, anyhow::Error>(chunk.map(|chunk| (chunk.bytes, state)))
        });
        let mut response = response(reply.status, &reply.content_type, Body::from_stream(stream))?;
        if let Some(error) = reply.tuner_error {
            response.headers_mut().insert("X-HDHomeRun-Error", error.parse()?);
        }
        if media && reply.status == 200 {
            native_media_response(&mut response);
            if let Some(resource) = reply.resource {
                response.headers_mut().insert("X-HDHomeRun-Resource", resource.parse()?);
            }
        }
        Ok(response)
    }.await;
    let mut reply = match result {
        Ok(response) => response,
        Err(error) => {
            eprintln!("HDHomeRun LAN request failed: {error:#}");
            let allowed = matches!(client.ip(), IpAddr::V4(ip) if proxy.lan.contains(&ip));
            response(
                if allowed { 502 } else { 403 },
                "text/plain",
                "Tuner request unavailable; inspect adapter health\n",
            )
            .expect("static error response")
        }
    };
    if matches!(client.ip(), IpAddr::V4(ip) if proxy.lan.contains(&ip)) {
        apply_cors(&mut reply, &headers, &proxy.web_origins);
    }
    if uri.path() != "/healthz" {
        let origin = headers
            .get("Origin")
            .and_then(|v| v.to_str().ok())
            .filter(|v| v.len() <= 256);
        eprintln!(
            "HDHomeRun HTTP client={client} method={method} path={:?} query_keys={:?} origin={origin:?} status={} version={:?} cors_allowed={}",
            uri.path().chars().take(128).collect::<String>(),
            uri.query().map(|q| q
                .split('&')
                .take(8)
                .map(|pair| pair
                    .split('=')
                    .next()
                    .unwrap_or_default()
                    .chars()
                    .take(32)
                    .collect::<String>())
                .collect::<Vec<_>>()),
            reply.status(),
            reply.version(),
            cors_origin(&headers, &proxy.web_origins).is_some()
        );
    }
    reply
}

async fn discovery_loop(proxy: Arc<Proxy>, socket: UdpSocket) -> Result<()> {
    let mut bytes = [0; discovery::MAX_PACKET + 1];
    let mut window = Instant::now();
    let mut budget = 64u32;
    loop {
        let (length, client) = socket.recv_from(&mut bytes).await?;
        if !matches!(client.ip(), IpAddr::V4(ip) if proxy.lan.contains(&ip)) || client.port() == 0 {
            continue;
        }
        if window.elapsed() >= Duration::from_secs(1) {
            budget = 64;
            window = Instant::now();
        }
        if budget == 0 {
            continue;
        }
        budget -= 1;
        let Ok(catalog) = proxy.catalog().await else {
            continue;
        };
        let (device, tuners) = catalog.identity()?;
        let auth = catalog.device.get("DeviceAuth").and_then(Value::as_str);
        match discovery::response(&bytes[..length], device, tuners, &proxy.base, auth) {
            Ok(Some(reply)) => {
                socket.send_to(&reply, client).await?;
            }
            Ok(None) => {}
            Err(error) => eprintln!("Rejected HDHomeRun discovery request: {error:#}"),
        }
    }
}

async fn proxy_state(config: Config) -> Result<(Arc<Proxy>, Ipv4Addr, u16, u16)> {
    let Config::Proxy {
        owner,
        owner_server_name,
        lan_ip,
        allowed_lan,
        http_port,
        stream_port,
        share_device_auth,
        web_origins,
        identity,
    } = config
    else {
        anyhow::bail!("proxy role required");
    };
    let mut client = mutual_client_config_from(&identity.credentials()?)?;
    let mut transport = quinn::TransportConfig::default();
    transport
        .receive_window((1024 * 1024u32).into())
        .stream_receive_window((256 * 1024u32).into())
        .send_window(64 * 1024)
        .keep_alive_interval(Some(Duration::from_secs(10)))
        .max_idle_timeout(Some(Duration::from_secs(30).try_into()?));
    client.transport_config(Arc::new(transport));
    let proxy = Arc::new(Proxy {
        endpoint: QuicEndpoint::client(
            if owner.is_ipv4() {
                "0.0.0.0:0"
            } else {
                "[::]:0"
            }
            .parse()?,
            client,
        )?,
        peer: Mutex::new(None),
        owner,
        server_name: owner_server_name,
        identity,
        lan: allowed_lan,
        base: format!("http://{lan_ip}:{http_port}"),
        stream_base: format!("http://{lan_ip}:{stream_port}"),
        share_auth: share_device_auth,
        web_origins,
        snapshot: RwLock::new(None),
        requests: Arc::new(Semaphore::new(32)),
    });
    proxy
        .refresh()
        .await
        .context("verify owner before advertising on LAN")?;
    Ok((proxy, lan_ip, http_port, stream_port))
}

pub async fn run_proxy(config: Config) -> Result<()> {
    let (proxy, lan_ip, http_port, stream_port) = proxy_state(config).await?;
    let metadata = TcpListener::bind((lan_ip, http_port)).await?;
    let stream = if stream_port != http_port {
        Some(TcpListener::bind((lan_ip, stream_port)).await?)
    } else {
        None
    };
    // Wildcard receive is needed for IPv4 broadcasts. The CIDR allowlist gates every reply.
    let udp = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, discovery::PORT)).await?;
    let app = Router::new()
        .fallback(any(handle))
        .with_state(proxy.clone());
    let refresh = async {
        loop {
            tokio::time::sleep(Duration::from_secs(30)).await;
            if let Err(error) = proxy.refresh().await {
                eprintln!("HDHomeRun refresh degraded: {error:#}");
            }
        }
        #[allow(unreachable_code)]
        Ok::<(), anyhow::Error>(())
    };
    let second = async {
        if let Some(stream) = stream {
            axum::serve(
                stream,
                app.clone()
                    .into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await?;
        } else {
            std::future::pending::<()>().await;
        }
        Ok::<(), anyhow::Error>(())
    };
    eprintln!(
        "HDHomeRun LAN proxy ready at {}; discovery UDP {} (LAN {})",
        proxy.base,
        discovery::PORT,
        proxy.lan
    );
    tokio::select! {
        result = axum::serve(metadata, app.clone().into_make_service_with_connect_info::<SocketAddr>()) => result.map_err(Into::into),
        result = second => result,
        result = discovery_loop(proxy.clone(), udp) => result,
        result = refresh => result,
        _ = tokio::signal::ctrl_c() => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::StatusCode;
    use bytes::Bytes;
    use rcgen::{CertifiedKey, generate_simple_self_signed};
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn identity_pair(directory: &std::path::Path) -> (Identity, Identity) {
        let CertifiedKey {
            cert: owner,
            signing_key: owner_key,
        } = generate_simple_self_signed(vec!["owner.test".into()]).unwrap();
        let CertifiedKey {
            cert: proxy,
            signing_key: proxy_key,
        } = generate_simple_self_signed(vec!["proxy.test".into()]).unwrap();
        let ca = directory.join("ca.pem");
        std::fs::write(&ca, format!("{}{}", owner.pem(), proxy.pem())).unwrap();
        let make = |name: &str,
                    cert: &rcgen::Certificate,
                    key: &rcgen::KeyPair,
                    peer: &rcgen::Certificate| {
            let cert_file = directory.join(format!("{name}.pem"));
            let key_file = directory.join(format!("{name}.key"));
            std::fs::write(&cert_file, cert.pem()).unwrap();
            std::fs::write(&key_file, key.serialize_pem()).unwrap();
            Identity {
                ca_file: ca.clone(),
                cert_file,
                key_file,
                allowed_peer_bindings: vec![certificate_key_binding(peer.der())],
            }
        };
        (
            make("owner", &owner, &owner_key, &proxy),
            make("proxy", &proxy, &proxy_key, &owner),
        )
    }

    struct Active(Arc<AtomicUsize>);
    impl Drop for Active {
        fn drop(&mut self) {
            self.0.fetch_sub(1, Ordering::SeqCst);
        }
    }

    async fn mock_tuner(State(active): State<Arc<AtomicUsize>>, uri: Uri) -> Response {
        match uri.path() {
            "/discover.json" => response(200, "application/json", json!({"DeviceID":"10626E55", "TunerCount":2, "DeviceAuth":"not-for-logs", "FriendlyName":"HDHomeRun CONNECT DUO"}).to_string()).unwrap(),
            "/lineup.json" => response(200, "application/json", json!([
                {"GuideNumber":"4.1", "GuideName":"KOMO", "URL":"http://remote:5004/auto/v4.1"},
                {"GuideNumber":"9.1", "GuideName":"Protected", "DRM":1, "URL":"http://remote:5004/auto/v9.1"}
            ]).to_string()).unwrap(),
            "/lineup_status.json" => response(200, "application/json", "{\"ScanInProgress\":0}").unwrap(),
            "/auto/v4.1" | "/tuner0/v4.1" | "/tuner1/v4.1" => {
                active.fetch_add(1, Ordering::SeqCst);
                let stream = futures_util::stream::unfold(Active(active), |guard| async {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                    let mut bytes = vec![0u8;188]; bytes[0] = 0x47;
                    Some((Ok::<_, std::io::Error>(Bytes::from(bytes)), guard))
                });
                let mut result = response(200, "video/mpeg", Body::from_stream(stream)).unwrap();
                result.headers_mut().insert("X-HDHomeRun-Resource", "tuner0".parse().unwrap());
                native_media_response(&mut result);
                result
            }
            _ => response(404, "text/plain", "not found").unwrap(),
        }
    }

    async fn wait_active(active: &AtomicUsize, expected: usize) {
        timeout(Duration::from_secs(3), async {
            while active.load(Ordering::SeqCst) != expected {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn real_mtls_proxy_discovery_playback_busy_cancel_and_stale_state() {
        let directory = tempfile::tempdir().unwrap();
        let (owner_identity, proxy_identity) = identity_pair(directory.path());
        let active = Arc::new(AtomicUsize::new(0));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream = listener.local_addr().unwrap();
        let mock = tokio::spawn(
            axum::serve(
                listener,
                Router::new()
                    .fallback(any(mock_tuner))
                    .with_state(active.clone()),
            )
            .into_future(),
        );
        let reservation = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let owner_address = reservation.local_addr().unwrap();
        drop(reservation);
        let owner = tokio::spawn(run_owner(owner_address, upstream, upstream, owner_identity));
        let config = Config::Proxy {
            owner: owner_address,
            owner_server_name: "owner.test".into(),
            lan_ip: Ipv4Addr::LOCALHOST,
            allowed_lan: "127.0.0.1/32".parse().unwrap(),
            http_port: 8088,
            stream_port: 5004,
            share_device_auth: false,
            web_origins: vec!["https://my.hdhomerun.com".into(), "null".into()],
            identity: proxy_identity.clone(),
        };
        config.validate().unwrap();
        // QUIC connect waits through initial listener startup without a fixed sleep.
        let (proxy, _, _, _) = proxy_state(config).await.unwrap();
        assert_eq!(proxy.catalog().await.unwrap().channels.len(), 1);

        let udp = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let udp_address = udp.local_addr().unwrap();
        let discovery = tokio::spawn(discovery_loop(proxy.clone(), udp));
        let client_udp = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        // Independent canonical wildcard request (not encoded by the adapter).
        let mut request = vec![0, 2, 0, 12, 1, 4, 0, 0, 0, 1, 2, 4, 255, 255, 255, 255];
        request.extend_from_slice(&crc32fast::hash(&request).to_le_bytes());
        client_udp.send_to(&request, udp_address).await.unwrap();
        let mut bytes = [0; 1460];
        let (length, _) = timeout(Duration::from_secs(2), client_udp.recv_from(&mut bytes))
            .await
            .unwrap()
            .unwrap();
        assert!(
            bytes[..length]
                .windows(b"http://127.0.0.1:8088".len())
                .any(|v| v == b"http://127.0.0.1:8088")
        );

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let http = tokio::spawn(
            axum::serve(
                listener,
                Router::new()
                    .fallback(any(handle))
                    .with_state(proxy.clone())
                    .into_make_service_with_connect_info::<SocketAddr>(),
            )
            .into_future(),
        );
        let client = http_client().unwrap();
        let base = format!("http://{address}");
        let inventory: stream_descriptors::service::ServiceCatalog = client
            .get(format!("{base}/v1/catalog"))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        inventory.validate().unwrap();
        assert_eq!(inventory.resources.len(), 2);
        assert!(inventory.owner_reference.starts_with("tls/"));
        let health: stream_descriptors::service::ServiceHealth = client
            .get(format!("{base}/v1/health"))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(
            health.effective_state(unix_ms().unwrap()),
            stream_descriptors::service::HealthState::Healthy
        );
        let streams: Value = client
            .get(format!("{base}/v1/streams"))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(
            streams[0]["descriptor"]["stream_id"],
            inventory.resources[1].stream_ids[0]
        );
        assert!(streams.to_string().find("DeviceAuth").is_none());
        let device: Value = client
            .get(format!("{base}/discover.json"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(device["DeviceID"], "10626E55");
        assert!(device.get("DeviceAuth").is_none());
        for origin in ["https://my.hdhomerun.com", "null"] {
            let result = client
                .get(format!("{base}/discover.json"))
                .header("Origin", origin)
                .send()
                .await
                .unwrap();
            assert_eq!(result.status(), 200);
            assert_eq!(result.headers()["Access-Control-Allow-Origin"], origin);
        }
        let preflight = client
            .request(Method::OPTIONS, format!("{base}/discover.json"))
            .header("Origin", "https://my.hdhomerun.com")
            .header("Access-Control-Request-Method", "GET")
            .header("Access-Control-Request-Private-Network", "true")
            .send()
            .await
            .unwrap();
        assert_eq!(preflight.status(), 204);
        assert_eq!(
            preflight.headers()["Access-Control-Allow-Private-Network"],
            "true"
        );
        let bad = client
            .get(format!("{base}/discover.json"))
            .header("Origin", "https://untrusted.example")
            .send()
            .await
            .unwrap();
        assert!(bad.headers().get("Access-Control-Allow-Origin").is_none());
        for (origin, method) in [
            ("https://untrusted.example", "GET"),
            ("https://my.hdhomerun.com", "POST"),
        ] {
            let result = client
                .request(Method::OPTIONS, format!("{base}/discover.json"))
                .header("Origin", origin)
                .header("Access-Control-Request-Method", method)
                .send()
                .await
                .unwrap();
            assert_eq!(result.status(), 403);
        }
        let playlist = client
            .get(format!("{base}/lineup.m3u"))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert!(
            playlist.contains("http://127.0.0.1:5004/auto/v4.1") && !playlist.contains("remote")
        );
        assert_eq!(
            client
                .post(format!("{base}/lineup.post"))
                .send()
                .await
                .unwrap()
                .status(),
            405
        );

        let mut first = client
            .get(format!("{base}/auto/v4.1"))
            .send()
            .await
            .unwrap();
        assert_eq!(first.status(), 200);
        assert_eq!(first.version(), Version::HTTP_10);
        assert_eq!(first.headers()["Content-Type"], "video/mpeg");
        assert_eq!(first.headers()["X-HDHomeRun-Resource"], "tuner0");
        assert!(first.headers().get("Transfer-Encoding").is_none());
        let packet = first.chunk().await.unwrap().unwrap();
        assert_eq!(packet[0], 0x47);
        let mut second = client
            .get(format!("{base}/tuner1/v4.1"))
            .send()
            .await
            .unwrap();
        assert_eq!(second.status(), 200);
        second.chunk().await.unwrap().unwrap();
        let third = client
            .get(format!("{base}/auto/v4.1"))
            .send()
            .await
            .unwrap();
        assert_eq!(third.status(), 503);
        assert_eq!(
            third.headers()["X-HDHomeRun-Error"],
            "805 All Tuners In Use"
        );
        wait_active(&active, 2).await;
        drop(first);
        drop(second);
        wait_active(&active, 0).await;
        // Do not let an HTTP client silently dechunk and hide the wire format.
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut socket = tokio::net::TcpStream::connect(address).await.unwrap();
        socket
            .write_all(format!("GET /auto/v4.1 HTTP/1.1\r\nHost: {address}\r\n\r\n").as_bytes())
            .await
            .unwrap();
        let mut raw = Vec::new();
        let split = timeout(Duration::from_secs(3), async {
            loop {
                let mut bytes = [0; 2048];
                let count = socket.read(&mut bytes).await.unwrap();
                assert_ne!(count, 0);
                raw.extend_from_slice(&bytes[..count]);
                assert!(raw.len() < 64 * 1024);
                if let Some(split) = raw.windows(4).position(|v| v == b"\r\n\r\n")
                    && raw.len() >= split + 4 + 188
                {
                    break split + 4;
                }
            }
        })
        .await
        .unwrap();
        let headers = String::from_utf8_lossy(&raw[..split]).to_ascii_lowercase();
        assert!(headers.starts_with("http/1.0 200"));
        assert!(headers.contains("connection: close\r\n"));
        assert!(headers.contains("content-type: video/mpeg\r\n"));
        assert!(!headers.contains("transfer-encoding") && !headers.contains("content-length"));
        assert_eq!(
            raw[split], 0x47,
            "raw body must start with MPEG-TS, not chunk framing"
        );
        drop(socket);
        wait_active(&active, 0).await;

        // Upgrade owners first: v1 clients still receive exactly their old header schema.
        let peer = proxy.peer().await.unwrap();
        let (mut send, mut recv) = peer.connection().open_bi().await.unwrap();
        write_json(
            &mut send,
            &Request {
                contract: LEGACY_CONTRACT.into(),
                operation: Operation::Stream {
                    channel: "4.1".into(),
                    tuner: None,
                    duration: None,
                },
            },
        )
        .await
        .unwrap();
        send.finish().unwrap();
        let legacy_reply: Value = read_json(&mut recv).await.unwrap();
        assert_eq!(legacy_reply["status"], 200);
        assert!(legacy_reply.get("resource").is_none());
        recv.stop(0u32.into()).unwrap();
        drop(recv);
        wait_active(&active, 0).await;
        let (reply, recv) = proxy
            .request(Operation::Stream {
                channel: "9.1".into(),
                tuner: None,
                duration: None,
            })
            .await
            .unwrap();
        assert_eq!(reply.status, 503);
        assert!(reply.tuner_error.unwrap().starts_with("811"));
        drop(recv);
        let (reply, recv) = proxy
            .request(Operation::Stream {
                channel: "99.1".into(),
                tuner: None,
                duration: None,
            })
            .await
            .unwrap();
        assert_eq!(reply.status, 404);
        drop(recv);

        let forbidden = handle(
            State(proxy.clone()),
            ConnectInfo("192.168.99.1:1234".parse().unwrap()),
            Method::GET,
            "/discover.json".parse().unwrap(),
            HeaderMap::new(),
        )
        .await;
        assert_eq!(forbidden.status(), StatusCode::FORBIDDEN);
        assert!(
            forbidden
                .headers()
                .get("Access-Control-Allow-Origin")
                .is_none()
        );
        // Same trust store, wrong pinned peer: no catalog reaches the LAN.
        let mut wrong = proxy_identity.clone();
        wrong.allowed_peer_bindings = vec![format!("sha256:{}", "0".repeat(64))];
        let invalid = Config::Proxy {
            owner: owner_address,
            owner_server_name: "owner.test".into(),
            lan_ip: Ipv4Addr::LOCALHOST,
            allowed_lan: "127.0.0.1/32".parse().unwrap(),
            http_port: 8088,
            stream_port: 5004,
            share_device_auth: false,
            web_origins: vec![],
            identity: wrong,
        };
        assert!(proxy_state(invalid).await.is_err());
        proxy.snapshot.write().await.as_mut().unwrap().observed = Instant::now() - STALE;
        assert!(proxy.catalog().await.is_err());
        let health = client.get(format!("{base}/healthz")).send().await.unwrap();
        assert_eq!(health.status(), 503);
        let stale = client
            .get(format!("{base}/v1/health"))
            .send()
            .await
            .unwrap();
        assert_eq!(stale.status(), 503);
        assert_eq!(stale.json::<Value>().await.unwrap()["state"], "stale");
        assert_eq!(
            client
                .get(format!("{base}/v1/catalog"))
                .send()
                .await
                .unwrap()
                .status(),
            503
        );
        assert_eq!(
            client
                .get(format!("{base}/v1/streams"))
                .send()
                .await
                .unwrap()
                .status(),
            503
        );
        client_udp.send_to(&request, udp_address).await.unwrap();
        assert!(
            timeout(Duration::from_millis(100), client_udp.recv_from(&mut bytes))
                .await
                .is_err()
        );
        discovery.abort();
        http.abort();
        owner.abort();
        mock.abort();
    }

    #[test]
    fn deployment_policy_rejects_public_and_unscoped_endpoints() {
        let directory = tempfile::tempdir().unwrap();
        let (identity, _) = identity_pair(directory.path());
        for address in ["0.0.0.0:18504", "8.8.8.8:18504"] {
            let config = Config::Owner {
                listen: address.parse().unwrap(),
                tuner_ip: "192.168.0.208".parse().unwrap(),
                metadata_port: 80,
                media_port: 5004,
                identity: identity.clone(),
            };
            assert!(config.validate().is_err());
        }
    }

    #[test]
    fn web_origins_are_exact_and_bounded() {
        for origin in ["https://my.hdhomerun.com", "http://localhost:8080", "null"] {
            assert!(valid_origin(origin));
        }
        for origin in [
            "*",
            "https://*.example.com",
            "https://example.com/path",
            "file:///x",
            "https://user:password@example.com",
            "https://example.com?x=1",
        ] {
            assert!(!valid_origin(origin), "{origin}");
        }
    }
}
