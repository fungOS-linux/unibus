//! Read-only service inventory. Discovery is not authorization or activation.
//! Stream IDs reference the existing v2 descriptor/lease contracts; this module
//! does not redefine media, process lifecycle, or domain command payloads.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

pub const SERVICE_CATALOG_KIND: &str = "unibus.service.catalog.v1";
pub const SERVICE_HEALTH_KIND: &str = "unibus.service.health.v1";
const MAX_LEASE_MS: u64 = 300_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HealthState {
    Healthy,
    Degraded,
    Disconnected,
    AuthenticationRequired,
    Busy,
    Stale,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceHealth {
    pub contract: String,
    pub service_id: String,
    pub state: HealthState,
    /// Non-secret, machine-readable reason, not a raw upstream error.
    pub reason: Option<String>,
    pub observed_at_unix_ms: u64,
    pub valid_until_unix_ms: u64,
}

impl ServiceHealth {
    pub fn validate(&self) -> Result<(), String> {
        if self.contract != SERVICE_HEALTH_KIND {
            return Err("unsupported service health contract".into());
        }
        id(&self.service_id)?;
        if let Some(reason) = &self.reason {
            id(reason)?;
        }
        lease(self.observed_at_unix_ms, self.valid_until_unix_ms)
    }

    /// Future-dated or expired evidence must never retain a healthy state.
    pub fn effective_state(&self, now_unix_ms: u64) -> HealthState {
        if self.validate().is_err()
            || now_unix_ms < self.observed_at_unix_ms
            || now_unix_ms >= self.valid_until_unix_ms
        {
            HealthState::Stale
        } else {
            self.state
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceOperation {
    pub id: String,
    /// Domain-owned typed request contract, not executable code or a URL.
    pub request_kind: String,
    /// Permission required at execution. Advertising it grants nothing.
    pub requires: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceResource {
    pub id: String,
    pub kind: String,
    pub display_name: String,
    /// References to independently published stream.descriptor.v2 documents.
    pub stream_ids: Vec<String>,
    pub operations: Vec<ServiceOperation>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceCatalog {
    pub contract: String,
    pub service_id: String,
    pub adapter_kind: String,
    /// Explicit producer/owner identity, never inferred from a device subnet.
    pub owner_reference: String,
    /// Distinguishes the presenting proxy from the protocol owner.
    pub presenter_reference: String,
    pub observed_at_unix_ms: u64,
    pub valid_until_unix_ms: u64,
    pub resources: Vec<ServiceResource>,
}

impl ServiceCatalog {
    pub fn validate(&self) -> Result<(), String> {
        if self.contract != SERVICE_CATALOG_KIND {
            return Err("unsupported service catalog contract".into());
        }
        for value in [&self.service_id, &self.adapter_kind] {
            id(value)?;
        }
        for reference in [&self.owner_reference, &self.presenter_reference] {
            // References are opaque and resolved by an authorized local client.
            // No credential-bearing URLs or inline tokens belong here.
            super::token("owner/presenter reference", reference, 256, true)?;
        }
        lease(self.observed_at_unix_ms, self.valid_until_unix_ms)?;
        if self.resources.len() > 2048 {
            return Err("too many service resources".into());
        }
        let mut resources = BTreeSet::new();
        let mut streams = BTreeSet::new();
        for resource in &self.resources {
            id(&resource.id)?;
            id(&resource.kind)?;
            if !resources.insert(&resource.id) {
                return Err("duplicate service resource".into());
            }
            if resource.display_name.len() > 256
                || resource.display_name.chars().any(char::is_control)
                || resource.stream_ids.len() > 64
                || resource.operations.len() > 64
            {
                return Err("service resource exceeds bounds".into());
            }
            for stream in &resource.stream_ids {
                id(stream)?;
                if !streams.insert(stream) {
                    return Err("duplicate service stream".into());
                }
            }
            let mut operations = BTreeSet::new();
            for operation in &resource.operations {
                for value in [&operation.id, &operation.request_kind, &operation.requires] {
                    id(value)?;
                }
                if !operations.insert(&operation.id) {
                    return Err("duplicate resource operation".into());
                }
            }
        }
        Ok(())
    }

    pub fn is_current(&self, now_unix_ms: u64) -> bool {
        self.validate().is_ok()
            && now_unix_ms >= self.observed_at_unix_ms
            && now_unix_ms < self.valid_until_unix_ms
    }
}

fn id(value: &str) -> Result<(), String> {
    super::token("service identifier", value, 128, true)
}

fn lease(observed: u64, until: u64) -> Result<(), String> {
    if until <= observed || until - observed > MAX_LEASE_MS {
        return Err("invalid service evidence lease".into());
    }
    Ok(())
}

/// Bounded consumer-side projection. Authenticate the source and authorize
/// publication before ingestion; discovery grants no execution authority.
#[derive(Debug, Default)]
pub struct ServiceInventory {
    entries: BTreeMap<(String, String), InventoryEntry>,
}

#[derive(Debug, Clone)]
pub struct InventoryEntry {
    pub catalog: ServiceCatalog,
    pub health: Option<ServiceHealth>,
}

impl InventoryEntry {
    pub fn state(&self, now_unix_ms: u64) -> HealthState {
        if !self.catalog.is_current(now_unix_ms) {
            HealthState::Stale
        } else {
            self.health.as_ref().map_or(HealthState::Unknown, |health| {
                health.effective_state(now_unix_ms)
            })
        }
    }
}

impl ServiceInventory {
    pub fn observe_catalog(
        &mut self,
        source_reference: &str,
        catalog: ServiceCatalog,
        now: u64,
    ) -> Result<(), String> {
        catalog.validate()?;
        if source_reference != catalog.presenter_reference || !catalog.is_current(now) {
            return Err("catalog source mismatch or non-current evidence".into());
        }
        let key = (source_reference.to_owned(), catalog.service_id.clone());
        if let Some(previous) = self.entries.get(&key) {
            if catalog.owner_reference != previous.catalog.owner_reference {
                return Err("service owner changed; explicit removal required".into());
            }
            if catalog.observed_at_unix_ms < previous.catalog.observed_at_unix_ms
                || (catalog.observed_at_unix_ms == previous.catalog.observed_at_unix_ms
                    && catalog != previous.catalog)
            {
                return Err("catalog evidence regressed or conflicts".into());
            }
        } else if self.entries.len() >= 256 {
            return Err("service inventory is full".into());
        }
        let health = self
            .entries
            .get(&key)
            .and_then(|entry| entry.health.clone());
        self.entries.insert(key, InventoryEntry { catalog, health });
        Ok(())
    }

    pub fn observe_health(
        &mut self,
        source_reference: &str,
        health: ServiceHealth,
        now: u64,
    ) -> Result<(), String> {
        health.validate()?;
        if health.observed_at_unix_ms > now {
            return Err("future health evidence".into());
        }
        let entry = self
            .entries
            .get_mut(&(source_reference.to_owned(), health.service_id.clone()))
            .ok_or("health has no matching source catalog")?;
        if let Some(previous) = &entry.health
            && (health.observed_at_unix_ms < previous.observed_at_unix_ms
                || (health.observed_at_unix_ms == previous.observed_at_unix_ms
                    && health != *previous))
        {
            return Err("health evidence regressed or conflicts".into());
        }
        entry.health = Some(health);
        Ok(())
    }

    pub fn entries(&self) -> impl Iterator<Item = &InventoryEntry> {
        self.entries.values()
    }

    /// Revocation or caller-owned retention policy, not automatic discovery.
    pub fn remove(&mut self, source_reference: &str, service_id: &str) -> bool {
        self.entries
            .remove(&(source_reference.to_owned(), service_id.to_owned()))
            .is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inventory_capacity_is_bounded_and_removal_releases_space() {
        let catalog = ServiceCatalog {
            contract: SERVICE_CATALOG_KIND.into(),
            service_id: "radio.one".into(),
            adapter_kind: "radioman".into(),
            owner_reference: "radio-pi".into(),
            presenter_reference: "radio-pi".into(),
            observed_at_unix_ms: 100,
            valid_until_unix_ms: 200,
            resources: vec![],
        };
        let mut inventory = ServiceInventory::default();
        for index in 0..256 {
            let mut next = catalog.clone();
            next.service_id = format!("radio.{index}");
            inventory.observe_catalog("radio-pi", next, 150).unwrap();
        }
        assert!(
            inventory
                .observe_catalog("radio-pi", catalog.clone(), 150)
                .is_err()
        );
        assert!(inventory.remove("radio-pi", "radio.0"));
        inventory.observe_catalog("radio-pi", catalog, 150).unwrap();
        assert_eq!(inventory.entries().count(), 256);
    }

    #[test]
    fn health_expires_and_rejects_future_evidence() {
        let mut health = ServiceHealth {
            contract: SERVICE_HEALTH_KIND.into(),
            service_id: "radio.one".into(),
            state: HealthState::Healthy,
            reason: None,
            observed_at_unix_ms: 100,
            valid_until_unix_ms: 200,
        };
        assert_eq!(health.effective_state(100), HealthState::Healthy);
        assert_eq!(health.effective_state(99), HealthState::Stale);
        assert_eq!(health.effective_state(200), HealthState::Stale);
        health.contract = "unknown".into();
        assert!(health.validate().is_err());
        assert_eq!(health.effective_state(150), HealthState::Stale);
    }

    #[test]
    fn catalog_is_bounded_deduplicated_and_strict() {
        let mut catalog = ServiceCatalog {
            contract: SERVICE_CATALOG_KIND.into(),
            service_id: "radio.one".into(),
            adapter_kind: "radioman".into(),
            owner_reference: "radio-pi".into(),
            presenter_reference: "radio-pi".into(),
            observed_at_unix_ms: 100,
            valid_until_unix_ms: 200,
            resources: vec![ServiceResource {
                id: "rtl-sdr.1".into(),
                kind: "radio.receiver".into(),
                display_name: "RTL-SDR".into(),
                stream_ids: vec!["radio.fft".into()],
                operations: vec![ServiceOperation {
                    id: "tune".into(),
                    request_kind: "radioman.tune.v1".into(),
                    requires: "radioman.control".into(),
                }],
            }],
        };
        assert!(catalog.is_current(100));
        assert!(!catalog.is_current(200));
        let mut inventory = ServiceInventory::default();
        assert!(
            inventory
                .observe_catalog("wrong", catalog.clone(), 150)
                .is_err()
        );
        inventory
            .observe_catalog("radio-pi", catalog.clone(), 150)
            .unwrap();
        inventory
            .observe_catalog("radio-pi", catalog.clone(), 150)
            .unwrap();
        assert_eq!(
            inventory.entries().next().unwrap().state(150),
            HealthState::Unknown
        );
        let health = ServiceHealth {
            contract: SERVICE_HEALTH_KIND.into(),
            service_id: catalog.service_id.clone(),
            state: HealthState::Healthy,
            reason: None,
            observed_at_unix_ms: 100,
            valid_until_unix_ms: 175,
        };
        assert!(
            inventory
                .observe_health("wrong", health.clone(), 150)
                .is_err()
        );
        inventory
            .observe_health("radio-pi", health.clone(), 150)
            .unwrap();
        assert_eq!(
            inventory.entries().next().unwrap().state(150),
            HealthState::Healthy
        );
        assert_eq!(
            inventory.entries().next().unwrap().state(175),
            HealthState::Stale
        );
        let mut regressed = catalog.clone();
        regressed.observed_at_unix_ms = 99;
        assert!(
            inventory
                .observe_catalog("radio-pi", regressed, 150)
                .is_err()
        );
        let mut changed_owner = catalog.clone();
        changed_owner.owner_reference = "other-owner".into();
        assert!(
            inventory
                .observe_catalog("radio-pi", changed_owner, 150)
                .is_err()
        );
        let mut regressed_health = health;
        regressed_health.observed_at_unix_ms = 99;
        assert!(
            inventory
                .observe_health("radio-pi", regressed_health, 150)
                .is_err()
        );
        assert!(inventory.remove("radio-pi", &catalog.service_id));
        assert_eq!(inventory.entries().count(), 0);
        let json = serde_json::to_string(&catalog).unwrap();
        assert_eq!(
            serde_json::from_str::<ServiceCatalog>(&json).unwrap(),
            catalog
        );
        let mut value = serde_json::to_value(&catalog).unwrap();
        value["credentials"] = "secret".into();
        assert!(serde_json::from_value::<ServiceCatalog>(value).is_err());
        catalog.resources.push(catalog.resources[0].clone());
        assert!(catalog.validate().is_err());
        catalog.resources.pop();
        catalog.valid_until_unix_ms = u64::MAX;
        assert!(catalog.validate().is_err());
    }
}
