//! Node authentication shape, aligned with fpl-auth (sso.fpl.dev) as the
//! identity root. Nodes enroll like fpl-auth desktop bridges — per-user
//! devices with revocable tokens — and present short-lived RS256 JWTs that
//! any holder of the issuer's pinned JWKS can validate offline, including a
//! peer in a direct session with no hub round-trip.
//!
//! This module defines the claim contract and time/identity validation.
//! Signature verification against fpl-auth's JWKS lives with the integrator
//! (see the `OfflineVerifier` seam and marbles `dock-65hv`); nothing here
//! claims a token is authentic — only that its *claims* are well-formed and
//! fresh.

use std::fmt;

use serde::{Deserialize, Serialize};

/// Clock skew tolerance, in seconds.
pub const MAX_SKEW_SECS: u64 = 5;
/// Assertions are minted short; expiry is the revocation window.
pub const MAX_ASSERTION_TTL_SECS: u64 = 300;

/// Claims a unibus node must present. Custom claims carry the grant scope so
/// verifiers can enforce it without consulting the hub.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeClaims {
    pub iss: String,
    pub sub: String,
    pub aud: String,
    pub iat: u64,
    pub exp: u64,
    pub jti: String,
    pub unibus_installation_id: String,
    pub unibus_producer_id: String,
    #[serde(default)]
    pub unibus_zones: Vec<String>,
    #[serde(default)]
    pub unibus_capabilities: Vec<String>,
    #[serde(default)]
    pub unibus_screen_anchors: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthError {
    Expired { now: u64, exp: u64 },
    NotYetValid { now: u64, iat: u64 },
    TooLongLived { ttl: u64 },
    AudienceMismatch { expected: String, got: String },
    InstallationMismatch { expected: String, got: String },
    EmptyClaim(&'static str),
    Malformed,
}

impl fmt::Display for AuthError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Expired { now, exp } => write!(f, "assertion expired at {exp}, now {now}"),
            Self::NotYetValid { now, iat } => {
                write!(f, "assertion issued in future: iat {iat}, now {now}")
            }
            Self::TooLongLived { ttl } => {
                write!(f, "assertion ttl {ttl}s exceeds the revocation window")
            }
            Self::AudienceMismatch { expected, got } => {
                write!(f, "audience mismatch: expected {expected}, got {got}")
            }
            Self::InstallationMismatch { expected, got } => {
                write!(f, "installation mismatch: expected {expected}, got {got}")
            }
            Self::EmptyClaim(name) => write!(f, "required claim is empty: {name}"),
            Self::Malformed => write!(f, "malformed assertion"),
        }
    }
}

impl std::error::Error for AuthError {}

impl NodeClaims {
    /// Structure and freshness checks against an injected clock. This is the
    /// *offline* half of offline validation; the signature half belongs to
    /// the `OfflineVerifier` implementation.
    pub fn validate(
        &self,
        expected_audience: &str,
        expected_installation: &str,
        now_unix_s: u64,
    ) -> Result<(), AuthError> {
        for (name, value) in [
            ("iss", &self.iss),
            ("sub", &self.sub),
            ("jti", &self.jti),
            ("unibus_producer_id", &self.unibus_producer_id),
        ] {
            if value.trim().is_empty() {
                return Err(AuthError::EmptyClaim(name));
            }
        }
        if self.aud != expected_audience {
            return Err(AuthError::AudienceMismatch {
                expected: expected_audience.to_owned(),
                got: self.aud.clone(),
            });
        }
        if self.unibus_installation_id != expected_installation {
            return Err(AuthError::InstallationMismatch {
                expected: expected_installation.to_owned(),
                got: self.unibus_installation_id.clone(),
            });
        }
        if self.exp.saturating_add(MAX_SKEW_SECS) < now_unix_s {
            return Err(AuthError::Expired {
                now: now_unix_s,
                exp: self.exp,
            });
        }
        if self.iat > now_unix_s + MAX_SKEW_SECS {
            return Err(AuthError::NotYetValid {
                now: now_unix_s,
                iat: self.iat,
            });
        }
        let ttl = self.exp.saturating_sub(self.iat);
        if ttl > MAX_ASSERTION_TTL_SECS {
            return Err(AuthError::TooLongLived { ttl });
        }
        Ok(())
    }

    /// The grant scope this assertion is allowed to exercise.
    pub fn scopes(&self) -> Scope<'_> {
        Scope {
            zones: &self.unibus_zones,
            capabilities: &self.unibus_capabilities,
            screen_anchors: &self.unibus_screen_anchors,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Scope<'a> {
    pub zones: &'a [String],
    pub capabilities: &'a [String],
    pub screen_anchors: &'a [String],
}

/// The seam an integrator implements: parse a compact JWS, verify the
/// signature against pinned/cached fpl-auth JWKS (RS256), then hand the
/// decoded claims to `NodeClaims::validate`. Implementations must reject
/// unknown critical extensions and must not fall back to `alg: none`.
pub trait OfflineVerifier {
    fn verify(&self, assertion: &str, now_unix_s: u64) -> Result<NodeClaims, AuthError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claims(now: u64) -> NodeClaims {
        NodeClaims {
            iss: "https://sso.fpl.dev".into(),
            sub: "avery/home-pi".into(),
            aud: "unibus:home".into(),
            iat: now,
            exp: now + 60,
            jti: "assertion-1".into(),
            unibus_installation_id: "home".into(),
            unibus_producer_id: "home-pi-tv".into(),
            unibus_zones: vec!["living-room".into()],
            unibus_capabilities: vec!["canvas.display".into()],
            unibus_screen_anchors: vec!["screen-tv".into()],
        }
    }

    #[test]
    fn fresh_claims_validate() {
        let now = 1_700_000_000;
        assert_eq!(claims(now).validate("unibus:home", "home", now), Ok(()));
    }

    #[test]
    fn expiry_audience_and_installation_fail() {
        let now = 1_700_000_000;
        let c = claims(now);
        assert!(matches!(
            c.validate("unibus:home", "home", now + 120),
            Err(AuthError::Expired { .. })
        ));
        assert!(matches!(
            c.validate("unibus:other", "home", now),
            Err(AuthError::AudienceMismatch { .. })
        ));
        assert!(matches!(
            c.validate("unibus:home", "office", now),
            Err(AuthError::InstallationMismatch { .. })
        ));
    }

    #[test]
    fn long_lived_and_future_are_rejected() {
        let now = 1_700_000_000;
        let mut c = claims(now);
        c.exp = now + 400;
        assert!(matches!(
            c.validate("unibus:home", "home", now),
            Err(AuthError::TooLongLived { .. })
        ));
        let c = claims(now + 60);
        assert!(matches!(
            c.validate("unibus:home", "home", now),
            Err(AuthError::NotYetValid { .. })
        ));
    }

    #[test]
    fn claims_round_trip_camel_free() {
        let c = claims(1_700_000_000);
        let json = serde_json::to_string(&c).unwrap();
        assert!(json.contains("unibus_producer_id"));
        let parsed: NodeClaims = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, c);
        assert_eq!(parsed.scopes().screen_anchors, ["screen-tv".to_string()]);
    }
}
