//! Provider-neutral router and peer endpoint discovery.
//!
//! Unibus consumes endpoint facts. It does not care whether they came from a
//! static file, DNS, mDNS, Mycelium, or another inventory system. Discovery
//! nominates a reachable address; authentication still decides whether the
//! endpoint is trusted.

use std::collections::BTreeMap;
use std::fmt;
use std::net::{IpAddr, SocketAddr};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Transport {
    Quic,
    TcpTls,
    Relay,
}

/// A connectable router or peer endpoint. `host` may be an IP literal or a
/// DNS name; callers resolve it at the point of use so DNS TTL remains the
/// source of truth.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NetworkEndpoint {
    pub transport: Transport,
    pub host: String,
    pub port: u16,
    /// TLS/QUIC name to verify. Required when `host` is an IP literal.
    #[serde(default)]
    pub server_name: Option<String>,
    /// DNS-SRV-compatible ordering: lower values are preferred.
    #[serde(default)]
    pub priority: u16,
    /// DNS-SRV-compatible relative weight among equal priorities.
    #[serde(default)]
    pub weight: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetworkEndpointError(pub String);

impl fmt::Display for NetworkEndpointError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for NetworkEndpointError {}

impl NetworkEndpoint {
    pub fn validate(&self) -> Result<(), NetworkEndpointError> {
        if self.host.is_empty() || self.host.len() > 253 || self.port == 0 {
            return Err(NetworkEndpointError("invalid endpoint host or port".into()));
        }
        if !valid_host(&self.host) {
            return Err(NetworkEndpointError("invalid endpoint host".into()));
        }
        if self.host.parse::<IpAddr>().is_ok()
            && self
                .server_name
                .as_deref()
                .is_none_or(|name| !valid_dns_name(name))
        {
            return Err(NetworkEndpointError(
                "IP endpoint requires a valid server_name".into(),
            ));
        }
        if self
            .server_name
            .as_deref()
            .is_some_and(|name| !valid_dns_name(name))
        {
            return Err(NetworkEndpointError("invalid server_name".into()));
        }
        Ok(())
    }

    pub fn socket_addr(&self) -> Option<SocketAddr> {
        self.host
            .parse::<IpAddr>()
            .ok()
            .map(|ip| SocketAddr::new(ip, self.port))
    }
}

/// Narrow discovery seam. Implementations may cache according to their own
/// source TTL, but must return only validated endpoint facts.
pub trait EndpointSource: Send + Sync {
    fn endpoints(
        &self,
        installation_id: &str,
    ) -> Result<Vec<NetworkEndpoint>, NetworkEndpointError>;
}

/// A provider-neutral service fact discovered by inventory, DNS, a static
/// deployment, or another observer. Discovery conveys reachability evidence;
/// the protocol adapter still authenticates and validates the endpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceObservation {
    pub service_id: String,
    pub kind: String,
    pub endpoints: Vec<String>,
    pub observed_at_unix_s: u64,
    pub expires_at_unix_s: u64,
    pub provenance_provider: String,
    pub provenance_observer: String,
    #[serde(default)]
    pub attributes: BTreeMap<String, String>,
}

impl ServiceObservation {
    pub fn validate(&self) -> Result<(), NetworkEndpointError> {
        if !valid_service_name(&self.service_id)
            || !valid_service_name(&self.kind)
            || self.endpoints.is_empty()
            || self.endpoints.len() > 32
            || self.observed_at_unix_s >= self.expires_at_unix_s
            || self.expires_at_unix_s - self.observed_at_unix_s > 24 * 60 * 60
            || self.provenance_provider.trim().is_empty()
            || self.provenance_observer.trim().is_empty()
        {
            return Err(NetworkEndpointError("invalid service observation".into()));
        }
        if self.endpoints.iter().any(|endpoint| {
            endpoint.len() > 2048
                || !matches!(
                    endpoint.split_once("://").map(|(scheme, _)| scheme),
                    Some("http" | "https" | "tcp" | "quic")
                )
        }) {
            return Err(NetworkEndpointError("invalid service endpoint".into()));
        }
        Ok(())
    }

    pub fn is_fresh_at(&self, now_unix_s: u64) -> bool {
        self.observed_at_unix_s <= now_unix_s && now_unix_s < self.expires_at_unix_s
    }
}

/// Narrow service-discovery seam. Mycelium, static configuration, DNS, or a
/// future standalone Unibus directory can implement this contract.
pub trait ServiceDirectory: Send + Sync {
    fn services(&self, kind: &str) -> Result<Vec<ServiceObservation>, NetworkEndpointError>;
}

#[derive(Debug, Clone)]
pub struct StaticServiceDirectory {
    services: Vec<ServiceObservation>,
}

impl StaticServiceDirectory {
    pub fn new(mut services: Vec<ServiceObservation>) -> Result<Self, NetworkEndpointError> {
        for service in &services {
            service.validate()?;
        }
        services.sort_by(|left, right| left.service_id.cmp(&right.service_id));
        services.dedup_by(|left, right| left.service_id == right.service_id);
        Ok(Self { services })
    }
}

impl ServiceDirectory for StaticServiceDirectory {
    fn services(&self, kind: &str) -> Result<Vec<ServiceObservation>, NetworkEndpointError> {
        if !valid_service_name(kind) {
            return Err(NetworkEndpointError("invalid service kind".into()));
        }
        Ok(self
            .services
            .iter()
            .filter(|service| service.kind == kind)
            .cloned()
            .collect())
    }
}

#[derive(Debug, Clone)]
pub struct StaticEndpointSource {
    endpoints: Vec<NetworkEndpoint>,
}

impl StaticEndpointSource {
    pub fn new(mut endpoints: Vec<NetworkEndpoint>) -> Result<Self, NetworkEndpointError> {
        for endpoint in &endpoints {
            endpoint.validate()?;
        }
        endpoints.sort_by_key(|endpoint| {
            (
                endpoint.priority,
                endpoint.transport,
                endpoint.host.clone(),
                endpoint.port,
            )
        });
        endpoints.dedup();
        Ok(Self { endpoints })
    }
}

impl EndpointSource for StaticEndpointSource {
    fn endpoints(
        &self,
        installation_id: &str,
    ) -> Result<Vec<NetworkEndpoint>, NetworkEndpointError> {
        if installation_id.trim().is_empty() {
            return Err(NetworkEndpointError("installation_id is empty".into()));
        }
        Ok(self.endpoints.clone())
    }
}

/// Conventional SRV owner for QUIC routers. A DNS-backed endpoint source may
/// resolve this name and map SRV priority/weight into `NetworkEndpoint`.
pub fn quic_srv_name(installation_id: &str, domain: &str) -> Result<String, NetworkEndpointError> {
    if !valid_label(installation_id) || !valid_dns_name(domain) {
        return Err(NetworkEndpointError(
            "invalid installation_id or discovery domain".into(),
        ));
    }
    Ok(format!("_unibus._udp.{installation_id}.{domain}"))
}

fn valid_host(value: &str) -> bool {
    value.parse::<IpAddr>().is_ok() || valid_dns_name(value)
}

fn valid_dns_name(value: &str) -> bool {
    let value = value.strip_suffix('.').unwrap_or(value);
    !value.is_empty()
        && value.len() <= 253
        && value.split('.').all(valid_label)
        && value.contains('.')
}

fn valid_label(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 63
        && !value.starts_with('-')
        && !value.ends_with('-')
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

fn valid_service_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':' | b'/')
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn static_source_validates_orders_and_deduplicates() {
        let secondary = NetworkEndpoint {
            transport: Transport::Quic,
            host: "router-b.home.arpa".into(),
            port: 18791,
            server_name: None,
            priority: 20,
            weight: 0,
        };
        let primary = NetworkEndpoint {
            priority: 10,
            host: "router-a.home.arpa".into(),
            ..secondary.clone()
        };
        let source = StaticEndpointSource::new(vec![secondary, primary.clone(), primary]).unwrap();
        let endpoints = source.endpoints("wagar-house").unwrap();
        assert_eq!(endpoints.len(), 2);
        assert_eq!(endpoints[0].host, "router-a.home.arpa");
    }

    #[test]
    fn ip_endpoint_requires_authenticated_server_name() {
        let endpoint = NetworkEndpoint {
            transport: Transport::Quic,
            host: "192.168.10.82".into(),
            port: 18791,
            server_name: None,
            priority: 0,
            weight: 0,
        };
        assert!(endpoint.validate().is_err());
    }

    #[test]
    fn dns_contract_is_stable_and_bounded() {
        assert_eq!(
            quic_srv_name("wagar-house", "mesh.fpl.dev").unwrap(),
            "_unibus._udp.wagar-house.mesh.fpl.dev"
        );
        assert!(quic_srv_name("bad/site", "mesh.fpl.dev").is_err());
    }

    #[test]
    fn static_service_directory_preserves_provenance_and_filters_kind() {
        let observation = ServiceObservation {
            service_id: "service/studio/mcp/http/8000".into(),
            kind: "mcp".into(),
            endpoints: vec!["http://127.0.0.1:8000/mcp".into()],
            observed_at_unix_s: 100,
            expires_at_unix_s: 200,
            provenance_provider: "static".into(),
            provenance_observer: "studio".into(),
            attributes: BTreeMap::from([("node_id".into(), "studio".into())]),
        };
        let directory = StaticServiceDirectory::new(vec![observation.clone()]).unwrap();
        assert_eq!(directory.services("mcp").unwrap(), [observation]);
        assert!(directory.services("dcp").unwrap().is_empty());
        assert!(directory.services("bad kind").is_err());
    }
}
