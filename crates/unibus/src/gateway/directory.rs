//! Consume provider-neutral observations; never scan or grant access.
use super::{GATEWAY_SERVICE_KIND, GatewayLease, ResourceId};
use crate::{PeerError, ServiceDirectory};
use std::collections::BTreeMap;

pub struct GatewayDirectory<D> {
    directory: D,
    /// Trusted canonical gateway machine -> authorized bus producer binding.
    producers: BTreeMap<String, String>,
}
impl<D: ServiceDirectory> GatewayDirectory<D> {
    pub fn new(directory: D, producers: BTreeMap<String, String>) -> Self {
        Self {
            directory,
            producers,
        }
    }
    pub fn gateways(
        &self,
        resource: &ResourceId,
        now: u64,
    ) -> Result<Vec<GatewayLease>, PeerError> {
        resource.validate()?;
        let observations = self
            .directory
            .services(GATEWAY_SERVICE_KIND)
            .map_err(|e| PeerError(e.to_string()))?;
        if observations.len() > 1024 {
            return Err(PeerError("gateway directory exceeds bound".into()));
        }
        let mut leases: BTreeMap<String, GatewayLease> = BTreeMap::new();
        for observation in observations {
            observation
                .validate()
                .map_err(|e| PeerError(e.to_string()))?;
            if observation.kind != GATEWAY_SERVICE_KIND || !observation.is_fresh_at(now) {
                continue;
            }
            let encoded = observation
                .attributes
                .get("gateway_lease")
                .ok_or_else(|| PeerError("missing gateway lease".into()))?;
            if encoded.len() > 8192 {
                return Err(PeerError("gateway lease exceeds bound".into()));
            }
            let lease: GatewayLease =
                serde_json::from_str(encoded).map_err(|e| PeerError(e.to_string()))?;
            if &lease.resource != resource {
                continue;
            }
            if self.producers.get(&lease.gateway_peer_id) != Some(&lease.endpoint.peer_id) {
                continue;
            }
            lease.validate_at(now)?;
            lease.require_owner(&observation.provenance_observer)?;
            if observation.attributes.get("node_id") != Some(&lease.gateway_peer_id)
                || observation.observed_at_unix_s != lease.observed_at_unix_s
                || observation.expires_at_unix_s != lease.expires_at_unix_s
                || observation
                    .endpoints
                    .iter()
                    .cloned()
                    .collect::<std::collections::BTreeSet<_>>()
                    != lease.connection_hints()?
            {
                return Err(PeerError("gateway observation and lease disagree".into()));
            }
            if let Some(existing) = leases.get(&lease.gateway_peer_id)
                && existing != &lease
            {
                return Err(PeerError("conflicting gateway leases".into()));
            }
            leases.insert(lease.gateway_peer_id.clone(), lease);
            if leases.len() > 32 {
                return Err(PeerError("too many gateways for resource".into()));
            }
        }
        Ok(leases.into_values().collect())
    }
}
