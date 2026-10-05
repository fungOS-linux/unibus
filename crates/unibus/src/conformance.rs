//! Deterministic conformance checks shared by edge-adapter crates.
//!
//! The harness checks the lifecycle contract owned by Unibus while leaving
//! protocol I/O and process supervision with the adapter. Keeping this here
//! prevents MQTT, HID, media, and future observability edges from each
//! inventing subtly different lease and reconnect behavior.

use std::{collections::BTreeSet, time::Duration};

use anyhow::{Result, ensure};

use crate::{
    AdapterDescriptor, AdapterInterest, AdapterReconciler, ReconnectBackoff, ReconnectPolicy,
};

/// Inputs that identify one installed adapter to the common lifecycle tests.
#[derive(Debug, Clone)]
pub struct AdapterConformanceFixture {
    pub node_id: String,
    pub adapter_id: String,
    pub adapter_kind: String,
    pub message_kinds: BTreeSet<String>,
}

impl AdapterConformanceFixture {
    pub fn new(
        node_id: impl Into<String>,
        adapter_id: impl Into<String>,
        adapter_kind: impl Into<String>,
        message_kinds: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        Self {
            node_id: node_id.into(),
            adapter_id: adapter_id.into(),
            adapter_kind: adapter_kind.into(),
            message_kinds: message_kinds.into_iter().map(Into::into).collect(),
        }
    }
}

/// Operational queue facts exposed by an adapter status projection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QueueSnapshot {
    pub depth: usize,
    pub capacity: usize,
    pub dropped_total: u64,
}

impl QueueSnapshot {
    pub fn validate(self) -> Result<()> {
        ensure!(self.capacity > 0, "adapter queue capacity must be positive");
        ensure!(
            self.depth <= self.capacity,
            "adapter queue depth exceeds its declared bound"
        );
        Ok(())
    }
}

/// Run the behavior common to every demand-driven edge adapter.
///
/// This deliberately uses an injected logical clock and performs no sleeps or
/// I/O, so adapter crates can call it from ordinary unit tests.
pub fn verify_adapter_lifecycle(fixture: &AdapterConformanceFixture) -> Result<()> {
    let descriptor = AdapterDescriptor {
        id: fixture.adapter_id.clone(),
        kind: fixture.adapter_kind.clone(),
        message_kinds: fixture.message_kinds.clone(),
        enabled: true,
        available: true,
        labels: Default::default(),
    };
    descriptor.validate()?;

    let mut reconciler = AdapterReconciler::new([descriptor], std::iter::empty())?;
    ensure!(
        !reconciler.desired_active(&fixture.adapter_id, 1_000),
        "adapter activated without demand"
    );

    let mut interest = AdapterInterest {
        lease_id: format!("conformance:{}", fixture.adapter_id),
        consumer_id: "unibus-conformance".into(),
        node_id: fixture.node_id.clone(),
        adapter_id: fixture.adapter_id.clone(),
        issued_at_unix_ms: 1_000,
        expires_at_unix_ms: 2_000,
        reason: Some("adapter-conformance".into()),
    };
    reconciler.apply_interest(&fixture.node_id, interest.clone())?;
    ensure!(reconciler.desired_active(&fixture.adapter_id, 1_500));
    ensure!(reconciler.active_lease_count(&fixture.adapter_id, 1_500) == 1);

    // Renewal is idempotent: the lease identity stays constant and cannot
    // inflate demand accounting.
    interest.issued_at_unix_ms = 1_500;
    interest.expires_at_unix_ms = 2_500;
    reconciler.apply_interest(&fixture.node_id, interest)?;
    ensure!(reconciler.active_lease_count(&fixture.adapter_id, 2_000) == 1);
    reconciler.expire(2_500);
    ensure!(!reconciler.desired_active(&fixture.adapter_id, 2_500));

    // Every adapter gets the same bounded retry semantics. A stable session
    // resets the history rather than carrying an old outage forever.
    let policy = ReconnectPolicy {
        initial_delay: Duration::from_millis(10),
        maximum_delay: Duration::from_millis(40),
        stable_after: Duration::from_secs(1),
    };
    let mut reconnect =
        ReconnectBackoff::new(policy).map_err(|error| anyhow::anyhow!(error))?;
    ensure!(reconnect.failed_after(Duration::ZERO) == Duration::from_millis(10));
    ensure!(reconnect.failed_after(Duration::ZERO) == Duration::from_millis(20));
    ensure!(reconnect.failed_after(Duration::ZERO) == Duration::from_millis(40));
    ensure!(reconnect.failed_after(Duration::ZERO) == Duration::from_millis(40));
    ensure!(reconnect.failed_after(Duration::from_secs(1)) == Duration::from_millis(10));

    QueueSnapshot {
        depth: 4,
        capacity: 8,
        dropped_total: 1,
    }
    .validate()?;
    ensure!(
        QueueSnapshot {
            depth: 9,
            capacity: 8,
            dropped_total: 0,
        }
        .validate()
        .is_err(),
        "unbounded queue state was accepted"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reference_adapter_passes_the_shared_contract() {
        verify_adapter_lifecycle(&AdapterConformanceFixture::new(
            "node-a",
            "reference.local",
            "reference",
            ["reference.event.v1"],
        ))
        .unwrap();
    }
}
