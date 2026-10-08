//! Direct peer selection and transport boundaries.
//!
//! These interfaces deliberately contain no Mycelium types. A deployment may
//! resolve peers through static configuration, DNS, Mycelium, or another
//! provider, then carry authorized Unibus frames through any suitable direct
//! transport. Discovery proves reachability, never authority.

use std::collections::BTreeMap;
use std::fmt;
use std::future::Future;
use std::pin::Pin;

use serde::{Deserialize, Serialize};

use crate::{Envelope, NetworkEndpoint};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PeerCandidate {
    pub peer_id: String,
    #[serde(default)]
    pub site: Option<String>,
    pub endpoints: Vec<NetworkEndpoint>,
    /// Optional key-addressed transports; old IP-only candidates remain valid.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub iroh_endpoints: Vec<IrohPeerEndpoint>,
    /// Provider freshness bound. A directory must not return this candidate
    /// after the timestamp without refreshing its source.
    pub expires_at_unix_s: u64,
}

impl PeerCandidate {
    pub fn validate(&self, now_unix_s: u64) -> Result<(), PeerError> {
        if self.peer_id.trim().is_empty() || self.peer_id.len() > 128 {
            return Err(PeerError("invalid peer id".into()));
        }
        if self.endpoints.len() + self.iroh_endpoints.len() == 0
            || self.endpoints.len() + self.iroh_endpoints.len() > 32
        {
            return Err(PeerError("peer requires 1..=32 endpoints".into()));
        }
        if self.expires_at_unix_s <= now_unix_s {
            return Err(PeerError("peer candidate is stale".into()));
        }
        for endpoint in &self.endpoints {
            endpoint
                .validate()
                .map_err(|error| PeerError(error.to_string()))?;
        }
        for endpoint in &self.iroh_endpoints {
            endpoint.validate()?;
        }
        Ok(())
    }
}

/// SDK-neutral public connection hints. These never constitute a grant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IrohPeerEndpoint {
    pub endpoint_id: String,
    #[serde(default)]
    pub direct_addresses: Vec<std::net::SocketAddr>,
    #[serde(default)]
    pub relay_urls: Vec<String>,
}

impl IrohPeerEndpoint {
    pub fn validate(&self) -> Result<(), PeerError> {
        if self.endpoint_id.len() != 64
            || !self
                .endpoint_id
                .bytes()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
            || self.endpoint_id.bytes().all(|c| c == b'0')
            || self.direct_addresses.len() > 16
            || self.relay_urls.len() > 4
            || self.direct_addresses.is_empty() && self.relay_urls.is_empty()
            || self
                .direct_addresses
                .iter()
                .any(|a| a.port() == 0 || a.ip().is_unspecified() || a.ip().is_multicast())
            || self.relay_urls.iter().any(|url| {
                url.len() > 2048
                    || !url.starts_with("https://")
                    || url.chars().any(char::is_control)
            })
        {
            return Err(PeerError("invalid Iroh endpoint hints".into()));
        }
        Ok(())
    }
}

/// Optional additional local policy. It may restrict, never replace, the
/// transport's signed-session and peer-key checks. Implementations must fail
/// closed and avoid blocking the executor (e.g. offload local authority IPC).
pub trait PeerAccessPolicy: Send + Sync {
    fn authorize<'a>(
        &'a self,
        source: &'a str,
        destination: &'a str,
        kind: &'a str,
        now_unix_s: u64,
    ) -> PeerFuture<'a, ()>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerError(pub String);

impl fmt::Display for PeerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for PeerError {}

/// Resolve one already-authorized Unibus peer into fresh connection
/// candidates. Capability selection and grants remain Unibus policy concerns.
pub trait PeerDirectory: Send + Sync {
    fn resolve(&self, peer_id: &str, now_unix_s: u64) -> Result<PeerCandidate, PeerError>;
}

/// Message-oriented direct session. Implementations may use QUIC, a Mycelium
/// selected path, or another authenticated transport without changing Unibus
/// contracts.
pub trait PeerSession: Send {
    fn peer_id(&self) -> &str;
    fn send<'a>(&'a mut self, envelope: &'a Envelope, payload: &'a [u8]) -> PeerFuture<'a, ()>;
    fn receive(&mut self) -> PeerFuture<'_, (Envelope, Vec<u8>)>;
}

pub trait PeerTransport: Send + Sync {
    fn connect<'a>(
        &'a self,
        candidate: &'a PeerCandidate,
        session_assertion: &'a str,
    ) -> PeerFuture<'a, Box<dyn PeerSession>>;
}

pub type PeerFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, PeerError>> + Send + 'a>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionPath {
    Direct,
    Fallback,
}

/// Prefer a direct transport while making the downgrade decision explicit at
/// the call site. The fallback receives the direct failure for logging and
/// policy decisions; there is no silent security or transport downgrade.
pub async fn connect_direct_or_else<F, Fut>(
    direct: &dyn PeerTransport,
    candidate: &PeerCandidate,
    session_assertion: &str,
    fallback: F,
) -> Result<(SessionPath, Box<dyn PeerSession>), PeerError>
where
    F: FnOnce(&PeerError) -> Fut,
    Fut: Future<Output = Result<Box<dyn PeerSession>, PeerError>>,
{
    match direct.connect(candidate, session_assertion).await {
        Ok(session) => Ok((SessionPath::Direct, session)),
        Err(direct_error) => fallback(&direct_error)
            .await
            .map(|session| (SessionPath::Fallback, session))
            .map_err(|fallback_error| {
                PeerError(format!(
                    "direct connection failed: {direct_error}; fallback failed: {fallback_error}"
                ))
            }),
    }
}

#[derive(Debug, Clone, Default)]
pub struct StaticPeerDirectory {
    peers: BTreeMap<String, PeerCandidate>,
}

impl StaticPeerDirectory {
    pub fn new(peers: Vec<PeerCandidate>, now_unix_s: u64) -> Result<Self, PeerError> {
        let mut indexed = BTreeMap::new();
        for peer in peers {
            peer.validate(now_unix_s)?;
            let id = peer.peer_id.clone();
            if indexed.insert(id.clone(), peer).is_some() {
                return Err(PeerError(format!("duplicate peer id {id}")));
            }
        }
        Ok(Self { peers: indexed })
    }
}

impl PeerDirectory for StaticPeerDirectory {
    fn resolve(&self, peer_id: &str, now_unix_s: u64) -> Result<PeerCandidate, PeerError> {
        let candidate = self
            .peers
            .get(peer_id)
            .ok_or_else(|| PeerError(format!("unknown peer {peer_id}")))?
            .clone();
        candidate.validate(now_unix_s)?;
        Ok(candidate)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Transport;

    #[test]
    fn static_directory_fails_closed_for_unknown_and_stale_peers() {
        let candidate = PeerCandidate {
            peer_id: "home-pi".into(),
            site: Some("wagar-house".into()),
            iroh_endpoints: vec![],
            endpoints: vec![NetworkEndpoint {
                transport: Transport::Quic,
                host: "home-pi.mesh.fpl.dev".into(),
                port: 18792,
                server_name: None,
                priority: 0,
                weight: 0,
            }],
            expires_at_unix_s: 200,
        };
        let directory = StaticPeerDirectory::new(vec![candidate], 100).unwrap();
        assert_eq!(
            directory.resolve("home-pi", 100).unwrap().site.as_deref(),
            Some("wagar-house")
        );
        assert!(directory.resolve("unknown", 100).is_err());
        assert!(directory.resolve("home-pi", 200).is_err());
    }

    struct FailingTransport;

    impl PeerTransport for FailingTransport {
        fn connect<'a>(
            &'a self,
            _candidate: &'a PeerCandidate,
            _session_assertion: &'a str,
        ) -> PeerFuture<'a, Box<dyn PeerSession>> {
            Box::pin(async { Err(PeerError("direct unavailable".into())) })
        }
    }

    struct StubSession;

    impl PeerSession for StubSession {
        fn peer_id(&self) -> &str {
            "home-pi"
        }

        fn send<'a>(
            &'a mut self,
            _envelope: &'a Envelope,
            _payload: &'a [u8],
        ) -> PeerFuture<'a, ()> {
            Box::pin(async { Ok(()) })
        }

        fn receive(&mut self) -> PeerFuture<'_, (Envelope, Vec<u8>)> {
            Box::pin(async { Err(PeerError("stub has no frames".into())) })
        }
    }

    #[tokio::test]
    async fn fallback_is_explicit_and_receives_direct_failure() {
        let candidate = PeerCandidate {
            peer_id: "home-pi".into(),
            site: None,
            iroh_endpoints: vec![],
            endpoints: vec![NetworkEndpoint {
                transport: Transport::Quic,
                host: "home-pi.mesh.fpl.dev".into(),
                port: 18792,
                server_name: None,
                priority: 0,
                weight: 0,
            }],
            expires_at_unix_s: 200,
        };
        let (path, session) =
            connect_direct_or_else(&FailingTransport, &candidate, "assertion", |direct_error| {
                assert_eq!(direct_error.0, "direct unavailable");
                std::future::ready(Ok(Box::new(StubSession) as Box<dyn PeerSession>))
            })
            .await
            .unwrap();
        assert_eq!(path, SessionPath::Fallback);
        assert_eq!(session.peer_id(), "home-pi");
    }
}
