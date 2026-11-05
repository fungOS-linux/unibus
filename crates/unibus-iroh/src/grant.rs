//! Service authorization from existing signed Mycelium access records.
//! Identity/key bindings and service grants remain separate statements.
pub use mycelium_peer_protocol::AccessRecord;
use mycelium_peer_protocol::AccessStatement;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    path::PathBuf,
    sync::Mutex,
    time::{Duration, Instant},
};
use unibus::{
    DirectSessionClaims, DirectSessionError, DirectSessionVerifier, PeerAccessPolicy, PeerBinding,
    PeerError, PeerFuture,
};

pub const SCOPE_PREFIX: &str = "unibus-session:";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Permit {
    pub audience: String,
    pub installation: String,
    pub initiator: PeerBinding,
    pub responder: PeerBinding,
    pub kinds: Vec<String>,
    pub max_payload_bytes: u32,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrustDocument {
    pub grant: AccessRecord,
    pub revocations: Vec<AccessRecord>,
    pub permit: Permit,
}

pub fn permit_scope(permit: &Permit) -> Result<String, PeerError> {
    let value = serde_json::to_value(permit).map_err(|e| PeerError(e.to_string()))?;
    let canonical = serde_json::to_vec(&value).map_err(|e| PeerError(e.to_string()))?;
    Ok(format!(
        "{SCOPE_PREFIX}{}",
        mycelium_peer_protocol::sha256_hex(&canonical)
    ))
}

fn authorization_shape(record: &AccessRecord) -> Result<AccessStatement, PeerError> {
    record.verify().map_err(PeerError)?;
    let mut statement = record.statement.clone();
    match &mut statement {
        AccessStatement::Grant {
            serial,
            not_before,
            not_after,
            ..
        } => {
            *serial = 0;
            *not_before = 0;
            *not_after = 0;
        }
        _ => return Err(PeerError("expected service grant".into())),
    }
    Ok(statement)
}

/// Reconcile signed public records from any carrier. The retained document is
/// the monotonic floor: missing/older observations cannot remove revocations or
/// replace permissions. Carrier availability is never authorization.
pub fn reconcile(
    current: &TrustDocument,
    grants: &[AccessRecord],
    revocations: &[AccessRecord],
    issuers: &BTreeSet<String>,
) -> Result<TrustDocument, PeerError> {
    let shape = authorization_shape(&current.grant)?;
    if !issuers.contains(&current.grant.signer) {
        return Err(PeerError("untrusted retained service issuer".into()));
    }
    let AccessStatement::Grant {
        grant_id, scopes, ..
    } = &current.grant.statement
    else {
        unreachable!()
    };
    if scopes
        .iter()
        .filter(|s| s.starts_with(SCOPE_PREFIX))
        .count()
        != 1
        || !scopes.contains(&permit_scope(&current.permit)?)
    {
        return Err(PeerError(
            "retained permit differs from signed scope".into(),
        ));
    }
    let serial = |r: &AccessRecord| match r.statement {
        AccessStatement::Grant { serial, .. } => serial,
        _ => 0,
    };
    let mut next = current.clone();
    for record in grants {
        if !matches!(&record.statement, AccessStatement::Grant { grant_id:id,.. } if id == grant_id)
        {
            continue;
        }
        if record.signer != current.grant.signer || authorization_shape(record)? != shape {
            return Err(PeerError(
                "service renewal changes issuer or authorization".into(),
            ));
        }
        if serial(record) == serial(&next.grant) && record != &next.grant {
            return Err(PeerError("conflicting service renewal serial".into()));
        }
        if serial(record) > serial(&next.grant) {
            next.grant = record.clone();
        }
    }
    for record in current.revocations.iter().chain(revocations) {
        record.verify().map_err(PeerError)?;
        if !issuers.contains(&record.signer)
            || !matches!(record.statement, AccessStatement::Revoke { .. })
        {
            return Err(PeerError("untrusted service revocation".into()));
        }
        // Retain serial-scoped revocations too; a later replay must not erase them.
        let relevant = match &record.statement {
            AccessStatement::Revoke {
                grant_id: id,
                principal,
                ..
            } => {
                id.as_ref() == Some(grant_id)
                    || matches!(&current.grant.statement, AccessStatement::Grant { principal:p,.. } if principal.as_ref()==Some(p))
            }
            _ => false,
        };
        if relevant && !next.revocations.contains(record) {
            next.revocations.push(record.clone());
        }
    }
    if next.revocations.len() > 64 {
        return Err(PeerError("service revocation bound exceeded".into()));
    }
    Ok(next)
}

/// Only explicitly configured issuer keys are trusted. Remote session JSON
/// cannot supply trust roots, grants, revocations, or an authorization lease.
pub struct GrantVerifier {
    path: PathBuf,
    issuers: BTreeSet<String>,
    cache: Mutex<Option<(Instant, TrustDocument)>>,
}

impl GrantVerifier {
    pub fn new(path: PathBuf, issuers: BTreeSet<String>) -> Result<Self, PeerError> {
        if issuers.is_empty() || issuers.len() > 16 {
            return Err(PeerError("explicit service grant issuers required".into()));
        }
        Ok(Self {
            path,
            issuers,
            cache: Mutex::new(None),
        })
    }

    fn document(&self) -> Result<TrustDocument, PeerError> {
        let mut cache = self
            .cache
            .lock()
            .map_err(|_| PeerError("grant cache poisoned".into()))?;
        if let Some((at, document)) = &*cache {
            if at.elapsed() < Duration::from_secs(1) {
                return Ok(document.clone());
            }
        }
        // Bound the actual read, not merely metadata checked before opening.
        use std::io::Read;
        let mut bytes = Vec::new();
        std::fs::File::open(&self.path)
            .and_then(|file| file.take(65537).read_to_end(&mut bytes))
            .map_err(|e| PeerError(format!("read retained service grant: {e}")))?;
        if bytes.len() > 65536 {
            return Err(PeerError("service grant document exceeds bound".into()));
        }
        let document: TrustDocument =
            serde_json::from_slice(&bytes).map_err(|e| PeerError(e.to_string()))?;
        // Verify signatures once per bounded trust refresh, never per media
        // packet. Validity windows and scoped authorization remain per frame.
        document.grant.verify().map_err(PeerError)?;
        if !self.issuers.contains(&document.grant.signer) || document.revocations.len() > 64 {
            return Err(PeerError(
                "untrusted service issuer or excessive revocations".into(),
            ));
        }
        for record in &document.revocations {
            record.verify().map_err(PeerError)?;
            if !self.issuers.contains(&record.signer) {
                return Err(PeerError("untrusted service revocation".into()));
            }
        }
        *cache = Some((Instant::now(), document.clone()));
        Ok(document)
    }

    pub fn permit(&self, now: u64) -> Result<(Permit, String, String, u64), PeerError> {
        let document = self.document()?;
        for record in &document.revocations {
            if matches!(&record.statement, AccessStatement::Revoke {revoked_at, ..} if *revoked_at <= now)
                && record.statement.revokes(&document.grant.statement)
            {
                return Err(PeerError("service grant revoked".into()));
            }
        }
        let AccessStatement::Grant {
            grant_id,
            scopes,
            not_before,
            not_after,
            ..
        } = &document.grant.statement
        else {
            return Err(PeerError("expected a signed access grant".into()));
        };
        if now < *not_before || now >= *not_after {
            return Err(PeerError("service grant outside validity window".into()));
        }
        let permits: Vec<_> = scopes
            .iter()
            .filter_map(|scope| scope.strip_prefix(SCOPE_PREFIX))
            .collect();
        if permits.len() != 1 {
            return Err(PeerError(
                "exactly one service session scope required".into(),
            ));
        }
        let permit = document.permit;
        if format!("{SCOPE_PREFIX}{}", permits[0]) != permit_scope(&permit)? {
            return Err(PeerError(
                "local permit differs from signed scope digest".into(),
            ));
        }
        Ok((
            permit,
            document.grant.signer.clone(),
            grant_id.clone(),
            *not_after,
        ))
    }

    /// Unsigned short-lived session parameters only specialize a locally held,
    /// authority-signed permit. Verification compares every authority-bearing
    /// field to that permit; possession of this JSON grants nothing by itself.
    pub fn assertion(&self, now: u64) -> Result<String, PeerError> {
        let (permit, issuer, id, until) = self.permit(now)?;
        let claims = DirectSessionClaims {
            schema_version: 1,
            iss: issuer,
            aud: permit.audience,
            installation_id: permit.installation,
            jti: format!("{id}-{now}"),
            iat: now,
            exp: (now + 240).min(until),
            initiator: permit.initiator,
            responder: permit.responder,
            kinds: permit.kinds,
            max_payload_bytes: permit.max_payload_bytes,
        };
        self.verify_claims(&claims, now)?;
        serde_json::to_string(&claims).map_err(|e| PeerError(e.to_string()))
    }

    fn verify_claims(&self, claims: &DirectSessionClaims, now: u64) -> Result<(), PeerError> {
        let (permit, issuer, id, until) = self.permit(now)?;
        if claims.iss != issuer
            || claims.aud != permit.audience
            || claims.installation_id != permit.installation
            || claims.initiator != permit.initiator
            || claims.responder != permit.responder
            || claims.kinds != permit.kinds
            || claims.max_payload_bytes != permit.max_payload_bytes
            || claims.exp > until
            || claims.jti != format!("{id}-{}", claims.iat)
        {
            return Err(PeerError(
                "session parameters exceed signed service permit".into(),
            ));
        }
        claims
            .validate(
                &permit.audience,
                &permit.installation,
                &permit.initiator.producer_id,
                now,
            )
            .map_err(|e| PeerError(e.to_string()))
    }
}

impl DirectSessionVerifier for GrantVerifier {
    fn verify(&self, assertion: &str, now: u64) -> Result<DirectSessionClaims, DirectSessionError> {
        if assertion.len() > 32768 {
            return Err(DirectSessionError("session assertion exceeds bound".into()));
        }
        let claims: DirectSessionClaims =
            serde_json::from_str(assertion).map_err(|e| DirectSessionError(e.to_string()))?;
        self.verify_claims(&claims, now)
            .map_err(|e| DirectSessionError(e.to_string()))?;
        Ok(claims)
    }
}

impl PeerAccessPolicy for GrantVerifier {
    fn authorize<'a>(
        &'a self,
        source: &'a str,
        destination: &'a str,
        kind: &'a str,
        now: u64,
    ) -> PeerFuture<'a, ()> {
        Box::pin(async move {
            let (permit, _, _, _) = self.permit(now)?;
            let a = &permit.initiator.producer_id;
            let b = &permit.responder.producer_id;
            if !((source == a && destination == b) || (source == b && destination == a))
                || !permit.kinds.iter().any(|allowed| allowed == kind)
            {
                return Err(PeerError("service operation outside signed permit".into()));
            }
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;

    fn fixture() -> (tempfile::TempDir, GrantVerifier, TrustDocument, SigningKey) {
        let dir = tempfile::tempdir().unwrap();
        let key = SigningKey::from_bytes(&[71; 32]);
        let permit = Permit {
            audience: "private-services".into(),
            installation: "fleet".into(),
            initiator: PeerBinding {
                producer_id: "a".into(),
                key_binding: format!("iroh:{}", "1".repeat(64)),
                candidates: vec![],
            },
            responder: PeerBinding {
                producer_id: "b".into(),
                key_binding: format!("iroh:{}", "2".repeat(64)),
                candidates: vec![],
            },
            kinds: vec!["unibus.transport.udp.v1".into()],
            max_payload_bytes: 65507,
        };
        let statement: AccessStatement = serde_json::from_value(serde_json::json!({
            "kind":"grant", "grant_id":"test-service", "principal":"test-client", "serial":1,
            "roles":[], "scopes":[permit_scope(&permit).unwrap()],
            "unix_users":[], "ssh_public_keys":[], "not_before":100, "not_after":1000
        }))
        .unwrap();
        let document = TrustDocument {
            grant: AccessRecord::sign(&key, statement).unwrap(),
            revocations: vec![],
            permit,
        };
        let path = dir.path().join("trust.json");
        std::fs::write(&path, serde_json::to_vec(&document).unwrap()).unwrap();
        let verifier =
            GrantVerifier::new(path, BTreeSet::from([document.grant.signer.clone()])).unwrap();
        (dir, verifier, document, key)
    }

    #[test]
    fn reconciliation_retains_revocations_and_monotonic_permissions() {
        let (_dir, _, initial, key) = fixture();
        let mut statement = initial.grant.statement.clone();
        if let AccessStatement::Grant {
            serial, not_after, ..
        } = &mut statement
        {
            *serial = 2;
            *not_after = 2000;
        }
        let renewed = AccessRecord::sign(&key, statement).unwrap();
        let issuers = BTreeSet::from([initial.grant.signer.clone()]);
        let next = reconcile(&initial, &[renewed.clone()], &[], &issuers).unwrap();
        assert_eq!(next.grant, renewed);
        assert_eq!(
            reconcile(&next, &[initial.grant.clone()], &[], &issuers)
                .unwrap()
                .grant,
            renewed
        );
        let revoke = AccessRecord::sign(
            &key,
            serde_json::from_value(serde_json::json!({
                "kind":"revoke", "revocation_id":"stop", "grant_id":"test-service",
                "revoked_at":150, "reason":"test"
            }))
            .unwrap(),
        )
        .unwrap();
        let revoked = reconcile(&next, &[], &[revoke.clone()], &issuers).unwrap();
        assert_eq!(
            reconcile(&revoked, &[], &[], &issuers).unwrap().revocations,
            vec![revoke]
        );
        let mut changed = renewed.statement.clone();
        if let AccessStatement::Grant { roles, .. } = &mut changed {
            roles.push("admin".into());
        }
        let changed = AccessRecord::sign(&key, changed).unwrap();
        assert!(reconcile(&next, &[changed], &[], &issuers).is_err());
        let mut collision = renewed.statement.clone();
        if let AccessStatement::Grant { not_after, .. } = &mut collision {
            *not_after += 1;
        }
        assert!(
            reconcile(
                &next,
                &[AccessRecord::sign(&key, collision).unwrap()],
                &[],
                &issuers
            )
            .is_err()
        );
        let stranger = SigningKey::from_bytes(&[72; 32]);
        assert!(
            reconcile(
                &next,
                &[AccessRecord::sign(&stranger, renewed.statement).unwrap()],
                &[],
                &issuers
            )
            .is_err()
        );
    }

    #[test]
    fn signed_permit_cannot_be_widened_by_session_parameters() {
        let (_dir, verifier, _, _) = fixture();
        let assertion = verifier.assertion(200).unwrap();
        let original = verifier.verify(&assertion, 200).unwrap();
        for field in ["key", "kind", "budget", "expiry", "issuer", "installation"] {
            let mut claims = original.clone();
            match field {
                "key" => claims.initiator.key_binding = format!("iroh:{}", "3".repeat(64)),
                "kind" => claims.kinds.push("admin.write".into()),
                "budget" => claims.max_payload_bytes += 1,
                "expiry" => claims.exp = 1001,
                "issuer" => claims.iss = "attacker".into(),
                _ => claims.installation_id = "other".into(),
            }
            assert!(
                verifier
                    .verify(&serde_json::to_string(&claims).unwrap(), 200)
                    .is_err(),
                "{field}"
            );
        }
        assert!(verifier.assertion(1000).is_err());
        assert!(verifier.assertion(99).is_err());
    }

    #[test]
    fn signed_revocation_and_untrusted_issuer_fail_closed() {
        let (_dir, verifier, mut document, key) = fixture();
        let revoke: AccessStatement = serde_json::from_value(serde_json::json!({
            "kind":"revoke", "revocation_id":"revoke-test", "grant_id":"test-service",
            "revoked_at":150, "reason":"test"
        }))
        .unwrap();
        document
            .revocations
            .push(AccessRecord::sign(&key, revoke).unwrap());
        std::fs::write(&verifier.path, serde_json::to_vec(&document).unwrap()).unwrap();
        assert!(verifier.assertion(200).is_err());
        let wrong =
            GrantVerifier::new(verifier.path.clone(), BTreeSet::from(["0".repeat(64)])).unwrap();
        assert!(wrong.assertion(200).is_err());
    }
}
