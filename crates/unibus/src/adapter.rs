//! Provider-neutral contracts for advertising and leasing edge adapters.
//!
//! An edge supervisor owns process lifecycle and local policy. The bus carries
//! catalog, status, and bounded demand; it never starts a process itself.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

pub const ADAPTER_CATALOG_KIND: &str = "unibus.adapter.catalog.v1";
pub const ADAPTER_STATUS_KIND: &str = "unibus.adapter.status.v1";
pub const ADAPTER_INTEREST_KIND: &str = "unibus.adapter.interest.v1";

const MAX_LEASE_MS: u64 = 5 * 60 * 1000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdapterDescriptor {
    /// Stable, node-local identity such as `mqtt.local` or `prometheus.host`.
    pub id: String,
    /// Protocol/runtime family such as `mqtt`, `prometheus`, or `media`.
    pub kind: String,
    /// Typed message kinds this adapter can produce or consume.
    #[serde(default)]
    pub message_kinds: BTreeSet<String>,
    /// Local policy permits activation. This is informative, not remotely
    /// mutable through the interest contract.
    pub enabled: bool,
    /// The installed binary and required local configuration are usable.
    pub available: bool,
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
}

impl AdapterDescriptor {
    pub fn validate(&self) -> Result<()> {
        validate_id(&self.id, "adapter id")?;
        validate_id(&self.kind, "adapter kind")?;
        ensure!(self.message_kinds.len() <= 128, "too many message kinds");
        ensure!(self.labels.len() <= 64, "too many adapter labels");
        for kind in &self.message_kinds {
            validate_id(kind, "message kind")?;
        }
        for (key, value) in &self.labels {
            validate_id(key, "label key")?;
            ensure!(value.len() <= 256, "adapter label value is too long");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdapterCatalog {
    pub node_id: String,
    pub revision: String,
    pub observed_at_unix_ms: u64,
    pub adapters: Vec<AdapterDescriptor>,
}

impl AdapterCatalog {
    pub fn validate(&self) -> Result<()> {
        validate_id(&self.node_id, "node id")?;
        validate_id(&self.revision, "catalog revision")?;
        ensure!(!self.adapters.is_empty(), "adapter catalog is empty");
        ensure!(self.adapters.len() <= 256, "too many adapters");
        let mut ids = BTreeSet::new();
        for adapter in &self.adapters {
            adapter.validate()?;
            ensure!(
                ids.insert(&adapter.id),
                "duplicate adapter id {}",
                adapter.id
            );
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdapterState {
    Stopped,
    Starting,
    Running,
    Stopping,
    Degraded,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdapterStatus {
    pub node_id: String,
    pub adapter_id: String,
    pub state: AdapterState,
    pub active_leases: u32,
    pub observed_at_unix_ms: u64,
    #[serde(default)]
    pub detail: Option<String>,
}

impl AdapterStatus {
    pub fn validate(&self) -> Result<()> {
        validate_id(&self.node_id, "node id")?;
        validate_id(&self.adapter_id, "adapter id")?;
        ensure!(
            self.detail
                .as_ref()
                .is_none_or(|detail| detail.len() <= 1024),
            "adapter status detail is too long"
        );
        Ok(())
    }
}

/// Idempotent, renewable demand for one adapter on one edge. Consumers stop
/// renewing to release demand; supervisors expire leases locally.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdapterInterest {
    pub lease_id: String,
    pub consumer_id: String,
    pub node_id: String,
    pub adapter_id: String,
    pub issued_at_unix_ms: u64,
    pub expires_at_unix_ms: u64,
    /// Optional stable reason such as a Canvas surface or recording session.
    #[serde(default)]
    pub reason: Option<String>,
}

impl AdapterInterest {
    pub fn validate(&self) -> Result<()> {
        validate_id(&self.lease_id, "lease id")?;
        validate_id(&self.consumer_id, "consumer id")?;
        validate_id(&self.node_id, "node id")?;
        validate_id(&self.adapter_id, "adapter id")?;
        ensure!(
            self.issued_at_unix_ms < self.expires_at_unix_ms,
            "adapter interest expires before it starts"
        );
        ensure!(
            self.expires_at_unix_ms - self.issued_at_unix_ms <= MAX_LEASE_MS,
            "adapter interest exceeds maximum lease"
        );
        ensure!(
            self.reason
                .as_ref()
                .is_none_or(|reason| reason.len() <= 256),
            "adapter interest reason is too long"
        );
        Ok(())
    }

    pub fn active_at(&self, now_unix_ms: u64) -> bool {
        self.issued_at_unix_ms <= now_unix_ms && now_unix_ms < self.expires_at_unix_ms
    }
}

fn validate_id(value: &str, label: &str) -> Result<()> {
    ensure!(!value.is_empty() && value.len() <= 128, "invalid {label}");
    ensure!(
        value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric()
                || matches!(byte, b'.' | b'-' | b'_' | b':' | b'/')),
        "invalid {label}"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interest_is_bounded_and_expires_deterministically() {
        let lease = AdapterInterest {
            lease_id: "canvas:screen-tv:weather".into(),
            consumer_id: "canvas-edge-home".into(),
            node_id: "home-pi".into(),
            adapter_id: "mqtt.local".into(),
            issued_at_unix_ms: 1_000,
            expires_at_unix_ms: 31_000,
            reason: Some("visible:home/weather".into()),
        };
        lease.validate().unwrap();
        assert!(lease.active_at(30_999));
        assert!(!lease.active_at(31_000));
    }

    #[test]
    fn interest_cannot_create_an_indefinite_remote_enable() {
        let lease = AdapterInterest {
            lease_id: "consumer:adapter".into(),
            consumer_id: "consumer".into(),
            node_id: "home-pi".into(),
            adapter_id: "mqtt.local".into(),
            issued_at_unix_ms: 1,
            expires_at_unix_ms: 1 + MAX_LEASE_MS + 1,
            reason: None,
        };
        assert!(lease.validate().is_err());
    }

    #[test]
    fn catalog_rejects_duplicate_adapter_ids() {
        let adapter = AdapterDescriptor {
            id: "mqtt.local".into(),
            kind: "mqtt".into(),
            message_kinds: BTreeSet::from(["mqtt.publication.v1".into()]),
            enabled: true,
            available: true,
            labels: BTreeMap::new(),
        };
        let catalog = AdapterCatalog {
            node_id: "home-pi".into(),
            revision: "sha256:abc".into(),
            observed_at_unix_ms: 1,
            adapters: vec![adapter.clone(), adapter],
        };
        assert!(catalog.validate().is_err());
    }
}
