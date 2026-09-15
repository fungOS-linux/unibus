//! Hub configuration. A v1 JARVIS `router.json` must load unchanged;
//! v2 additions are optional fields.

use std::path::PathBuf;

use anyhow::{Result, anyhow, ensure};
use serde::{Deserialize, Serialize};
use unibus::grants::{NodeGrant, Role};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HubConfig {
    pub installation_id: String,
    pub listen: String,
    /// Retained from v1 config files; the hub keeps no on-disk state in v1.
    #[serde(default)]
    pub state_dir: Option<PathBuf>,
    #[serde(default)]
    pub routes: Vec<RouteRule>,
    pub nodes: Vec<NodeConfig>,
}

/// One routing rule: an opaque data kind delivered to nodes holding a
/// capability. Without a rule (and without a plugin), a kind fails loudly.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouteRule {
    /// Message kind, v1 tag or namespaced name.
    pub kind: String,
    pub target: RouteTarget,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "mode")]
pub enum RouteTarget {
    /// Nodes in the envelope's destination zone (default `default`) that
    /// hold this capability.
    Capability { name: String },
    /// Broadcast to every live holder installation-wide.
    AnyHolder { name: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeConfig {
    pub producer_id: String,
    pub token_file: PathBuf,
    #[serde(default)]
    pub zones: Vec<String>,
    pub capabilities: Vec<String>,
    #[serde(default)]
    pub screen_anchors: Vec<String>,
    #[serde(default)]
    pub site: Option<String>,
    #[serde(default)]
    pub role: Option<Role>,
    /// fpl-auth subject required in assertions. Empty selects v1
    /// shared-token auth for this node.
    #[serde(default)]
    pub auth_subject: Option<String>,
}

impl NodeConfig {
    pub fn to_grant(&self) -> NodeGrant {
        NodeGrant {
            producer_id: self.producer_id.clone(),
            auth_subject: self.auth_subject.clone().unwrap_or_default(),
            zones: self.zones.clone(),
            capabilities: self.capabilities.clone(),
            screen_anchors: self.screen_anchors.clone(),
            site: self.site.clone().unwrap_or_else(|| "default".into()),
            role: self.role.unwrap_or_default(),
        }
    }
}

impl HubConfig {
    pub fn read(path: &std::path::Path) -> Result<Self> {
        let bytes =
            std::fs::read(path).map_err(|e| anyhow!("read hub config {}: {e}", path.display()))?;
        let mut config: Self = serde_json::from_slice(&bytes)
            .map_err(|e| anyhow!("parse hub config {}: {e}", path.display()))?;
        let base = path
            .parent()
            .ok_or_else(|| anyhow!("config parent missing"))?;
        for node in &mut config.nodes {
            if node.token_file.is_relative() {
                node.token_file = base.join(&node.token_file);
            }
        }
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(
            !self.installation_id.is_empty() && self.installation_id.len() <= 128,
            "invalid installation_id"
        );
        ensure!(!self.listen.is_empty(), "listen address is required");
        // TCP hub binds loopback only; remote nodes use authenticated
        // tunnels. A QUIC listener on the tailnet arrives with dock-48wn and
        // its own explicit bind address — never implied.
        let parsed: std::net::SocketAddr = self
            .listen
            .parse()
            .map_err(|e| anyhow!("invalid listen address {}: {e}", self.listen))?;
        ensure!(
            parsed.ip().is_loopback(),
            "TCP hub must bind loopback; remote nodes use tunnels"
        );
        ensure!(
            !self.nodes.is_empty(),
            "at least one node grant is required"
        );
        let mut seen = std::collections::HashSet::new();
        for node in &self.nodes {
            ensure!(
                seen.insert(node.producer_id.clone()),
                "duplicate producer_id: {}",
                node.producer_id
            );
            ensure!(
                !node.capabilities.is_empty(),
                "node {} has no capabilities",
                node.producer_id
            );
            ensure!(
                !node.zones.is_empty() || !node.screen_anchors.is_empty(),
                "node {} serves nothing: no zones or anchors",
                node.producer_id
            );
            node.to_grant()
                .validate_v1_compat()
                .map_err(|e| anyhow!("node {}: {e}", node.producer_id))?;
        }
        let mut kinds = std::collections::HashSet::new();
        for rule in &self.routes {
            ensure!(!rule.kind.trim().is_empty(), "route rule with empty kind");
            ensure!(
                kinds.insert(rule.kind.clone()),
                "duplicate route rule for kind {}",
                rule.kind
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_config(dir: &std::path::Path, content: &str) -> PathBuf {
        let path = dir.join("hub.json");
        std::fs::write(&path, content).unwrap();
        std::fs::write(dir.join("token"), "a".repeat(32)).unwrap();
        path
    }

    #[test]
    fn v1_jarvis_router_json_loads_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(
            dir.path(),
            r#"{
            "installation_id": "home",
            "listen": "127.0.0.1:18790",
            "state_dir": "state",
            "nodes": [
                {"producer_id": "mac-studio-brain", "token_file": "token", "zones": ["default", "living-room"], "capabilities": ["sound", "audio_output", "display"]},
                {"producer_id": "home-pi-display", "token_file": "token", "zones": ["living-room"], "capabilities": ["display"], "screen_anchors": ["screen-tv"]}
            ]
        }"#,
        );
        let config = HubConfig::read(&path).unwrap();
        assert_eq!(config.nodes.len(), 2);
        assert!(config.routes.is_empty());
    }

    #[test]
    fn rejects_non_loopback_listen() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(
            dir.path(),
            r#"{"installation_id":"home","listen":"0.0.0.0:18790","nodes":[]}"#,
        );
        assert!(HubConfig::read(&path).is_err());
    }

    #[test]
    fn rejects_unknown_fields_and_duplicate_kinds() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(
            dir.path(),
            r#"{"installation_id":"home","listen":"127.0.0.1:0","admin":true,"nodes":[]}"#,
        );
        assert!(HubConfig::read(&path).is_err());
        let path = write_config(
            dir.path(),
            r#"{"installation_id":"home","listen":"127.0.0.1:0","routes":[
                 {"kind":"canvas_command","target":{"mode":"capability","name":"display"}},
                 {"kind":"canvas_command","target":{"mode":"capability","name":"display"}}
               ],"nodes":[{"producer_id":"n","token_file":"token","zones":["z"],"capabilities":["display"]}]}"#,
        );
        assert!(HubConfig::read(&path).is_err());
    }
}
