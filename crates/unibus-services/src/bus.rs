//! Optional catalog transport over the existing bounded SDK. The router stays
//! body-agnostic; retention and provenance checks belong to the consumer.

use std::{
    collections::BTreeMap,
    net::SocketAddr,
    path::PathBuf,
    thread,
    time::{Duration, Instant},
};

use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use stream_descriptors::service::{
    SERVICE_CATALOG_KIND, SERVICE_HEALTH_KIND, ServiceCatalog, ServiceHealth, ServiceInventory,
};
use unibus::client::ClientConfig;
use unibus_sdk::{Event, Publish, Session};

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceBinding {
    pub producer_id: String,
    pub service_id: String,
    pub presenter_reference: String,
    pub owner_reference: String,
}

impl SourceBinding {
    fn validate(&self) -> Result<()> {
        ensure!(
            unibus::grants::is_valid_slug(&self.producer_id),
            "invalid source producer"
        );
        // Reuse contract validation rather than a second identifier registry.
        ServiceCatalog {
            contract: SERVICE_CATALOG_KIND.into(),
            service_id: self.service_id.clone(),
            adapter_kind: "binding".into(),
            owner_reference: self.owner_reference.clone(),
            presenter_reference: self.presenter_reference.clone(),
            observed_at_unix_ms: 1,
            valid_until_unix_ms: 2,
            resources: vec![],
        }
        .validate()
        .map_err(anyhow::Error::msg)
    }

    fn matches(&self, catalog: &ServiceCatalog) -> bool {
        catalog.service_id == self.service_id
            && catalog.presenter_reference == self.presenter_reference
            && catalog.owner_reference == self.owner_reference
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublicationConfig {
    pub bus: ClientConfig,
    pub source: String,
    pub bearer_token_file: Option<PathBuf>,
    pub binding: SourceBinding,
    pub interval_ms: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WatchConfig {
    pub bus: ClientConfig,
    pub sources: Vec<SourceBinding>,
}

fn validate_bus(bus: &ClientConfig) -> Result<()> {
    let endpoint: SocketAddr = bus.hub_endpoint.parse()?;
    ensure!(
        endpoint.ip().is_loopback() && bus.schema_version == 2,
        "catalog bus requires schema v2 on loopback; tunnel remote access privately"
    );
    Ok(())
}

impl PublicationConfig {
    pub fn validate(&self) -> Result<()> {
        validate_bus(&self.bus)?;
        super::validate_source(&self.source)?;
        self.binding.validate()?;
        ensure!(
            self.binding.producer_id == self.bus.producer_id,
            "publisher binding must match authenticated bus producer"
        );
        ensure!(
            (1000..=30_000).contains(&self.interval_ms),
            "publication interval must be 1000..30000 ms"
        );
        Ok(())
    }
}

/// Ingestion is valid only for events read from an authenticated SDK session.
/// Exact producer/service/owner/presenter bindings prevent cross-target poison.
pub struct BusInventory {
    bindings: BTreeMap<(String, String), SourceBinding>,
    inventory: ServiceInventory,
    pending_health: BTreeMap<(String, String), ServiceHealth>,
}

#[derive(Serialize)]
pub struct InventoryRow<'a> {
    pub catalog: &'a ServiceCatalog,
    pub health: &'a Option<ServiceHealth>,
    pub effective_state: stream_descriptors::service::HealthState,
}

impl BusInventory {
    pub fn new(bindings: Vec<SourceBinding>) -> Result<Self> {
        ensure!(
            !bindings.is_empty() && bindings.len() <= 256,
            "expected 1..256 source bindings"
        );
        let mut indexed = BTreeMap::new();
        let mut routes = std::collections::BTreeSet::new();
        let mut producers = std::collections::BTreeSet::new();
        for binding in bindings {
            binding.validate()?;
            ensure!(
                producers.insert(binding.producer_id.clone()),
                "one service catalog per producer is required by opaque snapshot coalescing"
            );
            ensure!(
                routes.insert((
                    binding.presenter_reference.clone(),
                    binding.service_id.clone()
                )),
                "ambiguous presenter binding"
            );
            ensure!(
                indexed
                    .insert(
                        (binding.producer_id.clone(), binding.service_id.clone()),
                        binding
                    )
                    .is_none(),
                "duplicate producer binding"
            );
        }
        Ok(Self {
            bindings: indexed,
            inventory: ServiceInventory::default(),
            pending_health: BTreeMap::new(),
        })
    }

    pub fn observe(&mut self, event: &Event, now: u64) -> Result<bool> {
        // A fleet-wide route can deliver unselected sources. Filtering those
        // is expected; malformed evidence from a selected source is an error.
        if !self
            .bindings
            .keys()
            .any(|(producer, _)| producer == &event.producer_id)
        {
            return Ok(false);
        }
        match event.kind.as_str() {
            SERVICE_CATALOG_KIND => {
                let catalog: ServiceCatalog = serde_json::from_value(event.body.clone())?;
                let key = (event.producer_id.clone(), catalog.service_id.clone());
                let Some(binding) = self.bindings.get(&key) else {
                    return Ok(false);
                };
                ensure!(binding.matches(&catalog), "catalog provenance mismatch");
                self.inventory
                    .observe_catalog(&binding.presenter_reference, catalog, now)
                    .map_err(anyhow::Error::msg)?;
                if let Some(health) = self.pending_health.remove(&key) {
                    self.inventory
                        .observe_health(&binding.presenter_reference, health, now)
                        .map_err(anyhow::Error::msg)?;
                }
                Ok(true)
            }
            SERVICE_HEALTH_KIND => {
                let health: ServiceHealth = serde_json::from_value(event.body.clone())?;
                let key = (event.producer_id.clone(), health.service_id.clone());
                let Some(binding) = self.bindings.get(&key) else {
                    return Ok(false);
                };
                health.validate().map_err(anyhow::Error::msg)?;
                ensure!(health.observed_at_unix_ms <= now, "future health evidence");
                if !self.inventory.entries().any(|entry| {
                    entry.catalog.service_id == health.service_id
                        && entry.catalog.presenter_reference == binding.presenter_reference
                }) {
                    if let Some(previous) = self.pending_health.get(&key) {
                        ensure!(
                            health.observed_at_unix_ms >= previous.observed_at_unix_ms,
                            "pending health evidence regressed"
                        );
                        ensure!(
                            health.observed_at_unix_ms != previous.observed_at_unix_ms
                                || health == *previous,
                            "pending health evidence conflicts"
                        );
                    }
                    // At most one item per configured binding (<=256), not an
                    // unbounded buffer of arbitrary discovered service IDs.
                    self.pending_health.insert(key, health);
                    return Ok(true);
                }
                self.inventory
                    .observe_health(&binding.presenter_reference, health, now)
                    .map_err(anyhow::Error::msg)?;
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    pub fn rows(&self, now: u64) -> Vec<InventoryRow<'_>> {
        self.inventory
            .entries()
            .map(|entry| InventoryRow {
                catalog: &entry.catalog,
                health: &entry.health,
                effective_state: entry.state(now),
            })
            .collect()
    }
}

/// One probe per configured publisher. Fanout uses existing capability routing.
/// Queue success is not delivery: all publications require bounded receipts.
pub fn publish(config: PublicationConfig) -> Result<()> {
    config.validate()?;
    let session = Session::connect(config.bus)?;
    let mut pending = BTreeMap::<String, Instant>::new();
    let mut observer_idle = None;
    let mut next = Instant::now();
    loop {
        ensure!(
            session.status().connected && session.status().dropped_events == 0,
            "catalog bus disconnected or inbound queue overflowed"
        );
        while let Some(event) = session.poll() {
            if unibus::kinds::classify(&event.kind) == unibus::kinds::Control::Reply {
                let request = event.body["request_id"].as_str().unwrap_or_default();
                if pending.remove(request).is_some() {
                    let idle = event.body["success"] != true
                        && event.body["message"] == unibus::kinds::REPLY_NO_ELIGIBLE_NODES;
                    ensure!(
                        event.body["success"] == true || idle,
                        "catalog publication rejected by router"
                    );
                    if observer_idle != Some(idle) {
                        eprintln!(
                            "catalog publication observers={}",
                            if idle {
                                "idle (none delivered)"
                            } else {
                                "active"
                            }
                        );
                        observer_idle = Some(idle);
                    }
                }
            }
        }
        ensure!(
            pending
                .values()
                .all(|sent| sent.elapsed() < Duration::from_secs(10)),
            "catalog publication receipt timed out"
        );
        if Instant::now() >= next {
            // Reconnect/retry is supervisor-owned. A failed probe exits loudly;
            // previous producer evidence expires rather than being renewed.
            let inspection =
                super::inspect_authorized(&config.source, config.bearer_token_file.as_deref())?;
            let Some(catalog) = inspection.catalog else {
                anyhow::bail!("source catalog is stale; refusing to renew it");
            };
            ensure!(
                config.binding.matches(&catalog),
                "source does not match configured owner/presenter/service"
            );
            for (kind, body) in [
                (SERVICE_CATALOG_KIND, serde_json::to_value(catalog)?),
                (
                    SERVICE_HEALTH_KIND,
                    serde_json::to_value(inspection.health)?,
                ),
            ] {
                ensure!(pending.len() < 32, "publication receipt backlog full");
                let request = session.publish(Publish {
                    destination: None,
                    kind: kind.into(),
                    body,
                })?;
                pending.insert(request, Instant::now());
            }
            next = Instant::now() + Duration::from_millis(config.interval_ms);
        }
        thread::sleep(Duration::from_millis(10));
    }
}

pub fn watch(config: WatchConfig, duration_ms: u64) -> Result<String> {
    validate_bus(&config.bus)?;
    ensure!(
        (100..=60_000).contains(&duration_ms),
        "watch duration must be 100..60000 ms"
    );
    let mut inventory = BusInventory::new(config.sources)?;
    let session = Session::connect(config.bus)?;
    let until = Instant::now() + Duration::from_millis(duration_ms);
    while Instant::now() < until {
        ensure!(
            session.status().connected && session.status().dropped_events == 0,
            "catalog bus disconnected or inbound queue overflowed"
        );
        while let Some(event) = session.poll() {
            inventory.observe(&event, unibus::envelope::now_ms())?;
        }
        thread::sleep(Duration::from_millis(10));
    }
    let rows = inventory.rows(unibus::envelope::now_ms());
    ensure!(
        !rows.is_empty(),
        "no catalog received for the selected sources"
    );
    Ok(serde_json::to_string_pretty(&rows)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provenance_scope_retention_and_expiry_are_consumer_owned() {
        let binding = SourceBinding {
            producer_id: "pi-catalog".into(),
            service_id: "hdhomerun.12345678".into(),
            presenter_reference: "lan/pi/8088".into(),
            owner_reference: "tls/owner".into(),
        };
        let catalog = ServiceCatalog {
            contract: SERVICE_CATALOG_KIND.into(),
            service_id: binding.service_id.clone(),
            adapter_kind: "hdhomerun".into(),
            owner_reference: binding.owner_reference.clone(),
            presenter_reference: binding.presenter_reference.clone(),
            observed_at_unix_ms: 100,
            valid_until_unix_ms: 200,
            resources: vec![],
        };
        let mut event = Event {
            producer_id: binding.producer_id.clone(),
            destination: None,
            request_id: "1".into(),
            issued_at_unix_ms: 100,
            kind: SERVICE_CATALOG_KIND.into(),
            body: serde_json::to_value(catalog).unwrap(),
        };
        let mut inventory = BusInventory::new(vec![binding.clone()]).unwrap();
        assert!(BusInventory::new(vec![binding.clone(), binding]).is_err());
        let early_health = Event {
            producer_id: "pi-catalog".into(),
            destination: None,
            request_id: "early".into(),
            issued_at_unix_ms: 100,
            kind: SERVICE_HEALTH_KIND.into(),
            body: serde_json::to_value(ServiceHealth {
                contract: SERVICE_HEALTH_KIND.into(),
                service_id: "hdhomerun.12345678".into(),
                state: stream_descriptors::service::HealthState::Healthy,
                reason: None,
                observed_at_unix_ms: 100,
                valid_until_unix_ms: 200,
            })
            .unwrap(),
        };
        inventory.observe(&early_health, 150).unwrap();
        assert!(inventory.rows(150).is_empty());
        inventory.observe(&event, 150).unwrap();
        assert_eq!(
            inventory.rows(150)[0].effective_state,
            stream_descriptors::service::HealthState::Healthy
        );
        event.producer_id = "rogue".into();
        assert!(!inventory.observe(&event, 150).unwrap());
        event.producer_id = "pi-catalog".into();
        event.body["owner_reference"] = "tls/other".into();
        assert!(inventory.observe(&event, 150).is_err());
        event.kind = SERVICE_HEALTH_KIND.into();
        event.body = serde_json::to_value(ServiceHealth {
            contract: SERVICE_HEALTH_KIND.into(),
            service_id: "hdhomerun.12345678".into(),
            state: stream_descriptors::service::HealthState::Healthy,
            reason: None,
            observed_at_unix_ms: 100,
            valid_until_unix_ms: 200,
        })
        .unwrap();
        inventory.observe(&event, 150).unwrap();
        assert_eq!(
            inventory.rows(150)[0].effective_state,
            stream_descriptors::service::HealthState::Healthy
        );
        assert_eq!(
            inventory.rows(200)[0].effective_state,
            stream_descriptors::service::HealthState::Stale
        );
        assert!(
            serde_json::to_string(&inventory.rows(150))
                .unwrap()
                .find("token")
                .is_none()
        );
    }
}
