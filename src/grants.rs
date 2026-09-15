//! Node grants: the hub's static statement of who a node is and what it may
//! do. Registration claims are checked against the grant and fail closed.
//! Capabilities are an open set of namespaced strings so a new domain never
//! requires an L1 release.

use std::fmt;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// Authoritative origin for content and state.
    Origin,
    /// Site pull-through cache; serves reads, writes go through origin.
    Cache,
    /// Display/audio/sensor surface with no authority of its own.
    Edge,
}

impl fmt::Display for Role {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Self::Origin => "origin",
            Self::Cache => "cache",
            Self::Edge => "edge",
        };
        f.write_str(name)
    }
}

/// One bus slot: identity plus permitted addressing scope. Unknown config
/// fields are a hard error — a typo'd grant must not silently widen scope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeGrant {
    pub producer_id: String,
    /// OIDC subject (fpl-auth) this node's tokens must present.
    pub auth_subject: String,
    #[serde(default)]
    pub zones: Vec<String>,
    #[serde(default)]
    pub capabilities: Vec<String>,
    #[serde(default)]
    pub screen_anchors: Vec<String>,
    /// Declared topology label: `home-lan`, `wan`, `agora-lan`, ...
    pub site: String,
    pub role: Role,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GrantError {
    InvalidName { field: &'static str, value: String },
    Duplicate(String),
}

impl fmt::Display for GrantError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidName { field, value } => {
                write!(f, "invalid {field} name: {value:?}")
            }
            Self::Duplicate(value) => write!(f, "duplicate grant entry: {value:?}"),
        }
    }
}

impl std::error::Error for GrantError {}

/// Namespaced, lowercase, boring: `canvas.display`, `audio.out`,
/// `presence.capture`, `sensor.temp`. At least two segments.
pub fn is_valid_namespaced_name(value: &str) -> bool {
    let mut segments = value.split('.');
    let count = value.match_indices('.').count() + 1;
    segments.all(is_valid_slug) && count >= 2
}

/// Single-segment identifier: `living-room`, `screen-tv`, `home-lan`.
pub fn is_valid_slug(value: &str) -> bool {
    if value.is_empty() || value.len() > 64 {
        return false;
    }
    let mut chars = value.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first.is_ascii_lowercase() || first.is_ascii_digit())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

impl NodeGrant {
    pub fn validate(&self) -> Result<(), GrantError> {
        for (field, values, valid) in [
            (
                "capability",
                &self.capabilities,
                is_valid_namespaced_name as fn(&str) -> bool,
            ),
            ("zone", &self.zones, is_valid_slug),
            ("screen_anchor", &self.screen_anchors, is_valid_slug),
        ] {
            let mut seen = std::collections::BTreeSet::new();
            for value in values {
                if !valid(value) {
                    return Err(GrantError::InvalidName {
                        field,
                        value: value.clone(),
                    });
                }
                if !seen.insert(value.clone()) {
                    return Err(GrantError::Duplicate(value.clone()));
                }
            }
        }
        if !is_valid_slug(&self.producer_id) || !is_valid_slug(&self.site) {
            return Err(GrantError::InvalidName {
                field: "identity",
                value: format!("{}@{}", self.producer_id, self.site),
            });
        }
        if self.auth_subject.trim().is_empty() {
            return Err(GrantError::InvalidName {
                field: "auth_subject",
                value: self.auth_subject.clone(),
            });
        }
        Ok(())
    }

    pub fn can(&self, capability: &str) -> bool {
        self.capabilities.iter().any(|c| c == capability)
    }

    pub fn in_zone(&self, zone: &str) -> bool {
        self.zones.iter().any(|z| z == zone)
    }

    pub fn serves(&self, zone: &str, capability: &str) -> bool {
        self.in_zone(zone) && self.can(capability)
    }

    pub fn claims_screen(&self, anchor: &str) -> bool {
        self.screen_anchors.iter().any(|a| a == anchor)
    }

    /// True when `claimed` is a subset of the grant. Checked at Register.
    pub fn admits_capabilities(&self, claimed: &[String]) -> bool {
        claimed.iter().all(|c| self.can(c))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grant() -> NodeGrant {
        NodeGrant {
            producer_id: "home-pi-tv".into(),
            auth_subject: "avery/home-pi".into(),
            zones: vec!["living-room".into()],
            capabilities: vec!["canvas.display".into(), "audio.out".into()],
            screen_anchors: vec!["screen-tv".into()],
            site: "home-lan".into(),
            role: Role::Edge,
        }
    }

    #[test]
    fn namespaced_names_validate() {
        assert!(is_valid_namespaced_name("canvas.display"));
        assert!(is_valid_namespaced_name("sensor.temp-c"));
        assert!(!is_valid_namespaced_name("display"));
        assert!(!is_valid_namespaced_name("Canvas.display"));
        assert!(!is_valid_namespaced_name("canvas."));
        assert!(!is_valid_namespaced_name(".canvas"));
    }

    #[test]
    fn grant_admits_subset_only() {
        let g = grant();
        assert!(g.validate().is_ok());
        assert!(g.admits_capabilities(&["canvas.display".into()]));
        assert!(!g.admits_capabilities(&["canvas.display".into(), "presence.capture".into()]));
        assert!(g.serves("living-room", "audio.out"));
        assert!(!g.serves("bedroom", "audio.out"));
        assert!(g.claims_screen("screen-tv"));
        assert!(!g.claims_screen("screen-left"));
    }

    #[test]
    fn unknown_grant_fields_are_rejected() {
        let json = serde_json::to_value(grant()).unwrap();
        let mut obj = json.as_object().unwrap().clone();
        obj.insert("admin".into(), serde_json::json!(true));
        let parsed: Result<NodeGrant, _> = serde_json::from_value(serde_json::Value::Object(obj));
        assert!(parsed.is_err());
    }

    #[test]
    fn duplicates_and_blanks_fail() {
        let mut g = grant();
        g.zones = vec!["living-room".into(), "living-room".into()];
        assert!(matches!(g.validate(), Err(GrantError::Duplicate(_))));
        let mut g = grant();
        g.site = " ".into();
        assert!(matches!(g.validate(), Err(GrantError::InvalidName { .. })));
    }
}

#[cfg(test)]
mod slug_tests {
    use super::*;

    #[test]
    fn zones_and_anchors_are_slugs_but_capabilities_must_namespace() {
        let mut g = NodeGrant {
            producer_id: "home-pi-tv".into(),
            auth_subject: "avery/home-pi".into(),
            zones: vec!["living-room".into()],
            capabilities: vec!["display".into()],
            screen_anchors: vec!["screen-tv".into()],
            site: "home-lan".into(),
            role: Role::Edge,
        };
        assert!(matches!(
            g.validate(),
            Err(GrantError::InvalidName {
                field: "capability",
                ..
            })
        ));
        g.capabilities = vec!["canvas.display".into()];
        assert_eq!(g.validate(), Ok(()));
    }
}
