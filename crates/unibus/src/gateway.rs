//! Transport-independent resource identity and explicit gateway leases.
//! Inventory is not an access grant. Native addresses and secrets stay local.
use crate::{PeerCandidate, PeerError, is_valid_namespaced_name};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

pub const GATEWAY_SCHEMA: u32 = 1;
pub const GATEWAY_INVOKE: &str = "unibus.gateway.invoke.v1";
pub const GATEWAY_RESULT: &str = "unibus.gateway.result.v1";
pub const MAX_GATEWAY_LEASE_SECS: u64 = 300;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "id",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum ResourceScope {
    /// LAN device without verified globally unique identity.
    Site(String),
    /// Local application or verified machine-owned resource.
    Peer(String),
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResourceId {
    pub scope: ResourceScope,
    pub key: String,
}
impl ResourceId {
    pub fn validate(&self) -> Result<(), PeerError> {
        let (ResourceScope::Site(scope) | ResourceScope::Peer(scope)) = &self.scope;
        identifier(scope)?;
        identifier(&self.key)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Protection {
    Present,
    Absent,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeSecurity {
    pub encryption: Protection,
    pub authentication: Protection,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GatewayLease {
    pub schema_version: u32,
    pub resource: ResourceId,
    /// Canonical fleet machine; distinct from the bus/application producer.
    pub gateway_peer_id: String,
    pub source_service_id: String,
    /// Reuse the existing peer transport candidate, including optional Iroh.
    pub endpoint: PeerCandidate,
    pub generation: u64,
    pub observed_at_unix_s: u64,
    pub expires_at_unix_s: u64,
    /// Exact operations this adapter actually implements, not permissions.
    pub operations: BTreeSet<String>,
    pub native_security: NativeSecurity,
}
impl GatewayLease {
    pub fn validate_at(&self, now: u64) -> Result<(), PeerError> {
        self.resource.validate()?;
        identifier(&self.gateway_peer_id)?;
        identifier(&self.source_service_id)?;
        self.endpoint.validate(now)?;
        if self.schema_version != GATEWAY_SCHEMA
            || self.generation == 0
            || self.observed_at_unix_s > now
            || now >= self.expires_at_unix_s
            || self
                .expires_at_unix_s
                .saturating_sub(self.observed_at_unix_s)
                > MAX_GATEWAY_LEASE_SECS
            || self.expires_at_unix_s > self.endpoint.expires_at_unix_s
        {
            return Err(PeerError("invalid or expired gateway lease".into()));
        }
        operations(&self.operations)
    }
    /// Signed observation ownership must be verified by the directory adapter
    /// before passing its canonical observer identity here.
    pub fn require_owner(&self, authenticated_observer: &str) -> Result<(), PeerError> {
        if self.gateway_peer_id != authenticated_observer {
            return Err(PeerError(
                "gateway lease observer does not own endpoint".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GatewayInvoke {
    pub schema_version: u32,
    pub request_id: String,
    pub resource: ResourceId,
    pub generation: u64,
    pub operation: String,
    /// Adapter-specific input, never interpreted as a generic proxy URL.
    pub input: serde_json::Value,
}
impl GatewayInvoke {
    pub fn validate(&self) -> Result<(), PeerError> {
        self.resource.validate()?;
        identifier(&self.request_id)?;
        if self.schema_version != GATEWAY_SCHEMA
            || self.generation == 0
            || self.operation.len() > 256
            || !is_valid_namespaced_name(&self.operation)
        {
            return Err(PeerError("invalid gateway invocation".into()));
        }
        if serde_json::to_vec(&self.input)
            .map_err(|e| PeerError(e.to_string()))?
            .len()
            > 65536
        {
            return Err(PeerError("gateway input exceeds bound".into()));
        }
        Ok(())
    }
}

pub fn identifier(value: &str) -> Result<(), PeerError> {
    if value.is_empty()
        || value.len() > 256
        || !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.:/".contains(&b))
    {
        return Err(PeerError("invalid bounded gateway identifier".into()));
    }
    Ok(())
}
pub fn operations(values: &BTreeSet<String>) -> Result<(), PeerError> {
    if values.is_empty()
        || values.len() > 32
        || values
            .iter()
            .any(|v| !is_valid_namespaced_name(v) || v.len() > 256)
    {
        return Err(PeerError("invalid gateway operations".into()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::IrohPeerEndpoint;
    fn lease() -> GatewayLease {
        GatewayLease {
            schema_version: 1,
            resource: ResourceId {
                scope: ResourceScope::Site("home".into()),
                key: "camera-192.168.1.20".into(),
            },
            gateway_peer_id: "fleet-peer".into(),
            source_service_id: "service/camera".into(),
            endpoint: PeerCandidate {
                peer_id: "local-unibus".into(),
                site: Some("home".into()),
                endpoints: vec![],
                iroh_endpoints: vec![IrohPeerEndpoint {
                    endpoint_id: "ab".repeat(32),
                    direct_addresses: vec!["192.168.1.10:9999".parse().unwrap()],
                    relay_urls: vec![],
                }],
                expires_at_unix_s: 400,
            },
            generation: 1,
            observed_at_unix_s: 100,
            expires_at_unix_s: 400,
            operations: BTreeSet::from(["camera.view".into()]),
            native_security: NativeSecurity {
                encryption: Protection::Absent,
                authentication: Protection::Unknown,
            },
        }
    }
    #[test]
    fn private_addresses_at_different_sites_are_not_the_same_resource() {
        let a = lease().resource;
        let mut b = a.clone();
        b.scope = ResourceScope::Site("lab".into());
        assert_ne!(a, b);
        assert_eq!(
            serde_json::from_str::<ResourceId>(&serde_json::to_string(&a).unwrap()).unwrap(),
            a
        );
    }
    #[test]
    fn leases_preserve_owner_expiry_and_transport_bounds() {
        let mut lease = lease();
        assert!(lease.validate_at(100).is_ok());
        assert!(lease.validate_at(400).is_err());
        assert!(lease.require_owner("other-peer").is_err());
        lease.expires_at_unix_s = 401;
        assert!(lease.validate_at(100).is_err());
    }
    #[test]
    fn unknown_fields_cannot_smuggle_credentials_or_proxy_destinations() {
        let mut json = serde_json::to_value(lease()).unwrap();
        json["token"] = "secret".into();
        assert!(serde_json::from_value::<GatewayLease>(json).is_err());
        let invoke = GatewayInvoke {
            schema_version: 1,
            request_id: "request".into(),
            resource: lease().resource,
            generation: 1,
            operation: "camera.view".into(),
            input: serde_json::json!({}),
        };
        assert!(invoke.validate().is_ok());
        let mut json = serde_json::to_value(invoke).unwrap();
        json["endpoint"] = "http://localhost/admin".into();
        assert!(serde_json::from_value::<GatewayInvoke>(json).is_err());
    }
}
