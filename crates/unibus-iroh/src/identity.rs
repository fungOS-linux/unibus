//! Optional consumer of Mycelium's portable contract, not its daemon.
use crate::transport::EndpointIdentity;
use mycelium_peer_protocol::identity::{
    IdentityRecord, IdentityStatement, IdentityTrust, verify_iroh_peer,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};
use unibus::PeerError;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FleetProof {
    pub membership: IdentityRecord,
    pub endpoint: IdentityRecord,
}

/// Returned only from trusted, retained local state. Storage owners must persist
/// revocations and floors across reboot; remote proofs may never supply them.
pub struct TrustSnapshot {
    pub fleet_id: String,
    pub membership_issuers: BTreeSet<String>,
    pub revocations: Vec<IdentityRecord>,
    pub minimum_membership_generation: u64,
    pub minimum_endpoint_generation: u64,
    pub observed_at: u64,
    pub expires_at: u64,
}

pub trait FleetTrustSource: Send + Sync {
    /// Nonblocking local lookup, scoped to machine and endpoint credential slot.
    fn snapshot(&self, peer: &str, slot: &str) -> Result<TrustSnapshot, PeerError>;
}

#[derive(Clone)]
pub struct ProducerIdentity {
    pub peer_id: String,
    pub endpoint_credential_id: String,
}

pub struct FleetIdentity {
    local: FleetProof,
    trust: Arc<dyn FleetTrustSource>,
    /// Operator-owned producer -> canonical machine bindings.
    producers: BTreeMap<String, ProducerIdentity>,
}

impl FleetIdentity {
    pub fn new(
        local: FleetProof,
        trust: Arc<dyn FleetTrustSource>,
        producers: BTreeMap<String, ProducerIdentity>,
    ) -> Self {
        Self {
            local,
            trust,
            producers,
        }
    }
}

impl EndpointIdentity for FleetIdentity {
    fn local_proof(&self) -> Result<serde_json::Value, PeerError> {
        serde_json::to_value(&self.local).map_err(|e| PeerError(e.to_string()))
    }
    fn verify(
        &self,
        producer: &str,
        handshake_key: &str,
        proof: &serde_json::Value,
        now: u64,
    ) -> Result<(), PeerError> {
        let proof: FleetProof =
            serde_json::from_value(proof.clone()).map_err(|e| PeerError(e.to_string()))?;
        let IdentityStatement::Membership { peer_id, .. } = &proof.membership.statement else {
            return Err(PeerError("expected fleet membership".into()));
        };
        let IdentityStatement::Endpoint { credential_id, .. } = &proof.endpoint.statement else {
            return Err(PeerError("expected endpoint proof".into()));
        };
        if !self.producers.get(producer).is_some_and(|binding| {
            binding.peer_id == *peer_id && binding.endpoint_credential_id == *credential_id
        }) {
            return Err(PeerError("producer is not bound to this fleet peer".into()));
        }
        let state = self.trust.snapshot(peer_id, credential_id)?;
        if state.observed_at > now
            || now >= state.expires_at
            || state.minimum_membership_generation == 0
            || state.minimum_endpoint_generation == 0
        {
            return Err(PeerError(
                "fleet trust state is stale or missing generation floors".into(),
            ));
        }
        verify_iroh_peer(
            &proof.membership,
            &proof.endpoint,
            handshake_key,
            &IdentityTrust {
                fleet_id: &state.fleet_id,
                membership_issuers: &state.membership_issuers,
                revocations: &state.revocations,
                minimum_membership_generation: state.minimum_membership_generation,
                minimum_endpoint_generation: state.minimum_endpoint_generation,
                now,
            },
        )
        .map_err(PeerError)?;
        Ok(())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;
    use mycelium_peer_protocol::{TransportCredentialBinding, TransportKind};
    use std::sync::atomic::{AtomicBool, Ordering};

    fn public(key: &SigningKey) -> String {
        key.verifying_key()
            .as_bytes()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }
    struct RetainedTrust {
        issuer: String,
        expires: u64,
        stale: Arc<AtomicBool>,
        revoked: Arc<AtomicBool>,
        authority: SigningKey,
    }
    impl FleetTrustSource for RetainedTrust {
        fn snapshot(&self, peer: &str, slot: &str) -> Result<TrustSnapshot, PeerError> {
            let revocations = if self.revoked.load(Ordering::SeqCst) {
                vec![
                    IdentityRecord::sign(
                        &self.authority,
                        "fleet".into(),
                        IdentityStatement::Revoke {
                            peer_id: peer.into(),
                            credential_id: slot.into(),
                            revoked_at: 0,
                        },
                    )
                    .unwrap(),
                ]
            } else {
                vec![]
            };
            Ok(TrustSnapshot {
                fleet_id: "fleet".into(),
                membership_issuers: BTreeSet::from([self.issuer.clone()]),
                revocations,
                minimum_membership_generation: 1,
                minimum_endpoint_generation: 1,
                observed_at: 0,
                expires_at: if self.stale.load(Ordering::SeqCst) {
                    0
                } else {
                    self.expires
                },
            })
        }
    }
    pub(crate) fn pair(
        a: &str,
        b: &str,
        now: u64,
    ) -> (
        Arc<FleetIdentity>,
        Arc<FleetIdentity>,
        Arc<AtomicBool>,
        Arc<AtomicBool>,
    ) {
        let authority = SigningKey::from_bytes(&[1; 32]);
        let machine_a = SigningKey::from_bytes(&[2; 32]);
        let machine_b = SigningKey::from_bytes(&[3; 32]);
        let proof = |machine: &SigningKey, key: &str| FleetProof {
            membership: IdentityRecord::sign(
                &authority,
                "fleet".into(),
                IdentityStatement::Membership {
                    credential_id: "membership".into(),
                    peer_id: public(machine),
                    generation: 1,
                    not_before: now,
                    not_after: now + 60,
                },
            )
            .unwrap(),
            endpoint: IdentityRecord::sign(
                machine,
                "fleet".into(),
                IdentityStatement::Endpoint {
                    credential_id: "unibus-endpoint".into(),
                    not_before: now,
                    credential: TransportCredentialBinding {
                        node_id: public(machine),
                        kind: TransportKind::Iroh,
                        public_key: key.into(),
                        generation: 1,
                        valid_until: Some(now + 60),
                    },
                },
            )
            .unwrap(),
        };
        let stale = Arc::new(AtomicBool::new(false));
        let revoked = Arc::new(AtomicBool::new(false));
        let trust = Arc::new(RetainedTrust {
            issuer: public(&authority),
            expires: now + 60,
            stale: stale.clone(),
            revoked: revoked.clone(),
            authority: authority.clone(),
        });
        let bindings = BTreeMap::from([
            (
                "a".into(),
                ProducerIdentity {
                    peer_id: public(&machine_a),
                    endpoint_credential_id: "unibus-endpoint".into(),
                },
            ),
            (
                "b".into(),
                ProducerIdentity {
                    peer_id: public(&machine_b),
                    endpoint_credential_id: "unibus-endpoint".into(),
                },
            ),
        ]);
        (
            Arc::new(FleetIdentity::new(
                proof(&machine_a, a),
                trust.clone(),
                bindings.clone(),
            )),
            Arc::new(FleetIdentity::new(proof(&machine_b, b), trust, bindings)),
            stale,
            revoked,
        )
    }
    #[test]
    fn real_signatures_enforce_key_producer_expiry_revocation_and_freshness() {
        let a = public(&SigningKey::from_bytes(&[4; 32]));
        let b = public(&SigningKey::from_bytes(&[5; 32]));
        let (local, remote, stale, revoked) = pair(&a, &b, 100);
        let proof = remote.local_proof().unwrap();
        assert!(local.verify("b", &b, &proof, 100).is_ok());
        assert!(local.verify("b", &a, &proof, 100).is_err());
        assert!(local.verify("a", &b, &proof, 100).is_err());
        assert!(local.verify("b", &b, &proof, 160).is_err());
        assert!(
            local
                .verify("b", &b, &serde_json::Value::Null, 100)
                .is_err()
        );
        let mut forged = proof.clone();
        forged["membership"]["signature"] = "00".repeat(64).into();
        assert!(local.verify("b", &b, &forged, 100).is_err());
        stale.store(true, Ordering::SeqCst);
        assert!(local.verify("b", &b, &proof, 100).is_err());
        stale.store(false, Ordering::SeqCst);
        revoked.store(true, Ordering::SeqCst);
        assert!(local.verify("b", &b, &proof, 100).is_err());
    }
}
