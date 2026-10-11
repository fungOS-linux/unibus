//! Optional read-only projection and HTTPS viewer. No network discovery or proxy.
//! Certificates establish a configured identity binding; existing node grants
//! determine visible capabilities/zones. No URLs, credentials or actions escape.
use crate::{
    Hub,
    config::{RouteRule, RouteTarget},
    registry::{EdgeNode, HEARTBEAT_TTL},
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::{Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};
use unibus::grants::NodeGrant;

pub const VIEWER_CAPABILITY: &str = "unibus.viewer";
const MAX_HEADER: usize = 8192;
const MAX_ENTRIES: usize = 512;
const MAX_RESPONSE: usize = 1024 * 1024;
const DEADLINE: Duration = Duration::from_secs(10);
const CSS: &str = include_str!("viewer/coherence-product.css");
const LAYOUT: &str = include_str!("viewer/layout.css");
mod advertisement;

#[derive(Debug, Serialize)]
pub struct Snapshot {
    pub schema_version: u32,
    pub installation_id: String,
    pub observed_at_unix_ms: u64,
    pub scope: &'static str,
    pub resources: Vec<Resource>,
}
#[derive(Debug, Serialize)]
pub struct Resource {
    pub id: String,
    pub kind: &'static str,
    pub title: String,
    pub state: &'static str,
    pub capabilities: Vec<String>,
    pub zones: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub heartbeat_age_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wire_schema: Option<u32>,
    pub preview: &'static str,
}
/// One projection shared by JSON, HTML and future Canvas adapters. Fail closed
/// on excess size; never return a silently truncated or unfiltered inventory.
pub fn project<'a>(
    installation: &str,
    grant: &NodeGrant,
    nodes: impl Iterator<Item = &'a EdgeNode>,
    routes: &[RouteRule],
) -> Result<Snapshot> {
    ensure!(
        grant.capabilities.iter().any(|c| c == VIEWER_CAPABILITY),
        "viewer capability required"
    );
    let allowed: BTreeSet<_> = grant
        .capabilities
        .iter()
        .filter(|c| c.as_str() != VIEWER_CAPABILITY)
        .collect();
    let mut resources = Vec::new();
    for node in nodes {
        let zones: Vec<_> = node
            .zones
            .iter()
            .filter(|z| grant.zones.contains(z))
            .cloned()
            .collect();
        let capabilities: Vec<_> = node
            .capabilities
            .iter()
            .filter(|c| allowed.contains(c))
            .cloned()
            .collect();
        if zones.is_empty() || capabilities.is_empty() {
            continue;
        }
        resources.push(Resource {
            id: format!("node/{}", node.producer_id),
            kind: "registration",
            title: node.producer_id.clone(),
            state: if node.is_alive() { "live" } else { "stale" },
            capabilities,
            zones,
            heartbeat_age_ms: Some(node.last_heartbeat.elapsed().as_millis().try_into()?),
            wire_schema: Some(node.schema_version),
            preview: "metadata",
        });
        ensure!(
            resources.len() <= MAX_ENTRIES,
            "viewer inventory exceeds entry budget"
        );
    }
    for route in routes {
        let capability = match &route.target {
            RouteTarget::Capability { name } | RouteTarget::AnyHolder { name } => name,
            RouteTarget::Producer => continue,
        };
        if !allowed.contains(capability) || grant.zones.is_empty() {
            continue;
        }
        resources.push(Resource {
            id: format!("route/{}", route.kind),
            kind: "route",
            title: route.kind.clone(),
            state: "configured",
            capabilities: vec![capability.clone()],
            zones: grant.zones.clone(),
            heartbeat_age_ms: None,
            wire_schema: None,
            preview: "metadata",
        });
        ensure!(
            resources.len() <= MAX_ENTRIES,
            "viewer inventory exceeds entry budget"
        );
    }
    resources.sort_by(|a, b| a.id.cmp(&b.id));
    ensure!(
        resources.windows(2).all(|r| r[0].id != r[1].id),
        "duplicate viewer resource identity"
    );
    let snapshot = Snapshot {
        schema_version: 1,
        installation_id: installation.into(),
        observed_at_unix_ms: unibus::envelope::now_ms(),
        scope: "local-daemon-authorized",
        resources,
    };
    ensure!(
        serde_json::to_vec(&snapshot)?.len() <= MAX_RESPONSE,
        "viewer snapshot exceeds byte budget"
    );
    Ok(snapshot)
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub listen: SocketAddr,
    pub ca_file: PathBuf,
    pub cert_file: PathBuf,
    pub key_file: PathBuf,
    /// Bootstrap certificate SHA-256 -> existing node-grant identity. Never
    /// derive this from observations or import another peer's private key.
    pub certificate_bindings: BTreeMap<String, String>,
    /// Optional application-owned Mycelium lease file; never an access grant.
    #[serde(default)]
    pub advertisement: Option<advertisement::Advertisement>,
}
impl Config {
    pub fn validate(&self) -> Result<()> {
        if let Some(advertisement) = &self.advertisement {
            advertisement.validate(self.listen)?;
        }
        ensure!(
            self.listen.port() != 0
                && self.listen.port() != 80
                && !self.listen.ip().is_unspecified()
                && !self.listen.ip().is_multicast(),
            "viewer needs explicit HTTPS address and port"
        );
        ensure!(
            [&self.ca_file, &self.cert_file, &self.key_file]
                .iter()
                .all(|p| p.is_absolute()),
            "viewer credential paths must be absolute"
        );
        ensure!(
            !self.certificate_bindings.is_empty() && self.certificate_bindings.len() <= 64,
            "viewer needs 1..64 certificate bindings"
        );
        for (digest, principal) in &self.certificate_bindings {
            ensure!(
                digest.len() == 64
                    && digest
                        .bytes()
                        .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
                    && !principal.is_empty()
                    && principal.len() <= 128
                    && !principal.chars().any(char::is_control),
                "invalid viewer certificate binding"
            );
        }
        Ok(())
    }
}
fn read_config(path: &Path) -> Result<Config> {
    let metadata = std::fs::metadata(path)?;
    ensure!(metadata.is_file(), "viewer config must be a regular file");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        ensure!(
            metadata.permissions().mode() & 0o077 == 0,
            "viewer config must be owner-only"
        );
    }
    let mut bytes = Vec::new();
    std::fs::File::open(path)?
        .take(16 * 1024 + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= 16 * 1024,
        "viewer config exceeds byte budget"
    );
    let config: Config = serde_json::from_slice(&bytes)
        .map_err(|_| anyhow::anyhow!("invalid viewer configuration"))?;
    config.validate()?;
    Ok(config)
}
fn tls(config: &Config) -> Result<Arc<rustls::ServerConfig>> {
    let certificates = |path: &Path| -> Result<Vec<rustls::pki_types::CertificateDer<'static>>> {
        let mut reader = std::io::BufReader::new(std::fs::File::open(path)?);
        Ok(rustls_pemfile::certs(&mut reader).collect::<std::io::Result<_>>()?)
    };
    let mut roots = rustls::RootCertStore::empty();
    for cert in certificates(&config.ca_file)? {
        roots.add(cert)?;
    }
    let verifier = rustls::server::WebPkiClientVerifier::builder_with_provider(
        Arc::new(roots),
        Arc::new(rustls::crypto::ring::default_provider()),
    )
    .build()?;
    let key = rustls_pemfile::private_key(&mut std::io::BufReader::new(std::fs::File::open(
        &config.key_file,
    )?))?
    .context("viewer private key missing")?;
    let server = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()?
    .with_client_cert_verifier(verifier)
    .with_single_cert(certificates(&config.cert_file)?, key)?;
    Ok(Arc::new(server))
}
pub fn start_optional(hub: Arc<Hub>) -> Result<()> {
    let Some(path) = std::env::var_os("UNIBUS_VIEWER_CONFIG") else {
        return Ok(());
    };
    let path = PathBuf::from(path);
    ensure!(
        path.is_absolute(),
        "viewer configuration path must be absolute"
    );
    let config = read_config(&path)?;
    let server = tls(&config)?;
    let listener = TcpListener::bind(config.listen).context("bind optional viewer")?;
    let advertisement_alive = config
        .advertisement
        .as_ref()
        .map(|ad| advertisement::start(ad.clone(), config.listen, path.clone()))
        .transpose()?;
    let address = config.listen;
    let active = Arc::new(AtomicUsize::new(0));
    std::thread::Builder::new()
        .name("unibus-viewer".into())
        .spawn(move || {
            for socket in listener.incoming() {
                let socket = match socket {
                    Ok(socket) => socket,
                    Err(error) => {
                        tracing::error!(%error,"viewer accept failed");
                        if let Some(alive) = &advertisement_alive {
                            alive.store(false, Ordering::Release);
                        }
                        break;
                    }
                };
                if active.fetch_add(1, Ordering::AcqRel) >= 32 {
                    active.fetch_sub(1, Ordering::AcqRel);
                    continue;
                }
                let active = active.clone();
                let hub = hub.clone();
                let path = path.clone();
                let server = server.clone();
                let permit = Permit(active);
                if let Err(error) = std::thread::Builder::new()
                    .name("unibus-viewer-request".into())
                    .spawn(move || {
                        let _permit = permit;
                        if let Err(error) = connection(socket, server, &path, address, &hub) {
                            // Do not log request headers, certificate bindings or credentials.
                            tracing::debug!(%error,"viewer request rejected");
                        }
                    })
                {
                    tracing::error!(%error, "viewer request thread failed");
                }
            }
        })?;
    tracing::info!(%address,"optional authenticated Unibus viewer enabled");
    Ok(())
}
struct Permit(Arc<AtomicUsize>);
impl Drop for Permit {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}
struct DeadlineSocket {
    socket: TcpStream,
    started: Instant,
}
impl DeadlineSocket {
    fn remaining(&self) -> std::io::Result<Duration> {
        let remaining = DEADLINE.saturating_sub(self.started.elapsed());
        if remaining.is_zero() {
            return Err(std::io::Error::from(std::io::ErrorKind::TimedOut));
        }
        Ok(remaining.min(Duration::from_secs(2)))
    }
}
impl Read for DeadlineSocket {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.socket.set_read_timeout(Some(self.remaining()?))?;
        self.socket.read(buf)
    }
}
impl Write for DeadlineSocket {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.socket.set_write_timeout(Some(self.remaining()?))?;
        self.socket.write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.remaining()?;
        self.socket.flush()
    }
}
fn connection(
    socket: TcpStream,
    server: Arc<rustls::ServerConfig>,
    path: &Path,
    address: SocketAddr,
    hub: &Hub,
) -> Result<()> {
    socket.set_read_timeout(Some(Duration::from_secs(2)))?;
    socket.set_write_timeout(Some(Duration::from_secs(2)))?;
    let started = Instant::now();
    let mut stream = rustls::StreamOwned::new(
        rustls::ServerConnection::new(server)?,
        DeadlineSocket { socket, started },
    );
    let mut header = Vec::new();
    loop {
        ensure!(
            started.elapsed() < DEADLINE,
            "viewer request deadline exceeded"
        );
        let mut byte = [0];
        ensure!(stream.read(&mut byte)? == 1, "incomplete viewer request");
        header.push(byte[0]);
        ensure!(
            header.len() <= MAX_HEADER,
            "viewer header exceeds byte budget"
        );
        if header.ends_with(b"\r\n\r\n") {
            break;
        }
    }
    let digest = stream
        .conn
        .peer_certificates()
        .and_then(|c| c.first())
        .map(|cert| format!("{:x}", Sha256::digest(cert.as_ref())))
        .context("viewer needs client certificate")?;
    let fresh = read_config(path)?;
    ensure!(
        fresh.listen == address,
        "viewer listener changed; restart required"
    );
    let Some(principal) = fresh.certificate_bindings.get(&digest) else {
        return response(&mut stream, 403, "text/plain", b"Access denied");
    };
    let snapshot = match hub.viewer_snapshot(principal) {
        Ok(s) => s,
        Err(_) => return response(&mut stream, 403, "text/plain", b"Access denied"),
    };
    let request = std::str::from_utf8(&header)?;
    let Some(target) = parse_request(request) else {
        return response(&mut stream, 400, "text/plain", b"Invalid request");
    };
    match target {
        "/coherence.css" => response(&mut stream, 200, "text/css; charset=utf-8", CSS.as_bytes()),
        "/viewer.css" => response(
            &mut stream,
            200,
            "text/css; charset=utf-8",
            LAYOUT.as_bytes(),
        ),
        "/api/index" => response(
            &mut stream,
            200,
            "application/json",
            &serde_json::to_vec(&snapshot)?,
        ),
        "/" => response(
            &mut stream,
            200,
            "text/html; charset=utf-8",
            render(&snapshot, None).as_bytes(),
        ),
        _ if target.starts_with("/?q=") => {
            let query = decode_query(&target[4..])?;
            response(
                &mut stream,
                200,
                "text/html; charset=utf-8",
                render(&snapshot, Some(&query)).as_bytes(),
            )
        }
        _ => response(&mut stream, 404, "text/plain", b"Not found"),
    }
}
fn parse_request(request: &str) -> Option<&str> {
    let mut lines = request.split("\r\n");
    let mut parts = lines.next()?.split_whitespace();
    let method = parts.next()?;
    let target = parts.next()?;
    let version = parts.next()?;
    if method != "GET"
        || version != "HTTP/1.1"
        || parts.next().is_some()
        || !target.starts_with('/')
        || target.len() > 1024
    {
        return None;
    }
    let mut hosts = 0;
    for line in lines.filter(|l| !l.is_empty()) {
        let (name, value) = line.split_once(':')?;
        if name.eq_ignore_ascii_case("host") {
            hosts += 1;
            if value.trim().is_empty() {
                return None;
            }
        }
        if name.eq_ignore_ascii_case("transfer-encoding")
            || (name.eq_ignore_ascii_case("content-length") && value.trim() != "0")
        {
            return None;
        }
    }
    (hosts == 1).then_some(target)
}
fn response(stream: &mut impl Write, status: u16, kind: &str, body: &[u8]) -> Result<()> {
    ensure!(
        body.len() <= MAX_RESPONSE,
        "viewer response exceeds byte budget"
    );
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        403 => "Forbidden",
        _ => "Not Found",
    };
    write!(
        stream,
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\nReferrer-Policy: no-referrer\r\nContent-Security-Policy: default-src 'none'; style-src 'self'; form-action 'self'; frame-ancestors 'none'; base-uri 'none'\r\n\r\n",
        body.len()
    )?;
    stream.write_all(body)?;
    stream.flush()?;
    Ok(())
}
fn escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}
fn decode_query(value: &str) -> Result<String> {
    ensure!(value.len() <= 768, "viewer query exceeds budget");
    let mut bytes = Vec::new();
    let mut input = value.bytes();
    while let Some(byte) = input.next() {
        bytes.push(match byte {
            b'+' => b' ',
            b'%' => {
                let hi = char::from(input.next().context("invalid query")?)
                    .to_digit(16)
                    .context("invalid query")?;
                let lo = char::from(input.next().context("invalid query")?)
                    .to_digit(16)
                    .context("invalid query")?;
                (hi * 16 + lo) as u8
            }
            _ => byte,
        });
    }
    let decoded = String::from_utf8(bytes)?;
    ensure!(
        decoded.len() <= 256 && !decoded.chars().any(char::is_control),
        "invalid viewer query"
    );
    Ok(decoded)
}
pub fn render(snapshot: &Snapshot, query: Option<&str>) -> String {
    use std::fmt::Write as _;
    let q = query.unwrap_or("");
    let needle = q.to_lowercase();
    let mut html = format!(
        "<!doctype html><html lang=\"en\" data-co-profile=\"product\" data-co-theme=\"dark\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><title>Unibus · Index</title><link rel=\"stylesheet\" href=\"/coherence.css\"><link rel=\"stylesheet\" href=\"/viewer.css\"></head><body><main class=\"co-container\"><header><p class=\"co-muted\">UNIBUS / RESOURCE INDEX</p><h1>Your connected environment</h1><p>Owner: {} · Local daemon, authorized scope · Read only</p></header><form action=\"/\" method=\"get\"><label for=\"search\">Find a resource or capability</label><input id=\"search\" class=\"co-input\" name=\"q\" type=\"search\" maxlength=\"256\" value=\"{}\"><button class=\"co-btn\" type=\"submit\">Search</button> <a class=\"co-btn\" href=\"/\">Refresh</a></form><p class=\"co-muted\">{} resources in scope. Heartbeat TTL: {} seconds. Snapshot: {} ms since epoch.</p><section class=\"resource-grid\" aria-label=\"Resources\">",
        escape(&snapshot.installation_id),
        escape(q),
        snapshot.resources.len(),
        HEARTBEAT_TTL.as_secs(),
        snapshot.observed_at_unix_ms
    );
    let mut visible = 0;
    for r in &snapshot.resources {
        if !format!(
            "{} {} {} {}",
            r.title,
            r.kind,
            r.capabilities.join(" "),
            r.zones.join(" ")
        )
        .to_lowercase()
        .contains(&needle)
        {
            continue;
        }
        visible += 1;
        let _ = write!(
            html,
            "<article class=\"co-card\"><p class=\"co-card__meta\">{} · {}</p><h2>{}</h2><p>{}</p><p>Zones: {}</p><details><summary>Resource details</summary><dl><dt>Identity</dt><dd>{}</dd><dt>Preview</dt><dd>Metadata only — no payload access granted by this index</dd><dt>Heartbeat age</dt><dd>{}</dd><dt>Wire schema</dt><dd>{}</dd></dl></details></article>",
            escape(r.kind),
            escape(r.state),
            escape(&r.title),
            escape(&r.capabilities.join(" · ")),
            escape(&r.zones.join(", ")),
            escape(&r.id),
            r.heartbeat_age_ms
                .map(|n| format!("{n} ms"))
                .unwrap_or_else(|| "Not observed".into()),
            r.wire_schema
                .map(|n| n.to_string())
                .unwrap_or_else(|| "Not applicable".into())
        );
    }
    if visible == 0 {
        html.push_str("<p>No authorized resources match. Missing registrations are not reported as healthy.</p>");
    }
    html.push_str("</section><footer><p>This is not a full mesh inventory. Mycelium owns discovery; Unibus enforces access. Refresh to update evidence.</p><a href=\"/api/index\">Typed JSON index</a></footer></main></body></html>");
    html
}

#[cfg(test)]
mod tests {
    use super::*;
    fn grant() -> NodeGrant {
        serde_json::from_value(serde_json::json!({"producer_id":"reader",
            "capabilities":[VIEWER_CAPABILITY,"canvas"],"zones":["home"]}))
        .unwrap()
    }
    fn node(id: &str, zone: &str, capability: &str) -> EdgeNode {
        let mut g = grant();
        g.producer_id = id.into();
        g.zones = vec![zone.into()];
        g.capabilities = vec![capability.into()];
        EdgeNode::new(&g)
    }
    #[test]
    fn projection_hides_other_zones_and_capabilities_and_requires_viewer_grant() {
        let nodes = [
            node("neo", "home", "canvas"),
            node("secret", "lab", "canvas"),
            node("private", "home", "adb"),
        ];
        let snapshot = project("local", &grant(), nodes.iter(), &[]).unwrap();
        assert_eq!(snapshot.resources.len(), 1);
        assert_eq!(snapshot.resources[0].id, "node/neo");
        let json = serde_json::to_string(&snapshot).unwrap();
        assert!(!json.contains("secret") && !json.contains("private") && !json.contains("token"));
        let mut denied = grant();
        denied.capabilities = vec!["canvas".into()];
        assert!(project("local", &denied, nodes.iter(), &[]).is_err());
    }
    #[test]
    fn expired_registration_is_stale_and_routes_are_not_claimed_live() {
        let mut edge = node("neo", "home", "canvas");
        edge.last_heartbeat = Instant::now() - HEARTBEAT_TTL - Duration::from_secs(1);
        let routes = [
            RouteRule {
                kind: "canvas.frame".into(),
                target: RouteTarget::Capability {
                    name: "canvas".into(),
                },
            },
            RouteRule {
                kind: "hidden.frame".into(),
                target: RouteTarget::AnyHolder {
                    name: "private".into(),
                },
            },
        ];
        let snapshot = project("local", &grant(), [&edge].into_iter(), &routes).unwrap();
        assert_eq!(snapshot.resources.len(), 2);
        assert_eq!(snapshot.resources[0].state, "stale");
        assert_eq!(snapshot.resources[1].state, "configured");
    }
    #[test]
    fn rendering_escapes_markup_in_resources_installation_and_search() {
        let edge = node("<img src=x onerror=alert(1)>", "home", "canvas");
        let snapshot =
            project("<script>bad()</script>", &grant(), [&edge].into_iter(), &[]).unwrap();
        let html = render(&snapshot, None);
        assert!(!html.contains("<img ") && !html.contains("<script>"));
        assert!(
            html.contains("&lt;img") && html.contains("co-card") && html.contains("/coherence.css")
        );
        let html = render(&snapshot, Some("\" autofocus onfocus=\"bad"));
        assert!(html.contains("&quot;") && !html.contains("value=\"\" autofocus"));
    }
    #[test]
    fn bounded_request_parser_rejects_actions_bodies_and_ambiguous_hosts() {
        assert_eq!(
            parse_request("GET /api/index HTTP/1.1\r\nHost: unibus.example\r\n\r\n"),
            Some("/api/index")
        );
        for bad in [
            "POST / HTTP/1.1\r\nHost: x\r\n\r\n",
            "GET / HTTP/1.1\r\n\r\n",
            "GET / HTTP/1.1\r\nHost: x\r\nHost: y\r\n\r\n",
            "GET / HTTP/1.1\r\nHost: x\r\nContent-Length: 1\r\n\r\n",
            "GET / HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked\r\n\r\n",
        ] {
            assert!(parse_request(bad).is_none());
        }
        assert_eq!(
            decode_query("canvas+neo%2Fdisplay").unwrap(),
            "canvas neo/display"
        );
        assert!(decode_query("%0a").is_err());
        assert!(decode_query("%xx").is_err());
    }
    #[test]
    fn configuration_is_explicit_and_bounded() {
        let mut config = Config {
            listen: "127.0.0.1:8444".parse().unwrap(),
            ca_file: "/private/ca.pem".into(),
            cert_file: "/private/server.pem".into(),
            key_file: "/private/key.pem".into(),
            certificate_bindings: BTreeMap::from([("a".repeat(64), "reader".into())]),
            advertisement: None,
        };
        config.validate().unwrap();
        config.listen = "0.0.0.0:8444".parse().unwrap();
        assert!(config.validate().is_err());
        config.listen = "127.0.0.1:80".parse().unwrap();
        assert!(config.validate().is_err());
        config.listen = "127.0.0.1:8444".parse().unwrap();
        config.key_file = "relative.pem".into();
        assert!(config.validate().is_err());
    }
    #[test]
    fn index_is_not_silently_truncated_and_duplicate_ids_fail() {
        let nodes: Vec<_> = (0..513)
            .map(|i| node(&format!("node{i}"), "home", "canvas"))
            .collect();
        assert!(project("local", &grant(), nodes.iter(), &[]).is_err());
        let edge = node("duplicate", "home", "canvas");
        assert!(project("local", &grant(), [&edge, &edge].into_iter(), &[]).is_err());
    }
}
