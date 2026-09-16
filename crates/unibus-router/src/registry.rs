//! Registry of connected nodes, their grants, and live state.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use unibus::grants::NodeGrant;

/// Heartbeat window: nodes must re-register or heartbeat within this interval.
pub const HEARTBEAT_TTL: Duration = Duration::from_secs(10);

pub type NodeId = String;

/// A registered node with live registration state.
#[derive(Debug, Clone)]
pub struct EdgeNode {
    pub producer_id: String,
    pub zones: Vec<String>,
    pub capabilities: Vec<String>,
    pub screen_anchors: Vec<String>,
    pub site: String,
    /// Wire schema the peer presented at registration: 1 = closed jarvis
    /// enum (rejects unknown kinds), 2 = routed-opaque unibus reader.
    pub schema_version: u32,
    pub connected_at: Instant,
    pub last_heartbeat: Instant,
    pub token_authenticated: bool,
}

impl EdgeNode {
    pub fn new(grant: &NodeGrant) -> Self {
        Self {
            producer_id: grant.producer_id.clone(),
            zones: grant.zones.clone(),
            capabilities: grant.capabilities.clone(),
            screen_anchors: grant.screen_anchors.clone(),
            site: grant.site.clone(),
            schema_version: 2,
            connected_at: Instant::now(),
            last_heartbeat: Instant::now(),
            token_authenticated: true,
        }
    }

    pub fn is_alive(&self) -> bool {
        self.last_heartbeat.elapsed() < HEARTBEAT_TTL
    }

    pub fn can(&self, capability: &str) -> bool {
        self.capabilities.iter().any(|c| c == capability)
    }

    pub fn in_zone(&self, zone: &str) -> bool {
        self.zones.iter().any(|z| z == zone)
    }

    pub fn serves(&self, zone: &str, capability: &str) -> bool {
        self.is_alive() && self.in_zone(zone) && self.can(capability)
    }
}

/// Thread-safe registry of connected nodes.
#[derive(Debug, Default)]
pub struct NodeRegistry {
    nodes: HashMap<NodeId, EdgeNode>,
}

impl NodeRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register or refresh a node.
    pub fn upsert(&mut self, node: EdgeNode) -> &EdgeNode {
        self.nodes
            .entry(node.producer_id.clone())
            .and_modify(|existing| {
                existing.last_heartbeat = Instant::now();
                existing.zones.clone_from(&node.zones);
                existing.capabilities.clone_from(&node.capabilities);
                existing.screen_anchors.clone_from(&node.screen_anchors);
                existing.site.clone_from(&node.site);
            })
            .or_insert(node)
    }

    pub fn remove(&mut self, producer_id: &str) {
        self.nodes.remove(producer_id);
    }

    /// Mark a heartbeat for a registered node. A dead node must re-register.
    pub fn heartbeat(&mut self, producer_id: &str) -> bool {
        match self.nodes.get_mut(producer_id) {
            Some(node) => {
                if !node.token_authenticated || !node.is_alive() {
                    return false;
                }
                node.last_heartbeat = Instant::now();
                true
            }
            None => false,
        }
    }

    /// Prune dead nodes. Returns the count of removed nodes.
    pub fn prune_dead(&mut self) -> usize {
        let before = self.nodes.len();
        self.nodes.retain(|_, node| node.is_alive());
        before - self.nodes.len()
    }

    /// Best available node for a zone + capability. Preference:
    /// longest-connected first, then producer_id for determinism.
    pub fn select(&self, zone: &str, capability: &str) -> Option<&EdgeNode> {
        self.nodes
            .values()
            .filter(|node| node.serves(zone, capability))
            .min_by_key(|node| {
                (
                    node.connected_at,
                    std::cmp::Reverse(node.producer_id.clone()),
                )
            })
    }

    /// A live display-capable node claiming a specific screen anchor.
    pub fn select_for_screen(&self, screen_anchor: &str) -> Option<&EdgeNode> {
        self.nodes
            .values()
            .filter(|node| {
                node.is_alive()
                    && node.capabilities.iter().any(|c| is_display_capability(c))
                    && node.screen_anchors.iter().any(|a| a == screen_anchor)
            })
            .min_by_key(|node| {
                (
                    node.connected_at,
                    std::cmp::Reverse(node.producer_id.clone()),
                )
            })
    }

    /// All live nodes serving a zone + capability.
    pub fn select_all(&self, zone: &str, capability: &str) -> Vec<&EdgeNode> {
        self.nodes
            .values()
            .filter(|node| node.serves(zone, capability))
            .collect()
    }

    pub fn is_known(&self, producer_id: &str) -> bool {
        self.nodes
            .get(producer_id)
            .map(|n| n.is_alive())
            .unwrap_or(false)
    }

    pub fn get(&self, producer_id: &str) -> Option<&EdgeNode> {
        self.nodes.get(producer_id)
    }

    pub fn count(&self) -> usize {
        self.nodes.len()
    }

    pub fn all(&self) -> impl Iterator<Item = &EdgeNode> {
        self.nodes.values()
    }

    pub fn live(&self) -> impl Iterator<Item = &EdgeNode> {
        self.nodes.values().filter(|n| n.is_alive())
    }
}

/// v1 compatibility: JARVIS's single-word `display` tag and the namespaced
/// `canvas.display` both mean "this node owns screens".
pub fn is_display_capability(capability: &str) -> bool {
    capability == "display" || capability == "canvas.display"
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grant(id: &str, cap: &str, zone: &str) -> NodeGrant {
        NodeGrant {
            producer_id: id.into(),
            auth_subject: String::new(),
            zones: vec![zone.into()],
            capabilities: vec![cap.into()],
            screen_anchors: Vec::new(),
            site: "default".into(),
            role: Default::default(),
        }
    }

    #[test]
    fn select_prefers_longest_connected() {
        let mut registry = NodeRegistry::new();
        let mut first = EdgeNode::new(&grant("home-pi", "display", "living"));
        first.connected_at = Instant::now() - Duration::from_secs(10);
        registry.upsert(first);
        let mut second = EdgeNode::new(&grant("macbook-neo", "display", "living"));
        second.connected_at = Instant::now() - Duration::from_secs(5);
        registry.upsert(second);

        let selected = registry.select("living", "display").unwrap();
        assert_eq!(selected.producer_id, "home-pi");
    }

    #[test]
    fn select_excludes_dead_and_wrong_zone() {
        let mut registry = NodeRegistry::new();
        let mut node = EdgeNode::new(&grant("home-pi", "audio_output", "living"));
        node.last_heartbeat = Instant::now() - Duration::from_secs(30);
        registry.upsert(node);

        assert!(registry.select("living", "audio_output").is_none());
        assert!(registry.select("bedroom", "audio_output").is_none());
    }

    #[test]
    fn select_for_screen_finds_display_with_anchor() {
        let mut mac = grant("mac-studio", "display", "default");
        mac.screen_anchors = vec!["screen-left".into()];
        let mut pi = grant("home-pi", "canvas.display", "living-room");
        pi.screen_anchors = vec!["screen-tv".into()];
        let mut registry = NodeRegistry::new();
        registry.upsert(EdgeNode::new(&mac));
        registry.upsert(EdgeNode::new(&pi));

        assert_eq!(
            registry
                .select_for_screen("screen-left")
                .unwrap()
                .producer_id,
            "mac-studio"
        );
        assert_eq!(
            registry.select_for_screen("screen-tv").unwrap().producer_id,
            "home-pi"
        );
        assert!(registry.select_for_screen("unknown-screen").is_none());
    }

    #[test]
    fn heartbeat_extends_lifetime() {
        let mut registry = NodeRegistry::new();
        let mut node = EdgeNode::new(&grant("edge-1", "display", "zone"));
        node.last_heartbeat = Instant::now() - Duration::from_secs(30);
        registry.upsert(node);

        assert!(!registry.is_known("edge-1"));
        assert!(!registry.heartbeat("edge-1"));

        registry.upsert(EdgeNode::new(&grant("edge-1", "display", "zone")));
        assert!(registry.is_known("edge-1"));
        assert!(registry.heartbeat("edge-1"));
    }

    #[test]
    fn prune_dead_removes_expired() {
        let mut registry = NodeRegistry::new();
        let mut dead = EdgeNode::new(&grant("dead", "display", "zone"));
        dead.last_heartbeat = Instant::now() - Duration::from_secs(60);
        registry.upsert(dead);
        registry.upsert(EdgeNode::new(&grant("alive", "display", "zone")));

        assert_eq!(registry.prune_dead(), 1);
        assert!(registry.is_known("alive"));
        assert!(!registry.is_known("dead"));
    }
}
