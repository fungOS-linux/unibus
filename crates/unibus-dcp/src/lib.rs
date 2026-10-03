//! DCP-over-Unibus binding.
//!
//! DCP owns decision semantics and canonical wire types. Unibus only leases
//! provider catalogs, routes revision-bound execute requests, and returns DCP
//! receipts. Providers remain authoritative; clients must not derive actions
//! from MCP tools or translate one protocol into the other.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Result, ensure};
use dcp::{Catalog, Discovery, ExecuteRequest, Receipt, VERSION};
use serde::{Deserialize, Serialize};
use unibus::Message;

pub mod http;

pub const BINDING_SCHEMA_VERSION: u32 = 1;
pub const CATALOG_KIND: &str = "dcp.catalog.v1";
pub const EXECUTE_KIND: &str = "dcp.execute.v1";
pub const RECEIPT_KIND: &str = "dcp.receipt.v1";
pub const PROVIDER_CAPABILITY: &str = "dcp.provider";
pub const CLIENT_CAPABILITY: &str = "dcp.client";
pub const MAX_PROVIDER_LEASE_MS: u64 = 120_000;
pub const MAX_CATALOG_BYTES: usize = 2 * 1024 * 1024;
pub const MAX_AUTHORIZATION_BYTES: usize = 16 * 1024;

/// A short-lived, state-derived DCP catalog announcement.
///
/// Repeated leases make discovery work on a non-retained bus without turning
/// Unibus into the owner of provider state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogAdvertisement {
    pub binding_schema_version: u32,
    pub node_id: String,
    pub producer_id: String,
    pub lease_id: String,
    pub issued_at_unix_ms: u64,
    pub expires_at_unix_ms: u64,
    pub discovery: Discovery,
    pub catalog: Catalog,
}

impl CatalogAdvertisement {
    pub fn validate(&self, now_unix_ms: u64) -> Result<()> {
        ensure!(
            self.binding_schema_version == BINDING_SCHEMA_VERSION,
            "unsupported DCP-over-Unibus binding schema"
        );
        for (label, value) in [
            ("node_id", self.node_id.as_str()),
            ("producer_id", self.producer_id.as_str()),
            ("lease_id", self.lease_id.as_str()),
        ] {
            ensure!(valid_identifier(value), "invalid {label}");
        }
        ensure!(
            self.issued_at_unix_ms <= now_unix_ms.saturating_add(5_000),
            "DCP catalog lease is future-dated"
        );
        ensure!(
            self.expires_at_unix_ms > now_unix_ms,
            "DCP catalog lease expired"
        );
        ensure!(
            self.expires_at_unix_ms
                .saturating_sub(self.issued_at_unix_ms)
                <= MAX_PROVIDER_LEASE_MS,
            "DCP catalog lease exceeds {MAX_PROVIDER_LEASE_MS} ms"
        );
        ensure!(
            self.discovery
                .versions
                .iter()
                .any(|version| version == VERSION),
            "provider does not advertise DCP {VERSION}"
        );
        ensure!(
            self.catalog.dcp_version == VERSION,
            "catalog is not DCP {VERSION}"
        );
        ensure!(
            self.discovery.provider == self.catalog.provider,
            "discovery and catalog providers differ"
        );
        ensure!(
            valid_identifier(&self.catalog.provider.id),
            "invalid DCP provider id"
        );
        ensure!(!self.catalog.actions.is_empty(), "DCP catalog is empty");
        ensure!(
            serde_json::to_vec(&self.catalog)?.len() <= MAX_CATALOG_BYTES,
            "DCP catalog is too large"
        );
        let mut action_ids = BTreeSet::new();
        for action in &self.catalog.actions {
            ensure!(valid_identifier(&action.id), "invalid DCP action id");
            ensure!(
                action_ids.insert(action.id.as_str()),
                "duplicate DCP action {}",
                action.id
            );
        }
        Ok(())
    }

    pub fn provider_id(&self) -> &str {
        &self.catalog.provider.id
    }

    pub fn into_message(self) -> Result<Message> {
        Ok(Message::new(CATALOG_KIND, serde_json::to_value(self)?))
    }
}

/// Authorization evidence is transport metadata, never part of a DCP catalog.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthorizationGrant {
    pub encoded: String,
}

impl AuthorizationGrant {
    fn validate(&self) -> Result<()> {
        ensure!(
            !self.encoded.trim().is_empty(),
            "authorization grant is empty"
        );
        ensure!(
            self.encoded.len() <= MAX_AUTHORIZATION_BYTES,
            "authorization grant is too large"
        );
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Execution {
    pub binding_schema_version: u32,
    pub provider_id: String,
    pub request: ExecuteRequest,
    pub authorization: AuthorizationGrant,
}

impl Execution {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.binding_schema_version == BINDING_SCHEMA_VERSION,
            "unsupported DCP-over-Unibus binding schema"
        );
        ensure!(valid_identifier(&self.provider_id), "invalid provider_id");
        ensure!(
            self.request.dcp_version == VERSION,
            "unsupported DCP version"
        );
        ensure!(
            valid_identifier(&self.request.request_id),
            "invalid request_id"
        );
        ensure!(
            valid_identifier(&self.request.decision_id),
            "invalid decision_id"
        );
        ensure!(
            valid_identifier(&self.request.action_id),
            "invalid action_id"
        );
        ensure!(
            !self.request.expected_catalog_revision.is_empty()
                && !self.request.expected_state_revision.is_empty(),
            "DCP execution must bind both revisions"
        );
        self.authorization.validate()
    }

    pub fn into_message(self) -> Result<Message> {
        self.validate()?;
        Ok(Message::new(EXECUTE_KIND, serde_json::to_value(self)?))
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionReceipt {
    pub binding_schema_version: u32,
    pub provider_id: String,
    pub receipt: Receipt,
}

impl ExecutionReceipt {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.binding_schema_version == BINDING_SCHEMA_VERSION,
            "unsupported DCP-over-Unibus binding schema"
        );
        ensure!(valid_identifier(&self.provider_id), "invalid provider_id");
        ensure!(
            self.receipt.dcp_version == VERSION,
            "unsupported DCP version"
        );
        ensure!(
            self.receipt.provider_id == self.provider_id,
            "receipt provider does not match route"
        );
        Ok(())
    }

    pub fn into_message(self) -> Result<Message> {
        self.validate()?;
        Ok(Message::new(RECEIPT_KIND, serde_json::to_value(self)?))
    }
}

#[derive(Debug, Default)]
pub struct ProviderRegistry {
    providers: BTreeMap<String, CatalogAdvertisement>,
}

impl ProviderRegistry {
    pub fn upsert(&mut self, advertisement: CatalogAdvertisement, now_unix_ms: u64) -> Result<()> {
        advertisement.validate(now_unix_ms)?;
        if let Some(current) = self.providers.get(advertisement.provider_id()) {
            ensure!(
                current.node_id == advertisement.node_id,
                "provider ownership changed"
            );
            ensure!(
                advertisement.issued_at_unix_ms >= current.issued_at_unix_ms,
                "stale DCP catalog advertisement"
            );
        }
        self.providers
            .insert(advertisement.provider_id().to_owned(), advertisement);
        Ok(())
    }

    pub fn prune(&mut self, now_unix_ms: u64) {
        self.providers
            .retain(|_, provider| provider.expires_at_unix_ms > now_unix_ms);
    }

    pub fn provider(&self, provider_id: &str, now_unix_ms: u64) -> Option<&CatalogAdvertisement> {
        self.providers
            .get(provider_id)
            .filter(|provider| provider.expires_at_unix_ms > now_unix_ms)
    }

    pub fn active(&self, now_unix_ms: u64) -> impl Iterator<Item = &CatalogAdvertisement> {
        self.providers
            .values()
            .filter(move |provider| provider.expires_at_unix_ms > now_unix_ms)
    }

    /// Resolve only an action advertised at the exact catalog and state
    /// revisions selected by the client. Providers still revalidate at commit.
    pub fn resolve_execution(
        &self,
        execution: &Execution,
        now_unix_ms: u64,
    ) -> Result<&dcp::Action> {
        execution.validate()?;
        let provider = self
            .provider(&execution.provider_id, now_unix_ms)
            .ok_or_else(|| anyhow::anyhow!("DCP provider is unavailable"))?;
        ensure!(
            provider.catalog.catalog_revision == execution.request.expected_catalog_revision,
            "stale DCP catalog revision"
        );
        ensure!(
            provider.catalog.state_revision == execution.request.expected_state_revision,
            "stale DCP state revision"
        );
        provider
            .catalog
            .actions
            .iter()
            .find(|action| action.id == execution.request.action_id)
            .ok_or_else(|| anyhow::anyhow!("action is not advertised by provider"))
    }
}

pub fn decode_message<T: for<'de> Deserialize<'de>>(
    message: &Message,
    expected_kind: &str,
) -> Result<T> {
    ensure!(
        message.kind == expected_kind,
        "expected {expected_kind}, got {}",
        message.kind
    );
    Ok(serde_json::from_value(serde_json::Value::Object(
        message.body.clone(),
    ))?)
}

fn valid_identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use dcp::{Action, Authorization, Availability, Endpoints, Evidence, Phase, Provider, Safety};
    use serde_json::json;

    fn advertisement(now: u64) -> CatalogAdvertisement {
        let provider = Provider {
            id: "philips-hue".into(),
            name: "Philips Hue".into(),
            instance: "home".into(),
        };
        CatalogAdvertisement {
            binding_schema_version: BINDING_SCHEMA_VERSION,
            node_id: "home-pi".into(),
            producer_id: "home-controls".into(),
            lease_id: "lease-1".into(),
            issued_at_unix_ms: now,
            expires_at_unix_ms: now + 30_000,
            discovery: Discovery {
                dcp: dcp::SPEC.into(),
                provider: provider.clone(),
                versions: vec![VERSION.into()],
                endpoints: Endpoints {
                    catalog: "/v1/decisions".into(),
                    execute: Some("/v1/decisions/execute".into()),
                    stream: None,
                },
                authorization: vec![Authorization {
                    scheme: "bearer".into(),
                    issuer: Some("https://auth.fpl.dev".into()),
                    scopes: vec!["home:lights".into()],
                }],
            },
            catalog: Catalog {
                dcp_version: VERSION.into(),
                provider,
                catalog_revision: "cat-1".into(),
                state_revision: "state-1".into(),
                observed_at: "2026-10-03T00:00:00Z".into(),
                expires_at: None,
                state: None,
                actions: vec![Action {
                    id: "hue.scene.activate".into(),
                    domain: "lighting".into(),
                    title: "Activate scene".into(),
                    description: "Activate one currently available Hue scene".into(),
                    input_schema: json!({
                        "$schema": dcp::SCHEMA_2020_12,
                        "type": "object",
                        "properties": {"scene_id":{"type":"string"}},
                        "required": ["scene_id"],
                        "additionalProperties": false
                    }),
                    phases: vec![Phase::Commit],
                    safety: Safety {
                        idempotent: true,
                        reversible: false,
                        requires_final: true,
                        confirmation_required: false,
                    },
                    availability: Availability {
                        available: true,
                        reason_code: None,
                        reason: None,
                    },
                }],
                tree: vec![],
            },
        }
    }

    fn execution() -> Execution {
        Execution {
            binding_schema_version: BINDING_SCHEMA_VERSION,
            provider_id: "philips-hue".into(),
            request: ExecuteRequest {
                dcp_version: VERSION.into(),
                request_id: "request-1".into(),
                decision_id: "decision-1".into(),
                action_id: "hue.scene.activate".into(),
                arguments: BTreeMap::from([("scene_id".into(), json!("relax"))]),
                phase: Phase::Commit,
                expected_catalog_revision: "cat-1".into(),
                expected_state_revision: "state-1".into(),
                prepared_receipt_id: None,
                confirmed: false,
                evidence: Evidence {
                    utterance_id: "utterance-1".into(),
                    transcript_revision: 4,
                    final_: true,
                },
            },
            authorization: AuthorizationGrant {
                encoded: "signed-grant".into(),
            },
        }
    }

    #[test]
    fn catalog_leases_expire_and_cannot_change_owner() {
        let mut registry = ProviderRegistry::default();
        registry.upsert(advertisement(1_000), 1_000).unwrap();
        assert!(registry.provider("philips-hue", 30_999).is_some());
        assert!(registry.provider("philips-hue", 31_000).is_none());

        let mut replacement = advertisement(40_000);
        replacement.node_id = "impostor".into();
        registry.upsert(advertisement(40_000), 40_000).unwrap();
        assert!(registry.upsert(replacement, 40_000).is_err());
    }

    #[test]
    fn execution_is_bound_to_advertised_revisions() {
        let mut registry = ProviderRegistry::default();
        registry.upsert(advertisement(1_000), 1_000).unwrap();
        assert_eq!(
            registry.resolve_execution(&execution(), 1_000).unwrap().id,
            "hue.scene.activate"
        );
        let mut stale = execution();
        stale.request.expected_state_revision = "state-old".into();
        assert!(registry.resolve_execution(&stale, 1_000).is_err());
    }

    #[test]
    fn messages_preserve_canonical_dcp_payloads() {
        let expected = advertisement(1_000);
        let message = expected.clone().into_message().unwrap();
        assert_eq!(
            decode_message::<CatalogAdvertisement>(&message, CATALOG_KIND).unwrap(),
            expected
        );
    }
}
