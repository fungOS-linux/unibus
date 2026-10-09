//! Explicit local-app routing in the existing daemon, not LAN discovery.
use crate::{HubHandle, HubPlugin};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex, RwLock},
};
use unibus::{
    Envelope, Message,
    gateway::{
        GATEWAY_INVOKE, GATEWAY_REGISTER, GATEWAY_RELEASE, GATEWAY_RESULT, GatewayInvoke,
        GatewayLease, GatewayRegistration, GatewayRelease, ResourceId, identifier, operations,
    },
};

const MAX_ROUTES: usize = 128;
const MAX_PENDING: usize = 256;
const MAX_RETAINED_IDS: usize = 4096;
#[cfg(feature = "owner-catalog")]
pub mod catalog;
pub mod generations;

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

pub use unibus::gateway::{GatewayDispatch, GatewayResult};
struct Pending {
    invocation: GatewayInvoke,
    subject: String,
    backend: String,
    expires_at_ms: u64,
    retain_until_ms: u64,
    completed: bool,
}

pub struct GatewayRouter {
    config: RwLock<GatewayConfig>,
    permits: BTreeMap<ResourceId, RegistrationPermit>,
    floors: Mutex<BTreeMap<ResourceId, u64>>,
    registered: RwLock<BTreeSet<ResourceId>>,
    pending: Mutex<BTreeMap<(String, String), Pending>>,
    publisher: Option<Arc<dyn GatewayCatalogSink>>,
    publication: Mutex<()>,
    generation_store: Option<Arc<dyn GenerationStore>>,
}
struct RegistrationPermit {
    backend: String,
    service: String,
    producer: String,
    operations: BTreeSet<String>,
}
/// Optional local publication seam. Never grants access or proxies traffic.
pub trait GatewayCatalogSink: Send + Sync {
    fn publish(&self, leases: &[GatewayLease], now: u64) -> Result<()>;
}
pub trait GenerationStore: Send + Sync {
    fn load(&self, owner: &str) -> Result<BTreeMap<ResourceId, u64>>;
    fn persist(&self, owner: &str, floors: &BTreeMap<ResourceId, u64>) -> Result<()>;
}
impl GatewayRouter {
    pub fn gateway_peer_id(&self) -> Result<String> {
        Ok(self
            .config
            .read()
            .map_err(|_| anyhow::anyhow!("gateway policy state unavailable"))?
            .gateway_peer_id
            .clone())
    }
    pub fn new(config: GatewayConfig, now: u64) -> Result<Self> {
        config.validate_at(now)?;
        let permits = config
            .routes
            .iter()
            .map(|r| {
                (
                    r.lease.resource.clone(),
                    RegistrationPermit {
                        backend: r.backend_producer.clone(),
                        service: r.lease.source_service_id.clone(),
                        producer: r.lease.endpoint.peer_id.clone(),
                        operations: r.lease.operations.clone(),
                    },
                )
            })
            .collect();
        let floors = config
            .routes
            .iter()
            .map(|r| (r.lease.resource.clone(), r.lease.generation))
            .collect();
        Ok(Self {
            config: RwLock::new(config),
            permits,
            floors: Mutex::new(floors),
            registered: RwLock::new(BTreeSet::new()),
            pending: Mutex::new(BTreeMap::new()),
            publisher: None,
            publication: Mutex::new(()),
            generation_store: None,
        })
    }
    pub fn with_generation_store(mut self, store: Arc<dyn GenerationStore>) -> Result<Self> {
        let config = self
            .config
            .get_mut()
            .map_err(|_| anyhow::anyhow!("gateway policy unavailable"))?;
        let floors = self
            .floors
            .get_mut()
            .map_err(|_| anyhow::anyhow!("gateway generations unavailable"))?;
        for (resource, generation) in store.load(&config.gateway_peer_id)? {
            floors
                .entry(resource)
                .and_modify(|floor| *floor = (*floor).max(generation))
                .or_insert(generation);
        }
        ensure!(
            floors.len() <= MAX_ROUTES,
            "retained gateway generations exceed bound"
        );
        // Keep declared permits but never resurrect retired routes from startup config.
        config
            .routes
            .retain(|route| route.lease.generation >= floors[&route.lease.resource]);
        store.persist(&config.gateway_peer_id, floors)?;
        self.generation_store = Some(store);
        Ok(self)
    }
    fn persist_generations(&self, owner: &str, floors: &BTreeMap<ResourceId, u64>) -> Result<()> {
        if let Some(store) = &self.generation_store {
            store.persist(owner, floors)?;
        }
        Ok(())
    }
    pub fn with_catalog_sink(mut self, sink: Arc<dyn GatewayCatalogSink>) -> Self {
        self.publisher = Some(sink);
        self
    }
    pub fn active_leases(&self, now: u64) -> Result<Vec<GatewayLease>> {
        let config = self
            .config
            .read()
            .map_err(|_| anyhow::anyhow!("gateway policy state unavailable"))?;
        if now >= config.policy_expires_at_unix_s {
            return Ok(vec![]);
        }
        let registered = self
            .registered
            .read()
            .map_err(|_| anyhow::anyhow!("gateway registrations unavailable"))?;
        Ok(config
            .routes
            .iter()
            .filter(|r| registered.contains(&r.lease.resource) && r.lease.validate_at(now).is_ok())
            .map(|r| {
                let mut lease = r.lease.clone();
                lease.expires_at_unix_s =
                    lease.expires_at_unix_s.min(config.policy_expires_at_unix_s);
                lease
            })
            .collect())
    }
    pub fn publish_catalog(&self, now: u64) -> Result<()> {
        if let Some(publisher) = &self.publisher {
            let _serial = self
                .publication
                .lock()
                .map_err(|_| anyhow::anyhow!("gateway publication state unavailable"))?;
            publisher.publish(&self.active_leases(now)?, now)?;
        }
        Ok(())
    }
    fn register(&self, source: &str, lease: GatewayLease, now: u64) -> Result<()> {
        lease.validate_at(now)?;
        let permit = self
            .permits
            .get(&lease.resource)
            .context("adapter has no declared resource registration permit")?;
        ensure!(
            permit.backend == source
                && permit.service == lease.source_service_id
                && permit.producer == lease.endpoint.peer_id
                && lease.operations.is_subset(&permit.operations),
            "adapter registration exceeds declared resource binding"
        );
        let mut config = self
            .config
            .write()
            .map_err(|_| anyhow::anyhow!("gateway policy state unavailable"))?;
        lease.require_owner(&config.gateway_peer_id)?;
        ensure!(
            now < config.policy_expires_at_unix_s
                && lease.expires_at_unix_s <= config.policy_expires_at_unix_s,
            "registration cannot renew or outlive access policy"
        );
        let mut floors = self
            .floors
            .lock()
            .map_err(|_| anyhow::anyhow!("gateway generations unavailable"))?;
        ensure!(
            lease.generation
                >= *floors
                    .get(&lease.resource)
                    .context("missing gateway generation floor")?,
            "retired gateway generation"
        );
        if let Some(route) = config
            .routes
            .iter_mut()
            .find(|r| r.lease.resource == lease.resource)
            && route.lease != lease
        {
            ensure!(
                lease.observed_at_unix_s > route.lease.observed_at_unix_s,
                "stale or conflicting gateway renewal"
            );
            if lease.generation == route.lease.generation {
                ensure!(
                    lease.endpoint.endpoints == route.lease.endpoint.endpoints
                        && lease.endpoint.iroh_endpoints == route.lease.endpoint.iroh_endpoints
                        && lease.operations == route.lease.operations
                        && lease.native_security == route.lease.native_security,
                    "changed resource requires a new generation"
                );
            }
        }
        let mut next_floors = floors.clone();
        next_floors.insert(lease.resource.clone(), lease.generation);
        self.persist_generations(&config.gateway_peer_id, &next_floors)?;
        if let Some(route) = config
            .routes
            .iter_mut()
            .find(|r| r.lease.resource == lease.resource)
        {
            route.lease = lease.clone();
        } else {
            config.routes.push(LocalRoute {
                lease: lease.clone(),
                backend_producer: source.into(),
            });
        }
        *floors = next_floors;
        self.registered
            .write()
            .map_err(|_| anyhow::anyhow!("gateway registrations unavailable"))?
            .insert(lease.resource.clone());
        drop(floors);
        drop(config);
        self.publish_catalog(now)
    }
    fn release(&self, source: &str, release: GatewayRelease, now: u64) -> Result<()> {
        let permit = self
            .permits
            .get(&release.resource)
            .context("unknown registration permit")?;
        ensure!(permit.backend == source, "adapter does not own resource");
        let mut config = self
            .config
            .write()
            .map_err(|_| anyhow::anyhow!("gateway policy state unavailable"))?;
        let route = config
            .routes
            .iter()
            .find(|r| r.lease.resource == release.resource)
            .context("resource already released")?;
        ensure!(
            route.lease.generation == release.generation,
            "release generation mismatch"
        );
        let next = release
            .generation
            .checked_add(1)
            .context("resource generation exhausted")?;
        let mut floors = self
            .floors
            .lock()
            .map_err(|_| anyhow::anyhow!("gateway generations unavailable"))?;
        let mut next_floors = floors.clone();
        next_floors.insert(release.resource.clone(), next);
        self.persist_generations(&config.gateway_peer_id, &next_floors)?;
        *floors = next_floors;
        config
            .routes
            .retain(|r| r.lease.resource != release.resource);
        self.registered
            .write()
            .map_err(|_| anyhow::anyhow!("gateway registrations unavailable"))?
            .remove(&release.resource);
        drop(floors);
        drop(config);
        self.publish_catalog(now)
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
            GATEWAY_INVOKE | GATEWAY_RESULT | GATEWAY_REGISTER | GATEWAY_RELEASE
        ) {
            return Ok(false);
        }
        ensure!(
            source == envelope.producer_id && payload.is_empty() && envelope.payload_bytes == 0,
            "invalid gateway frame source or payload"
        );
        let now_ms = unibus::envelope::now_ms();
        envelope.validate(hub.installation_id(), now_ms)?;
        if matches!(
            envelope.message.kind.as_str(),
            GATEWAY_REGISTER | GATEWAY_RELEASE
        ) {
            ensure!(
                serde_json::to_vec(&envelope.message.body)?.len() <= 65536,
                "gateway registration exceeds bound"
            );
            ensure!(
                !hub.grant_of(source)
                    .context("unregistered adapter")?
                    .auth_subject
                    .is_empty(),
                "adapter registration requires authenticated subject"
            );
            let body = serde_json::Value::Object(envelope.message.body.clone());
            if envelope.message.kind == GATEWAY_REGISTER {
                let registration: GatewayRegistration = serde_json::from_value(body)?;
                self.register(source, registration.lease, now_ms / 1000)?;
            } else {
                self.release(source, serde_json::from_value(body)?, now_ms / 1000)?;
            }
            return Ok(true);
        }
        let config = self
            .config
            .read()
            .map_err(|_| anyhow::anyhow!("gateway policy state unavailable"))?;
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
                config.resolve(source, &identity.auth_subject, &invocation, now_ms / 1000)?;
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
                    retain_until_ms: config.policy_expires_at_unix_s.saturating_mul(1000),
                    completed: false,
                },
            );
            drop(pending);
            let backend = route.backend_producer.clone();
            drop(config);
            if let Err(error) = hub.send_to_producer(&backend, &forwarded, &[]) {
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
            config.resolve(
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
            drop(config);
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
    fn durable_release_cannot_resurrect_from_startup_config() {
        let directory = tempfile::tempdir().unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700))
                .unwrap();
        }
        let path = directory.path().join("generations.json");
        let lease = configuration(100).routes[0].lease.clone();
        let router = GatewayRouter::new(configuration(100), 100)
            .unwrap()
            .with_generation_store(Arc::new(
                generations::GenerationFile::open(path.clone()).unwrap(),
            ))
            .unwrap();
        router.register("canvas", lease.clone(), 100).unwrap();
        router
            .release(
                "canvas",
                GatewayRelease {
                    resource: lease.resource.clone(),
                    generation: 1,
                },
                100,
            )
            .unwrap();
        drop(router);
        let router = GatewayRouter::new(configuration(100), 100)
            .unwrap()
            .with_generation_store(Arc::new(generations::GenerationFile::open(path).unwrap()))
            .unwrap();
        assert!(router.config.read().unwrap().routes.is_empty());
        assert!(router.register("canvas", lease.clone(), 100).is_err());
        let mut rebound = lease;
        rebound.generation = 2;
        router.register("canvas", rebound, 100).unwrap();
        assert_eq!(router.active_leases(100).unwrap().len(), 1);
    }
    #[test]
    fn failed_persistence_cannot_acknowledge_a_generation_change() {
        struct BrokenStore;
        impl GenerationStore for BrokenStore {
            fn load(&self, _: &str) -> Result<BTreeMap<ResourceId, u64>> {
                Ok(BTreeMap::new())
            }
            fn persist(&self, _: &str, _: &BTreeMap<ResourceId, u64>) -> Result<()> {
                anyhow::bail!("storage unavailable")
            }
        }
        assert!(
            GatewayRouter::new(configuration(100), 100)
                .unwrap()
                .with_generation_store(Arc::new(BrokenStore))
                .is_err()
        );
        let mut router = GatewayRouter::new(configuration(100), 100).unwrap();
        router.generation_store = Some(Arc::new(BrokenStore));
        let mut lease = configuration(100).routes[0].lease.clone();
        lease.generation = 2;
        lease.observed_at_unix_s = 101;
        assert!(router.register("canvas", lease.clone(), 101).is_err());
        assert_eq!(router.config.read().unwrap().routes[0].lease.generation, 1);
        assert_eq!(router.floors.lock().unwrap()[&lease.resource], 1);
        assert!(
            router
                .release(
                    "canvas",
                    GatewayRelease {
                        resource: lease.resource,
                        generation: 1
                    },
                    101
                )
                .is_err()
        );
        assert_eq!(router.config.read().unwrap().routes.len(), 1);
    }
    #[test]
    fn renewal_release_and_rebinding_do_not_create_permissions() {
        let config = configuration(100);
        let mut lease = config.routes[0].lease.clone();
        let router = GatewayRouter::new(config, 100).unwrap();
        lease.observed_at_unix_s = 101;
        assert!(router.register("intruder", lease.clone(), 101).is_err());
        router.register("canvas", lease.clone(), 101).unwrap();
        let mut expanded = lease.clone();
        expanded.operations.insert("canvas.admin".into());
        expanded.generation = 2;
        expanded.observed_at_unix_s = 102;
        assert!(router.register("canvas", expanded, 102).is_err());
        router
            .release(
                "canvas",
                GatewayRelease {
                    resource: lease.resource.clone(),
                    generation: 1,
                },
                103,
            )
            .unwrap();
        assert!(router.active_leases(103).unwrap().is_empty());
        lease.observed_at_unix_s = 104;
        assert!(router.register("canvas", lease.clone(), 104).is_err());
        lease.generation = 2;
        router.register("canvas", lease.clone(), 104).unwrap();
        lease.expires_at_unix_s = 161;
        lease.endpoint.expires_at_unix_s = 161;
        lease.observed_at_unix_s = 105;
        assert!(router.register("canvas", lease, 105).is_err());
        assert!(router.active_leases(160).unwrap().is_empty());
    }
    #[cfg(feature = "owner-catalog")]
    #[test]
    fn published_lease_is_consumed_through_provider_neutral_directory() {
        let config = configuration(100);
        let resource = config.routes[0].lease.resource.clone();
        let lease = config.routes[0].lease.clone();
        let router = GatewayRouter::new(config, 100).unwrap();
        assert!(router.active_leases(100).unwrap().is_empty());
        router.register("canvas", lease, 100).unwrap();
        let catalog =
            super::catalog::catalog("host", &router.active_leases(100).unwrap(), 100).unwrap();
        if let Some(path) = std::env::var_os("FPL_TEST_OWNER_CATALOG_FILE") {
            let publisher =
                super::catalog::OwnerCatalogFile::new(path.into(), "host".into()).unwrap();
            publisher
                .publish(&router.active_leases(100).unwrap(), 100)
                .unwrap();
        }
        let observations = catalog
            .observations
            .into_iter()
            .map(|o| unibus::ServiceObservation {
                service_id: o.service_id.0,
                kind: o.value.kind,
                endpoints: o.value.endpoints.into_iter().collect(),
                observed_at_unix_s: o.observed_at,
                expires_at_unix_s: o.expires_at,
                provenance_provider: o.provenance.provider,
                provenance_observer: o.provenance.observer,
                attributes: o.value.attributes,
            })
            .collect::<Vec<_>>();
        let directory = unibus::gateway::directory::GatewayDirectory::new(
            unibus::StaticServiceDirectory::new(observations.clone()).unwrap(),
            BTreeMap::from([("host".into(), "unibus-local".into())]),
        );
        assert_eq!(directory.gateways(&resource, 100).unwrap().len(), 1);
        assert!(directory.gateways(&resource, 160).unwrap().is_empty());
        let mut forged = observations;
        forged[0].provenance_observer = "other".into();
        let directory = unibus::gateway::directory::GatewayDirectory::new(
            unibus::StaticServiceDirectory::new(forged).unwrap(),
            BTreeMap::from([("host".into(), "unibus-local".into())]),
        );
        assert!(directory.gateways(&resource, 100).is_err());
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
        let router = GatewayRouter::new(config, now).unwrap();
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
        router.config.write().unwrap().policy_expires_at_unix_s = unibus::envelope::now_ms() / 1000;
        assert!(router.handle(&hub, "canvas", &response, &[]).is_err());
        assert_eq!(caller.0.lock().unwrap().len(), 1);
    }
}
