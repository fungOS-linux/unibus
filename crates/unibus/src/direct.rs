//! Hub-authorized, peer-to-peer session contract.
//!
//! A router may sign these claims after authenticating both peers and checking
//! their grants. Peers validate the signature through an injected verifier,
//! validate these bounded claims, and then connect directly. The router is not
//! required on the data path after authorization.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::discovery::NetworkEndpoint;
use crate::grants::is_valid_namespaced_name;

pub const DIRECT_SESSION_SCHEMA_V1: u32 = 1;
pub const MAX_DIRECT_SESSION_TTL_SECS: u64 = 300;
pub const MAX_DIRECT_KINDS: usize = 32;
pub const MAX_DIRECT_CANDIDATES: usize = 16;
pub const DIRECT_AUTH_KIND: &str = "unibus.direct.authenticate.v1";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PeerBinding {
    pub producer_id: String,
    /// Stable digest or encoded public key for the ephemeral/direct endpoint.
    pub key_binding: String,
    #[serde(default)]
    pub candidates: Vec<NetworkEndpoint>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DirectSessionClaims {
    pub schema_version: u32,
    pub iss: String,
    pub aud: String,
    pub jti: String,
    pub installation_id: String,
    pub iat: u64,
    pub exp: u64,
    pub initiator: PeerBinding,
    pub responder: PeerBinding,
    pub kinds: Vec<String>,
    pub max_payload_bytes: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirectSessionError(pub String);

impl fmt::Display for DirectSessionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for DirectSessionError {}

/// Signature boundary for a compact, router-issued direct-session assertion.
/// Implementations own the signing format and trust roots; this crate owns the
/// claims and their validation rules.
pub trait DirectSessionVerifier: Send + Sync {
    fn verify(
        &self,
        assertion: &str,
        now_unix_s: u64,
    ) -> Result<DirectSessionClaims, DirectSessionError>;
}

impl DirectSessionClaims {
    pub fn validate(
        &self,
        expected_audience: &str,
        expected_installation: &str,
        local_producer: &str,
        now_unix_s: u64,
    ) -> Result<(), DirectSessionError> {
        if self.schema_version != DIRECT_SESSION_SCHEMA_V1 {
            return Err(DirectSessionError(
                "unsupported direct-session schema".into(),
            ));
        }
        if self.aud != expected_audience || self.installation_id != expected_installation {
            return Err(DirectSessionError(
                "direct-session audience or installation mismatch".into(),
            ));
        }
        if self.iss.trim().is_empty() || self.jti.trim().is_empty() {
            return Err(DirectSessionError("missing issuer or session id".into()));
        }
        if self.exp <= self.iat
            || self.exp.saturating_sub(self.iat) > MAX_DIRECT_SESSION_TTL_SECS
            || now_unix_s < self.iat.saturating_sub(5)
            || now_unix_s > self.exp.saturating_add(5)
        {
            return Err(DirectSessionError(
                "invalid or expired direct-session lifetime".into(),
            ));
        }
        if local_producer != self.initiator.producer_id
            && local_producer != self.responder.producer_id
        {
            return Err(DirectSessionError(
                "local producer is not a session participant".into(),
            ));
        }
        if self.initiator.producer_id == self.responder.producer_id {
            return Err(DirectSessionError(
                "direct session requires two distinct peers".into(),
            ));
        }
        validate_peer(&self.initiator)?;
        validate_peer(&self.responder)?;
        if self.kinds.is_empty()
            || self.kinds.len() > MAX_DIRECT_KINDS
            || self
                .kinds
                .iter()
                .any(|kind| !is_valid_namespaced_name(kind))
        {
            return Err(DirectSessionError("invalid direct-session kinds".into()));
        }
        if self.max_payload_bytes == 0 || self.max_payload_bytes > crate::MAX_FRAME {
            return Err(DirectSessionError(
                "invalid direct-session payload bound".into(),
            ));
        }
        Ok(())
    }

    pub fn peer_for(&self, local_producer: &str) -> Option<&PeerBinding> {
        if local_producer == self.initiator.producer_id {
            Some(&self.responder)
        } else if local_producer == self.responder.producer_id {
            Some(&self.initiator)
        } else {
            None
        }
    }

    /// Signature and peer-key verification must precede these per-frame
    /// checks. Established sessions expire strictly, without handshake skew.
    pub fn authorize_frame(
        &self,
        envelope: &crate::Envelope,
        payload_len: usize,
        expected_producer: &str,
        now_unix_ms: u64,
    ) -> Result<(), DirectSessionError> {
        let remote = self
            .peer_for(expected_producer)
            .ok_or_else(|| DirectSessionError("producer is not a session participant".into()))?;
        if now_unix_ms / 1000 >= self.exp
            || now_unix_ms / 1000 < self.iat.saturating_sub(5)
            || envelope.installation_id != self.installation_id
            || envelope.producer_id != expected_producer
            || envelope
                .destination
                .as_deref()
                .is_some_and(|id| id != remote.producer_id)
            || !self.kinds.contains(&envelope.message.kind)
            || payload_len > self.max_payload_bytes as usize
            || payload_len != envelope.payload_bytes
            || !envelope.token.is_empty()
        {
            return Err(DirectSessionError(
                "frame exceeds direct-session lifetime, identity or scope".into(),
            ));
        }
        envelope
            .validate(&self.installation_id, now_unix_ms)
            .map_err(|error| DirectSessionError(error.to_string()))
    }
}

fn validate_peer(peer: &PeerBinding) -> Result<(), DirectSessionError> {
    if peer.producer_id.trim().is_empty()
        || peer.producer_id.len() > 128
        || peer.key_binding.len() < 16
        || peer.key_binding.len() > 512
        || peer.candidates.len() > MAX_DIRECT_CANDIDATES
    {
        return Err(DirectSessionError("invalid direct-session peer".into()));
    }
    for candidate in &peer.candidates {
        candidate
            .validate()
            .map_err(|error| DirectSessionError(error.to_string()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discovery::Transport;

    fn claims() -> DirectSessionClaims {
        let peer = |producer_id: &str, host: &str| PeerBinding {
            producer_id: producer_id.into(),
            key_binding: format!("sha256:{producer_id}-ephemeral-key"),
            candidates: vec![NetworkEndpoint {
                transport: Transport::Quic,
                host: host.into(),
                port: 18792,
                server_name: None,
                priority: 0,
                weight: 0,
            }],
        };
        DirectSessionClaims {
            schema_version: DIRECT_SESSION_SCHEMA_V1,
            iss: "router.home.arpa".into(),
            aud: "unibus:direct:wagar-house".into(),
            jti: "session-1".into(),
            installation_id: "wagar-house".into(),
            iat: 1_000,
            exp: 1_120,
            initiator: peer("neo-midi", "neo.home.arpa"),
            responder: peer("home-pi-midi", "home-pi.home.arpa"),
            kinds: vec!["midi.packet.v1".into()],
            max_payload_bytes: 4096,
        }
    }

    #[test]
    fn valid_claims_bind_both_peers_and_scope() {
        let claims = claims();
        claims
            .validate(
                "unibus:direct:wagar-house",
                "wagar-house",
                "neo-midi",
                1_060,
            )
            .unwrap();
        assert_eq!(
            claims.peer_for("neo-midi").unwrap().producer_id,
            "home-pi-midi"
        );
    }

    #[test]
    fn frame_guard_rechecks_expiry_recipient_source_and_payload() {
        let claims = claims();
        let mut frame = crate::Envelope::new(
            "wagar-house",
            "neo-midi",
            Some("home-pi-midi"),
            "",
            crate::Message::new("midi.packet.v1", serde_json::json!({})),
        );
        frame.issued_at_unix_ms = 1_060_000;
        assert!(
            claims
                .authorize_frame(&frame, 0, "neo-midi", 1_060_000)
                .is_ok()
        );
        assert!(
            claims
                .authorize_frame(&frame, 0, "neo-midi", 1_120_000)
                .is_err()
        );
        assert!(
            claims
                .authorize_frame(&frame, 1, "neo-midi", 1_060_000)
                .is_err()
        );
        assert!(
            claims
                .authorize_frame(&frame, 0, "intruder", 1_060_000)
                .is_err()
        );
        frame.destination = Some("intruder".into());
        assert!(
            claims
                .authorize_frame(&frame, 0, "neo-midi", 1_060_000)
                .is_err()
        );
    }

    #[test]
    fn outsider_expiry_and_unscoped_kinds_fail_closed() {
        let claims = claims();
        assert!(
            claims
                .validate(
                    "unibus:direct:wagar-house",
                    "wagar-house",
                    "intruder",
                    1_060,
                )
                .is_err()
        );
        assert!(
            claims
                .validate(
                    "unibus:direct:wagar-house",
                    "wagar-house",
                    "neo-midi",
                    2_000,
                )
                .is_err()
        );
        let mut unscoped = claims;
        unscoped.kinds = vec!["midi".into()];
        assert!(
            unscoped
                .validate(
                    "unibus:direct:wagar-house",
                    "wagar-house",
                    "neo-midi",
                    1_060,
                )
                .is_err()
        );
    }
}
