//! Direct peer selection and transport boundaries.
//!
//! These interfaces deliberately contain no Mycelium types. A deployment may
//! resolve peers through static configuration, DNS, Mycelium, or another
//! provider, then carry authorized Unibus frames through any suitable direct
//! transport. Discovery proves reachability, never authority.

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::{Envelope, NetworkEndpoint};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PeerCandidate {
    pub peer_id: String,
    #[serde(default)]
    pub site: Option<String>,
    pub endpoints: Vec<NetworkEndpoint>,
    /// Provider freshness bound. A directory must not return this candidate
    /// after the timestamp without refreshing its source.
    pub expires_at_unix_s: u64,
}

impl PeerCandidate {
    pub fn validate(&self, now_unix_s: u64) -> Result<(), PeerError> {
        if self.peer_id.trim().is_empty() || self.peer_id.len() > 128 {
            return Err(PeerError("invalid peer id".into()));
        }
        if self.endpoints.is_empty() || self.endpoints.len() > 32 {
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
        Ok(())
    }
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
    fn send(&mut self, envelope: &Envelope, payload: &[u8]) -> Result<(), PeerError>;
    fn receive(&mut self) -> Result<(Envelope, Vec<u8>), PeerError>;
}

pub trait PeerTransport: Send + Sync {
    fn connect(
        &self,
        candidate: &PeerCandidate,
        session_assertion: &str,
    ) -> Result<Box<dyn PeerSession>, PeerError>;
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
}
