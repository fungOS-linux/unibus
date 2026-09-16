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
    /// Compatibility escape hatch for plaintext TCP on a trusted LAN. Remote
    /// deployments should use QUIC/TLS or put loopback TCP behind a tunnel.
    #[serde(default)]
    pub allow_insecure_lan: bool,
    /// Optional production QUIC/TLS listener. Paths are resolved relative to
    /// the configuration file.
    #[serde(default)]
    pub quic: Option<QuicConfig>,
    /// Retained from v1 config files; the hub keeps no on-disk state in v1.
    #[serde(default)]
    pub state_dir: Option<PathBuf>,
    #[serde(default)]
    pub routes: Vec<RouteRule>,
    pub nodes: Vec<NodeConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QuicConfig {
    pub listen: String,
    pub cert_file: PathBuf,
    pub key_file: PathBuf,
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
    /// Deliver to the producer named by the envelope's `destination`
    /// (request/response correlation, e.g. face-ID replies).
    Producer,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeConfig {
    pub producer_id: String,
    #[serde(default)]
    pub token_file: Option<PathBuf>,
    #[serde(default)]
    pub zones: Vec<String>,
    pub capabilities: Vec<String>,
    /// Exact data message kinds this node may publish. Empty retains v1
    /// compatibility only; schema-v2 sessions fail closed.
    #[serde(default)]
    pub publishes: Vec<String>,
    /// Address scopes usable by published messages.
    #[serde(default)]
    pub publish_zones: Vec<String>,
    #[serde(default)]
    pub publish_screen_anchors: Vec<String>,
    #[serde(default)]
    pub publish_producers: Vec<String>,
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
            publishes: self.publishes.clone(),
            publish_zones: self.publish_zones.clone(),
            publish_screen_anchors: self.publish_screen_anchors.clone(),
            publish_producers: self.publish_producers.clone(),
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
            if node
                .token_file
                .as_ref()
                .is_some_and(|path| path.is_relative())
            {
                node.token_file = node.token_file.as_ref().map(|path| base.join(path));
            }
        }
        if let Some(quic) = &mut config.quic {
            if quic.cert_file.is_relative() {
                quic.cert_file = base.join(&quic.cert_file);
            }
            if quic.key_file.is_relative() {
                quic.key_file = base.join(&quic.key_file);
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
        // Loopback is the default and preferred; `0.0.0.0` (direct trusted
        // LAN, token-authenticated) is the JARVIS deployment reality. Any
        // other specific bind address is rejected — a hub on one interface
        // is a decision, not an accident. QUIC-on-tailnet arrives with
        // dock-48wn under its own explicit policy.
        let parsed: std::net::SocketAddr = self
            .listen
            .parse()
            .map_err(|e| anyhow!("invalid listen address {}: {e}", self.listen))?;
        ensure!(
            parsed.ip().is_loopback() || parsed.ip().is_unspecified(),
            "TCP hub must bind loopback or 0.0.0.0; got {}",
            parsed.ip()
        );
        ensure!(
            !parsed.ip().is_unspecified() || self.allow_insecure_lan,
            "plaintext TCP wildcard bind requires allow_insecure_lan=true"
        );
        if let Some(quic) = &self.quic {
            let _: std::net::SocketAddr = quic
                .listen
                .parse()
                .map_err(|error| anyhow!("invalid QUIC listen address {}: {error}", quic.listen))?;
            ensure!(
                !quic.cert_file.as_os_str().is_empty() && !quic.key_file.as_os_str().is_empty(),
                "QUIC cert_file and key_file are required"
            );
        }
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
                node.token_file.is_some()
                    || node
                        .auth_subject
                        .as_ref()
                        .is_some_and(|subject| !subject.trim().is_empty()),
                "node {} requires token_file or auth_subject",
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
    fn accepts_direct_lan_bind() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(
            dir.path(),
            r#"{"installation_id":"home","listen":"0.0.0.0:18790","allow_insecure_lan":true,"nodes":[{"producer_id":"n","token_file":"token","zones":["z"],"capabilities":["display"]}]}"#,
        );
        assert!(HubConfig::read(&path).is_ok());
    }

    #[test]
    fn rejects_wildcard_without_explicit_insecure_policy() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(
            dir.path(),
            r#"{"installation_id":"home","listen":"0.0.0.0:18790","nodes":[{"producer_id":"n","token_file":"token","zones":["z"],"capabilities":["display"]}]}"#,
        );
        assert!(HubConfig::read(&path).is_err());
    }

    #[test]
    fn rejects_specific_non_loopback_bind_for_the_bind_reason() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(
            dir.path(),
            r#"{"installation_id":"home","listen":"192.0.2.1:18790","nodes":[{"producer_id":"n","token_file":"token","zones":["z"],"capabilities":["display"]}]}"#,
        );
        let error = HubConfig::read(&path).unwrap_err().to_string();
        assert!(error.contains("must bind loopback or 0.0.0.0"), "{error}");
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

    #[test]
    fn quic_paths_resolve_relative_to_the_config() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(
            dir.path(),
            r#"{
              "installation_id":"home",
              "listen":"127.0.0.1:0",
              "quic":{"listen":"0.0.0.0:18791","cert_file":"tls/server.pem","key_file":"tls/server-key.pem"},
              "nodes":[{"producer_id":"n","token_file":"token","zones":["z"],"capabilities":["display"]}]
            }"#,
        );
        let config = HubConfig::read(&path).unwrap();
        let quic = config.quic.unwrap();
        assert_eq!(quic.cert_file, dir.path().join("tls/server.pem"));
        assert_eq!(quic.key_file, dir.path().join("tls/server-key.pem"));
    }
}
