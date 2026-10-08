//! Project application-owned endpoints through the provider-neutral directory.
//! Mycelium sync ALPNs alone are never eligible for Unibus traffic.
use std::collections::BTreeMap;
use unibus::{IrohPeerEndpoint, PeerCandidate, PeerDirectory, PeerError, ServiceDirectory};

#[derive(Debug, Clone)]
pub struct ServiceBinding {
    pub service_id: String,
    pub node_id: String,
}

pub struct IrohDirectory<D> {
    directory: D,
    /// Trusted deployment bindings: Unibus producer -> service and owner.
    bindings: BTreeMap<String, ServiceBinding>,
}

impl<D> IrohDirectory<D> {
    pub fn new(directory: D, bindings: BTreeMap<String, ServiceBinding>) -> Self {
        Self {
            directory,
            bindings,
        }
    }
}

impl<D: ServiceDirectory> PeerDirectory for IrohDirectory<D> {
    fn resolve(&self, peer_id: &str, now: u64) -> Result<PeerCandidate, PeerError> {
        let binding = self
            .bindings
            .get(peer_id)
            .ok_or_else(|| PeerError("no Iroh service binding for peer".into()))?;
        let observations = self
            .directory
            .services("iroh")
            .map_err(|e| PeerError(e.to_string()))?;
        if observations.len() > 256 {
            return Err(PeerError("Iroh directory exceeds bound".into()));
        }
        let mut matches = observations
            .into_iter()
            .filter(|s| s.service_id == binding.service_id);
        let observation = matches
            .next()
            .ok_or_else(|| PeerError("Iroh service is unavailable".into()))?;
        if matches.next().is_some() {
            return Err(PeerError("conflicting Iroh service identities".into()));
        }
        observation
            .validate()
            .map_err(|e| PeerError(e.to_string()))?;
        if observation
            .attributes
            .iter()
            .map(|(k, v)| k.len() + v.len())
            .sum::<usize>()
            > 65536
        {
            return Err(PeerError("Iroh attributes exceed bound".into()));
        }
        if !observation.is_fresh_at(now)
            || observation
                .expires_at_unix_s
                .saturating_sub(observation.observed_at_unix_s)
                > 300
            || observation.provenance_observer != binding.node_id
            || observation.attributes.get("node_id") != Some(&binding.node_id)
        {
            return Err(PeerError("stale or incorrectly owned Iroh service".into()));
        }
        let attribute = |name: &str| {
            observation
                .attributes
                .get(name)
                .ok_or_else(|| PeerError(format!("missing Iroh {name}")))
        };
        let alpns: Vec<String> =
            serde_json::from_str(attribute("alpns")?).map_err(|e| PeerError(e.to_string()))?;
        if alpns.len() > 16 || !alpns.iter().any(|alpn| alpn == crate::PEER_ALPN) {
            return Err(PeerError(
                "service does not support Unibus peer protocol".into(),
            ));
        }
        let endpoint = IrohPeerEndpoint {
            endpoint_id: attribute("endpoint_id")?.clone(),
            direct_addresses: serde_json::from_str(attribute("direct_addresses")?)
                .map_err(|e| PeerError(e.to_string()))?,
            relay_urls: serde_json::from_str(attribute("relay_urls")?)
                .map_err(|e| PeerError(e.to_string()))?,
        };
        let candidate = PeerCandidate {
            peer_id: peer_id.into(),
            site: None,
            endpoints: vec![],
            iroh_endpoints: vec![endpoint],
            expires_at_unix_s: observation.expires_at_unix_s,
        };
        candidate.validate(now)?;
        Ok(candidate)
    }
}
