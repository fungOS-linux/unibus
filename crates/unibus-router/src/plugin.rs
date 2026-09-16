//! Extension seam for hub-side behavior that is genuinely stateful.
//!
//! The v1 `jarvis-router` hard-coded presence fan-out into the relay loop.
//! The lifted hub routes opaque data by configuration; a domain that needs
//! hub state (like Presence's zone-keyed latest-value store) ships its own
//! plugin compiled next to that domain, keeping L1 body-agnostic.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use anyhow::Result;
pub use unibus::envelope::Envelope;

use crate::config::NodeConfig;
use crate::registry::NodeRegistry;

/// A node that has been registered and authenticated by the hub.
#[derive(Debug, Clone)]
pub struct Delivery {
    pub producer_id: String,
    /// Declared topology label (`home-lan`, `wan`, ...).
    pub site: String,
    pub zones: Vec<String>,
}

/// Anything a hub-side message can be pushed into.
pub trait Sink: Send + Sync {
    fn push(&self, envelope: &Envelope, payload: &[u8]) -> Result<()>;
    /// Wire schema of the peer behind this sink; hub advisories (like
    /// node_state) only go to peers that can parse unknown kinds.
    fn peer_schema(&self) -> u32 {
        1
    }
}

/// A cloneable view of the hub passed to plugins.
#[derive(Clone)]
pub struct HubHandle {
    pub(crate) installation_id: String,
    pub(crate) grants: Arc<HashMap<String, NodeConfig>>,
    pub(crate) registry: Arc<Mutex<NodeRegistry>>,
    pub(crate) conns: Arc<Mutex<HashMap<String, Arc<dyn Sink>>>>,
}

pub trait HubPlugin: Send + Sync {
    /// Called for every data (non-control) message the hub receives.
    /// Returning `true` means the plugin handled delivery and default
    /// routing is skipped. Plugins emit via `hub.send_to_producer` and the
    /// `nodes_with` fan-out helpers.
    fn handle(
        &self,
        hub: &HubHandle,
        source: &str,
        envelope: &Envelope,
        payload: &[u8],
    ) -> Result<bool>;
}

impl HubHandle {
    pub(crate) fn connection(&self, producer_id: &str) -> Option<Arc<dyn Sink>> {
        self.conns
            .lock()
            .ok()
            .and_then(|conns| conns.get(producer_id).cloned())
    }

    pub fn installation_id(&self) -> &str {
        &self.installation_id
    }

    pub fn grant_of(&self, producer_id: &str) -> Option<unibus::grants::NodeGrant> {
        self.grants.get(producer_id).map(NodeConfig::to_grant)
    }

    /// Direct write to one connected node.
    pub fn send_to_producer(
        &self,
        producer_id: &str,
        envelope: &Envelope,
        payload: &[u8],
    ) -> Result<()> {
        match self.connection(producer_id) {
            Some(conn) => conn.push(envelope, payload),
            None => Err(anyhow::anyhow!("node '{producer_id}' is not connected")),
        }
    }

    /// Every live producer except `exclude` (installation-wide broadcast).
    pub fn broadcast(&self, exclude: &str, envelope: &Envelope, payload: &[u8]) -> usize {
        let targets: Vec<String> = match self.conns.lock() {
            Ok(conns) => conns
                .keys()
                .filter(|id| id.as_str() != exclude)
                .cloned()
                .collect(),
            Err(_) => return 0,
        };
        let mut delivered = 0;
        for target in targets {
            if self
                .connection(&target)
                .map(|conn| conn.push(envelope, payload).is_ok())
                .unwrap_or(false)
            {
                delivered += 1;
            }
        }
        delivered
    }

    /// Live nodes holding a capability, installation-wide.
    pub fn nodes_with(&self, capability: &str) -> Vec<Delivery> {
        let registry = match self.registry.lock() {
            Ok(registry) => registry,
            Err(_) => return Vec::new(),
        };
        registry
            .live()
            .filter(|node| node.can(capability))
            .map(|node| Delivery {
                producer_id: node.producer_id.clone(),
                site: node.site.clone(),
                zones: node.zones.clone(),
            })
            .collect()
    }
}
