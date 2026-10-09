//! Optional Mycelium-backed discovery and identity providers for Unibus.
//!
//! This adapter speaks Mycelium's public newline-delimited JSON-RPC boundary.
//! Mycelium does not import Unibus, and Unibus core does not import Mycelium.
//! Removing this crate leaves static and DNS discovery fully usable.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, BufReader, Write};
use std::net::{IpAddr, Ipv6Addr};
use std::path::PathBuf;
use std::time::Duration;

use serde::Deserialize;
pub mod access;
use unibus::{
    NetworkEndpoint, NetworkEndpointError, PeerCandidate, PeerDirectory, PeerError,
    ServiceDirectory, ServiceObservation, Transport,
};
use unibus_quic::{PeerCredentialMaterial, PeerCredentials, PemPeerCredentials};

/// Loads the enrolled Mycelium peer identity through Unibus's provider-neutral
/// credential boundary. Mycelium remains unaware of Unibus traffic and policy.
#[derive(Debug, Clone)]
pub struct MyceliumCredentials {
    inner: PemPeerCredentials,
}

impl MyceliumCredentials {
    /// Use the stable layout produced by `mycelium enroll install`.
    pub fn from_home(home: impl Into<PathBuf>) -> Self {
        let home = home.into();
        Self::new(
            home.join("pki/ca.pem"),
            home.join("pki/node.pem"),
            home.join("pki/node-key.pem"),
        )
    }

    /// Use explicitly selected Mycelium peer credential files. This also
    /// supports installations whose service environment overrides the default
    /// enrollment paths.
    pub fn new(
        ca_file: impl Into<PathBuf>,
        cert_file: impl Into<PathBuf>,
        key_file: impl Into<PathBuf>,
    ) -> Self {
        Self {
            inner: PemPeerCredentials::new(ca_file, cert_file, key_file),
        }
    }
}

impl PeerCredentials for MyceliumCredentials {
    fn load(&self) -> anyhow::Result<PeerCredentialMaterial> {
        self.inner.load()
    }
}

#[derive(Debug, Clone)]
pub struct MyceliumDirectory {
    socket_path: PathBuf,
    quic_port: u16,
    server_name_suffix: String,
    candidate_ttl: Duration,
}

impl MyceliumDirectory {
    pub fn new(
        socket_path: PathBuf,
        quic_port: u16,
        server_name_suffix: impl Into<String>,
        candidate_ttl: Duration,
    ) -> Result<Self, PeerError> {
        let server_name_suffix = server_name_suffix.into();
        if socket_path.as_os_str().is_empty()
            || quic_port == 0
            || server_name_suffix.trim_matches('.').is_empty()
            || candidate_ttl.is_zero()
            || candidate_ttl > Duration::from_secs(300)
        {
            return Err(PeerError("invalid Mycelium directory configuration".into()));
        }
        Ok(Self {
            socket_path,
            quic_port,
            server_name_suffix: server_name_suffix.trim_matches('.').to_owned(),
            candidate_ttl,
        })
    }

    fn candidate_from_topology(
        &self,
        topology: Topology,
        peer_id: &str,
        now_unix_s: u64,
    ) -> Result<PeerCandidate, PeerError> {
        let node = topology
            .nodes
            .into_values()
            .find(|node| {
                node.id == peer_id
                    || node.hostnames.contains(peer_id)
                    || node.annotation.name.as_deref() == Some(peer_id)
            })
            .ok_or_else(|| PeerError(format!("Mycelium does not know peer {peer_id}")))?;
        let server_name = format!("{peer_id}.{}", self.server_name_suffix);
        let mut endpoints = node
            .ips
            .into_keys()
            .filter_map(|address| address.parse::<IpAddr>().ok())
            .filter(|address| usable_unicast(*address))
            .map(|address| NetworkEndpoint {
                transport: Transport::Quic,
                host: address.to_string(),
                port: self.quic_port,
                server_name: Some(server_name.clone()),
                priority: endpoint_priority(address),
                weight: 0,
            })
            .collect::<Vec<_>>();
        endpoints.sort_by_key(|endpoint| (endpoint.priority, endpoint.host.clone()));
        endpoints.dedup();
        let candidate = PeerCandidate {
            peer_id: peer_id.to_owned(),
            site: node.sites.into_iter().next(),
            endpoints,
            iroh_endpoints: vec![],
            expires_at_unix_s: now_unix_s.saturating_add(self.candidate_ttl.as_secs()),
        };
        candidate.validate(now_unix_s)?;
        Ok(candidate)
    }
}

impl PeerDirectory for MyceliumDirectory {
    fn resolve(&self, peer_id: &str, now_unix_s: u64) -> Result<PeerCandidate, PeerError> {
        let topology = request_topology(&self.socket_path)?;
        self.candidate_from_topology(topology, peer_id, now_unix_s)
    }
}

/// Imports Mycelium's existing provider-neutral service scan through its
/// public RPC boundary. This adapter performs no discovery of its own.
#[derive(Debug, Clone)]
pub struct MyceliumServiceDirectory {
    socket_path: PathBuf,
}

impl MyceliumServiceDirectory {
    pub fn new(socket_path: PathBuf) -> Result<Self, NetworkEndpointError> {
        if socket_path.as_os_str().is_empty() {
            return Err(NetworkEndpointError(
                "invalid Mycelium service directory configuration".into(),
            ));
        }
        Ok(Self { socket_path })
    }
}

impl ServiceDirectory for MyceliumServiceDirectory {
    fn services(&self, kind: &str) -> Result<Vec<ServiceObservation>, NetworkEndpointError> {
        if kind.trim().is_empty() {
            return Err(NetworkEndpointError("service kind is empty".into()));
        }
        let catalog = request_services(&self.socket_path)?;
        let mut observations = catalog
            .observations
            .into_iter()
            .filter(|observation| observation.value.kind == kind)
            .map(|observation| {
                let service = ServiceObservation {
                    service_id: observation.service_id,
                    kind: observation.value.kind,
                    endpoints: observation.value.endpoints,
                    observed_at_unix_s: observation.observed_at,
                    expires_at_unix_s: observation.expires_at,
                    provenance_provider: observation.provenance.provider,
                    provenance_observer: observation.provenance.observer,
                    attributes: observation.value.attributes,
                };
                service.validate()?;
                Ok(service)
            })
            .collect::<Result<Vec<_>, NetworkEndpointError>>()?;
        observations.sort_by(|left, right| left.service_id.cmp(&right.service_id));
        Ok(observations)
    }
}

#[derive(Debug, Deserialize)]
struct RpcResponse {
    ok: bool,
    #[serde(default)]
    result: Option<Topology>,
    #[serde(default)]
    error: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ServiceRpcResponse {
    ok: bool,
    #[serde(default)]
    result: Option<ServiceCatalog>,
    #[serde(default)]
    error: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ServiceCatalog {
    #[serde(default)]
    observations: Vec<MyceliumServiceObservation>,
}

#[derive(Debug, Deserialize)]
struct MyceliumServiceObservation {
    service_id: String,
    observed_at: u64,
    expires_at: u64,
    provenance: ServiceProvenance,
    value: ObservedService,
}

#[derive(Debug, Deserialize)]
struct ServiceProvenance {
    provider: String,
    observer: String,
}

#[derive(Debug, Deserialize)]
struct ObservedService {
    kind: String,
    #[serde(default)]
    endpoints: Vec<String>,
    #[serde(default)]
    attributes: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize)]
struct Topology {
    nodes: BTreeMap<String, Node>,
}

#[derive(Debug, Deserialize)]
struct Node {
    id: String,
    #[serde(default)]
    ips: BTreeMap<String, serde_json::Value>,
    #[serde(default)]
    hostnames: BTreeSet<String>,
    #[serde(default)]
    sites: BTreeSet<String>,
    #[serde(default)]
    annotation: Annotation,
}

#[derive(Debug, Default, Deserialize)]
struct Annotation {
    #[serde(default)]
    name: Option<String>,
}

#[cfg(unix)]
fn request_topology(socket_path: &std::path::Path) -> Result<Topology, PeerError> {
    use std::os::unix::net::UnixStream;

    let mut stream = UnixStream::connect(socket_path).map_err(|error| {
        PeerError(format!(
            "connect Mycelium socket {}: {error}",
            socket_path.display()
        ))
    })?;
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .map_err(|error| PeerError(format!("set Mycelium read timeout: {error}")))?;
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .map_err(|error| PeerError(format!("set Mycelium write timeout: {error}")))?;
    stream
        .write_all(b"{\"method\":\"topology\"}\n")
        .map_err(|error| PeerError(format!("request Mycelium topology: {error}")))?;
    stream
        .flush()
        .map_err(|error| PeerError(format!("flush Mycelium request: {error}")))?;
    let mut line = String::new();
    BufReader::new(stream)
        .read_line(&mut line)
        .map_err(|error| PeerError(format!("read Mycelium topology: {error}")))?;
    let response: RpcResponse = serde_json::from_str(&line)
        .map_err(|error| PeerError(format!("decode Mycelium topology: {error}")))?;
    if !response.ok {
        return Err(PeerError(format!(
            "Mycelium topology failed: {}",
            response.error.unwrap_or_else(|| "unknown error".into())
        )));
    }
    response
        .result
        .ok_or_else(|| PeerError("Mycelium topology response has no result".into()))
}

#[cfg(unix)]
fn request_services(socket_path: &std::path::Path) -> Result<ServiceCatalog, NetworkEndpointError> {
    use std::os::unix::net::UnixStream;

    let mut stream = UnixStream::connect(socket_path).map_err(|error| {
        NetworkEndpointError(format!(
            "connect Mycelium socket {}: {error}",
            socket_path.display()
        ))
    })?;
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .map_err(|error| NetworkEndpointError(format!("set Mycelium read timeout: {error}")))?;
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .map_err(|error| NetworkEndpointError(format!("set Mycelium write timeout: {error}")))?;
    stream
        .write_all(b"{\"method\":\"services\"}\n")
        .map_err(|error| NetworkEndpointError(format!("request Mycelium services: {error}")))?;
    stream
        .flush()
        .map_err(|error| NetworkEndpointError(format!("flush Mycelium request: {error}")))?;
    let mut line = String::new();
    BufReader::new(stream)
        .read_line(&mut line)
        .map_err(|error| NetworkEndpointError(format!("read Mycelium services: {error}")))?;
    let response: ServiceRpcResponse = serde_json::from_str(&line)
        .map_err(|error| NetworkEndpointError(format!("decode Mycelium services: {error}")))?;
    if !response.ok {
        return Err(NetworkEndpointError(format!(
            "Mycelium services failed: {}",
            response.error.unwrap_or_else(|| "unknown error".into())
        )));
    }
    response
        .result
        .ok_or_else(|| NetworkEndpointError("Mycelium services response has no result".into()))
}

#[cfg(not(unix))]
fn request_topology(_socket_path: &std::path::Path) -> Result<Topology, PeerError> {
    Err(PeerError(
        "Mycelium Unix-socket adapter is unavailable on this platform".into(),
    ))
}

#[cfg(not(unix))]
fn request_services(
    _socket_path: &std::path::Path,
) -> Result<ServiceCatalog, NetworkEndpointError> {
    Err(NetworkEndpointError(
        "Mycelium Unix-socket adapter is unavailable on this platform".into(),
    ))
}

fn usable_unicast(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => {
            !address.is_unspecified()
                && !address.is_loopback()
                && !address.is_multicast()
                && !address.is_broadcast()
        }
        IpAddr::V6(address) => {
            !address.is_unspecified()
                && !address.is_loopback()
                && !address.is_multicast()
                && !is_unicast_link_local(address)
        }
    }
}

fn is_unicast_link_local(address: Ipv6Addr) -> bool {
    (address.segments()[0] & 0xffc0) == 0xfe80
}

fn endpoint_priority(address: IpAddr) -> u16 {
    match address {
        IpAddr::V4(address) if address.is_private() => 10,
        IpAddr::V6(address) if !is_unique_local(address) => 20,
        IpAddr::V6(_) => 30,
        IpAddr::V4(_) => 40,
    }
}

fn is_unique_local(address: Ipv6Addr) -> bool {
    (address.segments()[0] & 0xfe00) == 0xfc00
}

#[cfg(test)]
mod tests {
    use super::*;

    fn topology() -> Topology {
        serde_json::from_value(serde_json::json!({
            "nodes": {
                "peer-key": {
                    "id": "peer-key",
                    "ips": {
                        "127.0.0.1": {},
                        "192.168.10.82": {},
                        "100.120.101.5": {},
                        "fe80::1": {}
                    },
                    "hostnames": ["agora-one"],
                    "sites": ["wagar-house"],
                    "annotation": {"name": null}
                }
            }
        }))
        .unwrap()
    }

    #[test]
    fn projects_fresh_validated_candidate_without_loopback_or_link_local() {
        let directory = MyceliumDirectory::new(
            "/tmp/myceliumd.sock".into(),
            18792,
            "mesh.fpl.dev",
            Duration::from_secs(15),
        )
        .unwrap();
        let candidate = directory
            .candidate_from_topology(topology(), "agora-one", 100)
            .unwrap();
        assert_eq!(candidate.site.as_deref(), Some("wagar-house"));
        assert_eq!(candidate.expires_at_unix_s, 115);
        assert_eq!(candidate.endpoints.len(), 2);
        assert_eq!(candidate.endpoints[0].host, "192.168.10.82");
        assert!(
            candidate
                .endpoints
                .iter()
                .all(|endpoint| endpoint.server_name.as_deref() == Some("agora-one.mesh.fpl.dev"))
        );
    }

    #[test]
    fn unknown_peer_fails_loudly() {
        let directory = MyceliumDirectory::new(
            "/tmp/myceliumd.sock".into(),
            18792,
            "mesh.fpl.dev",
            Duration::from_secs(15),
        )
        .unwrap();
        assert!(
            directory
                .candidate_from_topology(topology(), "missing", 100)
                .is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn resolves_through_public_mycelium_rpc_boundary() {
        use std::io::{BufRead, BufReader, Write};
        use std::os::unix::net::UnixListener;
        use std::time::{SystemTime, UNIX_EPOCH};

        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let socket = std::env::temp_dir().join(format!(
            "unibus-mycelium-test-{}-{nonce}.sock",
            std::process::id()
        ));
        let listener = UnixListener::bind(&socket).unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = String::new();
            BufReader::new(stream.try_clone().unwrap())
                .read_line(&mut request)
                .unwrap();
            assert_eq!(request, "{\"method\":\"topology\"}\n");
            let response = serde_json::json!({"ok": true, "result": {
                "nodes": {"peer-key": {
                    "id": "peer-key",
                    "ips": {"192.168.10.82": {}},
                    "hostnames": ["agora-one"],
                    "sites": ["wagar-house"],
                    "annotation": {"name": null}
                }}
            }});
            writeln!(stream, "{response}").unwrap();
        });

        let directory = MyceliumDirectory::new(
            socket.clone(),
            18792,
            "mesh.fpl.dev",
            Duration::from_secs(15),
        )
        .unwrap();
        let candidate = directory.resolve("agora-one", 100).unwrap();
        assert_eq!(candidate.endpoints[0].host, "192.168.10.82");
        server.join().unwrap();
        std::fs::remove_file(socket).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn imports_existing_mycelium_service_catalog_without_rescanning() {
        use std::io::{BufRead, BufReader, Write};
        use std::os::unix::net::UnixListener;
        use std::time::{SystemTime, UNIX_EPOCH};

        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let socket = std::env::temp_dir().join(format!("ums-{}-{nonce}.sock", std::process::id()));
        let listener = UnixListener::bind(&socket).unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = String::new();
            BufReader::new(stream.try_clone().unwrap())
                .read_line(&mut request)
                .unwrap();
            assert_eq!(request, "{\"method\":\"services\"}\n");
            let response = serde_json::json!({"ok":true,"result":{
                "schema_version":1,
                "observations":[{
                    "schema_version":1,
                    "service_id":"service/studio/mcp/tcp/8000",
                    "observed_at":100,
                    "expires_at":200,
                    "provenance":{"provider":"mycelium-topology","observer":"studio","source_id":"tcp:8000/mcp"},
                    "confidence":"strong",
                    "value":{"kind":"mcp","endpoints":["tcp://192.168.1.2:8000"],"protocols":["tcp"],"formats":[],"attributes":{"node_id":"studio"}}
                }]
            }});
            writeln!(stream, "{response}").unwrap();
        });

        let directory = MyceliumServiceDirectory::new(socket.clone()).unwrap();
        let services = directory.services("mcp").unwrap();
        assert_eq!(services.len(), 1);
        assert_eq!(services[0].provenance_provider, "mycelium-topology");
        assert_eq!(services[0].attributes["node_id"], "studio");
        server.join().unwrap();
        std::fs::remove_file(socket).unwrap();
    }
}
