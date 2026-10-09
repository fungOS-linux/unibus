//! Explicit local-app routing in the existing daemon, not LAN discovery.
use crate::{HubHandle, HubPlugin};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Mutex,
};
use unibus::{
    Envelope, Message,
    gateway::{
        GATEWAY_INVOKE, GATEWAY_RESULT, GatewayInvoke, GatewayLease, ResourceId, identifier,
        operations,
    },
};

const MAX_ROUTES: usize = 128;
const MAX_PENDING: usize = 256;
const MAX_RETAINED_IDS: usize = 4096;

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LocalRoute {
    pub lease: GatewayLease,
    /// An existing authenticated bus adapter, not a caller-selected address.
    pub backend_producer: String,
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ResourceGrant {
    pub caller_producer: String,
    pub auth_subject: String,
    pub resource: ResourceId,
    pub operations: BTreeSet<String>,
    pub expires_at_unix_s: u64,
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GatewayConfig {
    pub gateway_peer_id: String,
    pub policy_expires_at_unix_s: u64,
    pub routes: Vec<LocalRoute>,
    pub grants: Vec<ResourceGrant>,
}
impl GatewayConfig {
    pub fn validate_at(&self, now: u64) -> Result<()> {
        identifier(&self.gateway_peer_id)?;
        ensure!(
            now < self.policy_expires_at_unix_s
                && self.policy_expires_at_unix_s.saturating_sub(now) <= 300,
            "gateway policy expired or exceeds freshness bound"
        );
        ensure!(
            self.routes.len() <= MAX_ROUTES && self.grants.len() <= 512,
            "gateway configuration exceeds bounds"
        );
        let mut resources = BTreeSet::new();
        for route in &self.routes {
            route.lease.validate_at(now)?;
            route.lease.require_owner(&self.gateway_peer_id)?;
            identifier(&route.backend_producer)?;
            ensure!(
                resources.insert(route.lease.resource.clone()),
                "duplicate local gateway resource"
            );
        }
        let mut grants = BTreeSet::new();
        for grant in &self.grants {
            identifier(&grant.caller_producer)?;
            ensure!(
                !grant.auth_subject.is_empty() && grant.auth_subject.len() <= 512,
                "gateway grant needs authenticated subject"
            );
            grant.resource.validate()?;
            operations(&grant.operations)?;
            ensure!(
                now < grant.expires_at_unix_s
                    && grant.expires_at_unix_s <= self.policy_expires_at_unix_s,
                "gateway grant exceeds policy validity"
            );
            ensure!(
                resources.contains(&grant.resource),
                "grant names unknown resource"
            );
            for operation in &grant.operations {
                ensure!(
                    grants.insert((
                        grant.caller_producer.clone(),
                        grant.resource.clone(),
                        operation.clone()
                    )),
                    "ambiguous gateway grant"
                );
            }
        }
        Ok(())
    }
    fn resolve(
        &self,
        source: &str,
        subject: &str,
        invoke: &GatewayInvoke,
        now: u64,
    ) -> Result<&LocalRoute> {
        invoke.validate()?;
        ensure!(
            now < self.policy_expires_at_unix_s,
            "gateway policy is stale"
        );
        // Check access before exposing whether a resource exists.
        ensure!(
            self.grants.iter().any(|g| g.caller_producer == source
                && g.auth_subject == subject
                && g.resource == invoke.resource
                && g.operations.contains(&invoke.operation)
                && now < g.expires_at_unix_s),
            "gateway access denied"
        );
        let route = self
            .routes
            .iter()
            .find(|r| r.lease.resource == invoke.resource)
            .context("gateway resource unavailable")?;
        route.lease.validate_at(now)?;
        ensure!(
            route.lease.generation == invoke.generation,
            "gateway resource generation changed"
        );
        ensure!(
            route.lease.operations.contains(&invoke.operation),
            "operation unsupported by adapter"
        );
        Ok(route)
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GatewayDispatch {
    pub caller_producer: String,
    pub invocation: GatewayInvoke,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GatewayResult {
    pub caller_producer: String,
    pub request_id: String,
    pub resource: ResourceId,
    pub generation: u64,
    pub output: serde_json::Value,
}
struct Pending {
    invocation: GatewayInvoke,
    subject: String,
    backend: String,
    expires_at_ms: u64,
    retain_until_ms: u64,
    completed: bool,
}

pub struct GatewayRouter {
    config: GatewayConfig,
    pending: Mutex<BTreeMap<(String, String), Pending>>,
}
impl GatewayRouter {
    pub fn new(config: GatewayConfig, now: u64) -> Result<Self> {
        config.validate_at(now)?;
        Ok(Self {
            config,
            pending: Mutex::new(BTreeMap::new()),
        })
    }
}
impl HubPlugin for GatewayRouter {
    fn handle(
        &self,
        hub: &HubHandle,
        source: &str,
        envelope: &Envelope,
        payload: &[u8],
    ) -> Result<bool> {
        if !matches!(
            envelope.message.kind.as_str(),
            GATEWAY_INVOKE | GATEWAY_RESULT
        ) {
            return Ok(false);
        }
        ensure!(
            source == envelope.producer_id && payload.is_empty() && envelope.payload_bytes == 0,
            "invalid gateway frame source or payload"
        );
        let now_ms = unibus::envelope::now_ms();
        envelope.validate(hub.installation_id(), now_ms)?;
        let mut pending = self
            .pending
            .lock()
            .map_err(|_| anyhow::anyhow!("gateway request state unavailable"))?;
        pending.retain(|_, p| now_ms < p.retain_until_ms);
        if envelope.message.kind == GATEWAY_INVOKE {
            let invocation: GatewayInvoke =
                serde_json::from_value(serde_json::Value::Object(envelope.message.body.clone()))?;
            let identity = hub
                .grant_of(source)
                .context("gateway caller is not registered")?;
            let route =
                self.config
                    .resolve(source, &identity.auth_subject, &invocation, now_ms / 1000)?;
            ensure!(
                hub.connection(&route.backend_producer).is_some(),
                "gateway adapter disconnected"
            );
            let key = (source.to_owned(), invocation.request_id.clone());
            ensure!(
                pending.len() < MAX_RETAINED_IDS
                    && pending
                        .values()
                        .filter(|p| !p.completed && now_ms < p.expires_at_ms)
                        .count()
                        < MAX_PENDING
                    && !pending.contains_key(&key),
                "gateway request capacity or duplicate request"
            );
            let dispatch = GatewayDispatch {
                caller_producer: source.into(),
                invocation: invocation.clone(),
            };
            let mut forwarded = envelope.clone();
            forwarded.token.clear();
            forwarded.destination = Some(route.backend_producer.clone());
            forwarded.message = Message::new(GATEWAY_INVOKE, serde_json::to_value(dispatch)?);
            // Keep original authenticated source; backend cannot select identity.
            let expires_at_ms = (now_ms + 5000)
                .min(
                    envelope
                        .issued_at_unix_ms
                        .saturating_add(envelope.ttl_ms.unwrap_or(unibus::DEFAULT_TTL_MS)),
                )
                .min(route.lease.expires_at_unix_s.saturating_mul(1000));
            pending.insert(
                key.clone(),
                Pending {
                    invocation,
                    subject: identity.auth_subject,
                    backend: route.backend_producer.clone(),
                    expires_at_ms,
                    retain_until_ms: self.config.policy_expires_at_unix_s.saturating_mul(1000),
                    completed: false,
                },
            );
            drop(pending);
            if let Err(error) = hub.send_to_producer(&route.backend_producer, &forwarded, &[]) {
                // Delivery may be uncertain. Retain the ID to avoid re-executing.
                self.pending
                    .lock()
                    .map_err(|_| anyhow::anyhow!("gateway request state unavailable"))?
                    .get_mut(&key)
                    .context("gateway request lost")?
                    .completed = true;
                return Err(error);
            }
        } else {
            ensure!(
                serde_json::to_vec(&envelope.message.body)?.len() <= 65536,
                "gateway result exceeds bound"
            );
            let result: GatewayResult =
                serde_json::from_value(serde_json::Value::Object(envelope.message.body.clone()))?;
            let key = (result.caller_producer.clone(), result.request_id.clone());
            let request = pending
                .get(&key)
                .context("gateway request unknown or expired")?;
            ensure!(
                !request.completed && now_ms < request.expires_at_ms,
                "gateway request completed or expired"
            );
            ensure!(
                request.backend == source
                    && result.resource == request.invocation.resource
                    && result.generation == request.invocation.generation,
                "gateway result does not match adapter and resource"
            );
            self.config.resolve(
                &result.caller_producer,
                &request.subject,
                &request.invocation,
                now_ms / 1000,
            )?;
            let current = hub
                .grant_of(&result.caller_producer)
                .context("gateway caller grant removed")?;
            ensure!(
                current.auth_subject == request.subject,
                "gateway caller identity changed"
            );
            let mut forwarded = envelope.clone();
            forwarded.token.clear();
            forwarded.destination = Some(result.caller_producer.clone());
            pending
                .get_mut(&key)
                .context("gateway request lost")?
                .completed = true;
            drop(pending);
            hub.send_to_producer(&result.caller_producer, &forwarded, &[])?;
        }
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{NodeConfig, NodeRegistry, Sink};
    use std::{collections::HashMap, sync::Arc};
    use unibus::{
        IrohPeerEndpoint, PeerCandidate,
        gateway::{NativeSecurity, Protection, ResourceScope},
    };
    #[derive(Default)]
    struct RecordingSink(Mutex<Vec<Envelope>>);
    impl Sink for RecordingSink {
        fn push(&self, envelope: &Envelope, _: &[u8]) -> Result<()> {
            self.0.lock().unwrap().push(envelope.clone());
            Ok(())
        }
    }
    fn configuration(now: u64) -> GatewayConfig {
        let resource = ResourceId {
            scope: ResourceScope::Peer("host".into()),
            key: "canvas-tv".into(),
        };
        GatewayConfig {
            gateway_peer_id: "host".into(),
            policy_expires_at_unix_s: now + 60,
            routes: vec![LocalRoute {
                backend_producer: "canvas".into(),
                lease: GatewayLease {
                    schema_version: 1,
                    resource: resource.clone(),
                    gateway_peer_id: "host".into(),
                    source_service_id: "canvas-dcp".into(),
                    generation: 1,
                    observed_at_unix_s: now,
                    expires_at_unix_s: now + 60,
                    operations: BTreeSet::from(["canvas.control".into()]),
                    native_security: NativeSecurity {
                        encryption: Protection::Unknown,
                        authentication: Protection::Present,
                    },
                    endpoint: PeerCandidate {
                        peer_id: "unibus-local".into(),
                        site: None,
                        endpoints: vec![],
                        iroh_endpoints: vec![IrohPeerEndpoint {
                            endpoint_id: "ab".repeat(32),
                            direct_addresses: vec!["127.0.0.1:5000".parse().unwrap()],
                            relay_urls: vec![],
                        }],
                        expires_at_unix_s: now + 60,
                    },
                },
            }],
            grants: vec![ResourceGrant {
                caller_producer: "caller".into(),
                auth_subject: "oidc-subject".into(),
                resource,
                operations: BTreeSet::from(["canvas.control".into()]),
                expires_at_unix_s: now + 60,
            }],
        }
    }
    fn invocation(config: &GatewayConfig) -> GatewayInvoke {
        GatewayInvoke {
            schema_version: 1,
            request_id: "request-1".into(),
            resource: config.routes[0].lease.resource.clone(),
            generation: 1,
            operation: "canvas.control".into(),
            input: serde_json::json!({"action":"list"}),
        }
    }
    #[test]
    fn exact_identity_resource_operation_and_freshness_are_required() {
        let config = configuration(100);
        let mut invoke = invocation(&config);
        assert!(
            config
                .resolve("caller", "oidc-subject", &invoke, 100)
                .is_ok()
        );
        for (producer, subject, now) in [
            ("other", "oidc-subject", 100),
            ("caller", "forged", 100),
            ("caller", "oidc-subject", 160),
        ] {
            assert!(config.resolve(producer, subject, &invoke, now).is_err());
        }
        invoke.generation = 2;
        assert!(
            config
                .resolve("caller", "oidc-subject", &invoke, 100)
                .is_err()
        );
        invoke.generation = 1;
        invoke.operation = "canvas.close".into();
        assert!(
            config
                .resolve("caller", "oidc-subject", &invoke, 100)
                .is_err()
        );
    }
    #[test]
    fn duplicate_routes_and_empty_subjects_fail_configuration() {
        let mut config = configuration(100);
        config.routes.push(config.routes[0].clone());
        assert!(config.validate_at(100).is_err());
        config.routes.pop();
        config.grants[0].auth_subject.clear();
        assert!(config.validate_at(100).is_err());
    }
    #[test]
    fn local_app_round_trip_denies_spoofed_backend_and_replay() {
        let now = unibus::envelope::now_ms() / 1000;
        let config = configuration(now);
        let invoke = invocation(&config);
        let caller = Arc::new(RecordingSink::default());
        let backend = Arc::new(RecordingSink::default());
        let grant:NodeConfig=serde_json::from_value(serde_json::json!({"producer_id":"caller","capabilities":[],"auth_subject":"oidc-subject"})).unwrap();
        let hub = HubHandle {
            installation_id: "test".into(),
            grants: Arc::new(HashMap::from([("caller".into(), grant)])),
            registry: Arc::new(Mutex::new(NodeRegistry::default())),
            conns: Arc::new(Mutex::new(HashMap::from([
                ("caller".into(), caller.clone() as Arc<dyn Sink>),
                ("canvas".into(), backend.clone() as Arc<dyn Sink>),
            ]))),
        };
        let mut router = GatewayRouter::new(config, now).unwrap();
        let request = Envelope::new(
            "test",
            "caller",
            None,
            "",
            Message::new(GATEWAY_INVOKE, serde_json::to_value(&invoke).unwrap()),
        );
        assert!(router.handle(&hub, "caller", &request, &[]).unwrap());
        assert_eq!(
            backend.0.lock().unwrap()[0].destination.as_deref(),
            Some("canvas")
        );
        assert!(router.handle(&hub, "caller", &request, &[]).is_err());
        let result = GatewayResult {
            caller_producer: "caller".into(),
            request_id: invoke.request_id.clone(),
            resource: invoke.resource,
            generation: 1,
            output: serde_json::json!({"workspaces":["home"]}),
        };
        let mut response = Envelope::new(
            "test",
            "intruder",
            None,
            "",
            Message::new(GATEWAY_RESULT, serde_json::to_value(&result).unwrap()),
        );
        assert!(router.handle(&hub, "intruder", &response, &[]).is_err());
        response.producer_id = "canvas".into();
        assert!(router.handle(&hub, "canvas", &response, &[]).unwrap());
        assert_eq!(caller.0.lock().unwrap().len(), 1);
        assert!(router.handle(&hub, "canvas", &response, &[]).is_err());
        assert!(router.handle(&hub, "caller", &request, &[]).is_err());
        let mut second_request = request.clone();
        second_request
            .message
            .body
            .insert("request_id".into(), "request-2".into());
        assert!(router.handle(&hub, "caller", &second_request, &[]).unwrap());
        response
            .message
            .body
            .insert("request_id".into(), "request-2".into());
        router.config.policy_expires_at_unix_s = unibus::envelope::now_ms() / 1000;
        assert!(router.handle(&hub, "canvas", &response, &[]).is_err());
        assert_eq!(caller.0.lock().unwrap().len(), 1);
    }
}
