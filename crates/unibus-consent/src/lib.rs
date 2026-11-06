//! Lower an approved host consent decision into an ordinary Unibus node grant.
//!
//! Consent does not mint authority. It only decides whether the existing
//! Unibus issuer may create the `NodeGrant` the caller already previewed.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use soil_consent::{Decision, Effect, Preview, Request, Scope, client::Client};
use thiserror::Error;
use unibus::grants::{NodeGrant, Role};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeGrantSpec {
    pub producer_id: String,
    pub auth_subject: String,
    pub zones: Vec<String>,
    pub capabilities: Vec<String>,
    pub publishes: Vec<String>,
    pub publish_zones: Vec<String>,
    pub publish_screen_anchors: Vec<String>,
    pub publish_producers: Vec<String>,
    pub screen_anchors: Vec<String>,
    pub site: String,
    pub role: Role,
    /// Requested lifetime. Consent may shorten it; it can never lengthen it.
    pub lifetime_s: u64,
}

impl NodeGrantSpec {
    pub fn grant(&self) -> NodeGrant {
        NodeGrant {
            producer_id: self.producer_id.clone(),
            auth_subject: self.auth_subject.clone(),
            zones: self.zones.clone(),
            capabilities: self.capabilities.clone(),
            publishes: self.publishes.clone(),
            publish_zones: self.publish_zones.clone(),
            publish_screen_anchors: self.publish_screen_anchors.clone(),
            publish_producers: self.publish_producers.clone(),
            screen_anchors: self.screen_anchors.clone(),
            site: self.site.clone(),
            role: self.role,
        }
    }

    pub fn digest(&self) -> Result<String, Error> {
        let encoded = serde_json::to_vec(self).map_err(Error::Json)?;
        Ok(format!("sha256:{:x}", Sha256::digest(encoded)))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovedNodeGrant {
    pub grant: NodeGrant,
    pub expires_at_unix_s: u64,
    pub request_id: String,
}

#[derive(Debug, Error)]
pub enum Error {
    #[error("consent denied")]
    Denied,
    #[error("consent decision is for a different request")]
    WrongRequest,
    #[error("approved payload does not match the request digest")]
    PayloadMismatch,
    #[error("consent request is not a Unibus grant request")]
    WrongKind,
    #[error("lifetime is zero")]
    ZeroLifetime,
    #[error("grant is invalid: {0}")]
    InvalidGrant(String),
    #[error("consent client: {0}")]
    Consent(String),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
}

/// Ask the host consent popup and lower an approval in one call. The caller
/// should check for an already-valid grant before invoking this.
pub fn request_node_grant(
    client: &Client,
    request: &Request,
    spec: &NodeGrantSpec,
    now_unix_s: u64,
) -> Result<ApprovedNodeGrant, Error> {
    let decision = client
        .request(request)
        .map_err(|error| Error::Consent(error.to_string()))?;
    approved_node_grant(request, &decision, spec, now_unix_s)
}

/// Build the grant previewed in `request` only when `decision` allows the exact
/// same payload. The caller still owns installing it into Unibus policy/lease
/// state; this function never bypasses Unibus verification.
pub fn approved_node_grant(
    request: &Request,
    decision: &Decision,
    spec: &NodeGrantSpec,
    now_unix_s: u64,
) -> Result<ApprovedNodeGrant, Error> {
    if request.kind != soil_consent::Kind::UnibusGrant {
        return Err(Error::WrongKind);
    }
    if decision.request_id != request.id {
        return Err(Error::WrongRequest);
    }
    if !decision.allowed {
        return Err(Error::Denied);
    }
    if spec.digest()? != request.payload_digest {
        return Err(Error::PayloadMismatch);
    }
    if spec.lifetime_s == 0 {
        return Err(Error::ZeroLifetime);
    }
    if !request
        .effects
        .iter()
        .any(|effect| matches!(effect, Effect::External | Effect::Write))
    {
        return Err(Error::WrongKind);
    }
    if request.effects.contains(&Effect::Destructive)
        && !matches!(
            request.preview,
            Preview::Artifact { .. } | Preview::SporeScene { .. }
        )
    {
        return Err(Error::WrongKind);
    }
    let grant = spec.grant();
    grant
        .validate()
        .map_err(|error| Error::InvalidGrant(error.to_string()))?;

    let lifetime = match decision.scope {
        Scope::Once => 300,
        Scope::Session => spec.lifetime_s.min(86_400),
        Scope::Persistent => spec.lifetime_s,
        Scope::Until(until) => until.saturating_sub(now_unix_s).max(1),
    };
    Ok(ApprovedNodeGrant {
        grant,
        expires_at_unix_s: now_unix_s.saturating_add(lifetime),
        request_id: request.id.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use soil_consent::{Kind, Preview};

    fn spec() -> NodeGrantSpec {
        NodeGrantSpec {
            producer_id: "marbles".into(),
            auth_subject: "oidc:example#avery".into(),
            zones: vec!["home".into()],
            capabilities: vec!["marbles.issue.read".into()],
            publishes: vec!["marbles.issue.changed".into()],
            publish_zones: vec!["home".into()],
            publish_screen_anchors: vec![],
            publish_producers: vec![],
            screen_anchors: vec![],
            site: "home-lan".into(),
            role: Role::Origin,
            lifetime_s: 600,
        }
    }

    fn request(digest: String, effects: Vec<Effect>) -> Request {
        Request {
            id: "req-1".into(),
            kind: Kind::UnibusGrant,
            requester: "unibus".into(),
            principal: "oidc:example#avery".into(),
            action: "unibus.node.grant".into(),
            payload_digest: digest,
            effects,
            scopes: vec!["marbles.issue.read".into()],
            resources: vec!["HOME".into()],
            requested_scope: Scope::Session,
            preview: Preview::SporeScene {
                spore: "dev.fungos.marbles".into(),
                view: "grant-preview".into(),
                summary: "Marbles issue read".into(),
            },
            timeout_ms: 1_000,
        }
    }

    fn decision(allowed: bool, scope: Scope) -> Decision {
        Decision {
            request_id: "req-1".into(),
            allowed,
            scope,
            reason: None,
            decided_by: "human:avery".into(),
            decided_at_ms: 1_000_000,
        }
    }

    #[test]
    fn approved_grant_is_shortened_by_once_scope() {
        let spec = spec();
        let request = request(spec.digest().unwrap(), vec![Effect::Write]);
        let approved =
            approved_node_grant(&request, &decision(true, Scope::Once), &spec, 1_000).unwrap();
        assert_eq!(approved.grant.producer_id, "marbles");
        assert_eq!(approved.expires_at_unix_s, 1_300);
    }

    #[test]
    fn denial_and_payload_mismatch_fail_closed() {
        let spec = spec();
        let digest = spec.digest().unwrap();
        let request = request(digest.clone(), vec![Effect::Write]);
        assert!(matches!(
            approved_node_grant(&request, &decision(false, Scope::Once), &spec, 1_000),
            Err(Error::Denied)
        ));

        let mut changed = spec.clone();
        changed.capabilities.push("marbles.issue.close".into());
        assert!(matches!(
            approved_node_grant(&request, &decision(true, Scope::Once), &changed, 1_000),
            Err(Error::PayloadMismatch)
        ));
    }
}
