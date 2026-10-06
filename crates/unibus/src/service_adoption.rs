//! Shared lifecycle vocabulary for turning discovered services into active
//! protocol adapters. Protocol crates own probing and session semantics.

use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdoptionState {
    Discovered,
    Probing,
    Active,
    Degraded,
    Stale,
    Rejected,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdoptionStatus {
    pub service_id: String,
    pub state: AdoptionState,
    pub endpoint: Option<String>,
    pub provider_id: Option<String>,
    pub consecutive_failures: u32,
    pub last_error: Option<String>,
    pub last_probe_unix_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdoptionPolicy {
    pub local_node_id: String,
    pub max_failures: u32,
}

impl AdoptionPolicy {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            !self.local_node_id.trim().is_empty(),
            "local node id is empty"
        );
        ensure!(self.max_failures > 0, "max_failures must be positive");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policy_requires_a_named_node_and_a_nonzero_failure_bound() {
        assert!(AdoptionPolicy { local_node_id: "studio".into(), max_failures: 3 }.validate().is_ok());
        assert!(AdoptionPolicy { local_node_id: " ".into(), max_failures: 3 }.validate().is_err());
        assert!(AdoptionPolicy { local_node_id: "studio".into(), max_failures: 0 }.validate().is_err());
    }

    #[test]
    fn lifecycle_status_roundtrips_without_a_protocol_adapter() {
        let status = AdoptionStatus {
            service_id: "studio/canvas".into(), state: AdoptionState::Degraded,
            endpoint: Some("http://127.0.0.1:8000".into()), provider_id: None,
            consecutive_failures: 1, last_error: Some("unavailable".into()),
            last_probe_unix_ms: Some(123),
        };
        let decoded: AdoptionStatus = serde_json::from_value(serde_json::to_value(&status).unwrap()).unwrap();
        assert_eq!(decoded, status);
    }
}
