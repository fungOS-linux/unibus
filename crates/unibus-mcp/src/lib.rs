//! MCP-over-Unibus L2 contract.
//!
//! Existing MCP servers remain the owners of their tools. This crate defines
//! how an edge adapter leases a catalog into Unibus and how a bridge invokes
//! those tools without teaching either side about Blender, Ghidra, or any
//! other provider. Unibus remains the transport; authorization is verified by
//! an injected policy provider such as Mycelium.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use unibus::Message;

pub mod http;
pub mod server;

pub const MCP_SCHEMA_VERSION: u32 = 1;
pub const PROVIDER_KIND: &str = "mcp.provider.v1";
pub const INVOKE_KIND: &str = "mcp.invoke.v1";
pub const CANCEL_KIND: &str = "mcp.cancel.v1";
pub const PROGRESS_KIND: &str = "mcp.progress.v1";
pub const RESULT_KIND: &str = "mcp.result.v1";
pub const PROVIDER_CAPABILITY: &str = "mcp.provider";
pub const BRIDGE_CAPABILITY: &str = "mcp.bridge";
pub const MAX_PROVIDER_LEASE_MS: u64 = 120_000;
pub const MAX_CATALOG_BYTES: usize = 2 * 1024 * 1024;
pub const MAX_ARGUMENT_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_RESULT_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_AUTHORIZATION_BYTES: usize = 16 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MutationClass {
    ReadOnly,
    Mutating,
    /// Fail-closed classification for tools whose owner supplied no policy.
    Unknown,
}

impl MutationClass {
    pub fn requires_write_grant(self) -> bool {
        !matches!(self, Self::ReadOnly)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolDescriptor {
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub input_schema: Value,
    #[serde(default)]
    pub output_schema: Option<Value>,
    pub mutation: MutationClass,
}

impl ToolDescriptor {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            valid_mcp_name(&self.name),
            "invalid MCP tool name {}",
            self.name
        );
        ensure!(
            self.description.len() <= 16_384,
            "tool description is too large"
        );
        validate_schema(&self.input_schema, "input")?;
        if let Some(schema) = &self.output_schema {
            validate_schema(schema, "output")?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderAdvertisement {
    pub schema_version: u32,
    pub provider_id: String,
    pub node_id: String,
    /// Unibus producer to address for invocations. This is deliberately
    /// separate from the stable Mycelium node identity.
    pub producer_id: String,
    pub lease_id: String,
    pub issued_at_unix_ms: u64,
    pub expires_at_unix_ms: u64,
    pub catalog_digest: String,
    pub tools: Vec<ToolDescriptor>,
}

impl ProviderAdvertisement {
    pub fn new(
        provider_id: impl Into<String>,
        node_id: impl Into<String>,
        producer_id: impl Into<String>,
        lease_id: impl Into<String>,
        issued_at_unix_ms: u64,
        expires_at_unix_ms: u64,
        tools: Vec<ToolDescriptor>,
    ) -> Result<Self> {
        let catalog_digest = catalog_digest(&tools)?;
        let advertisement = Self {
            schema_version: MCP_SCHEMA_VERSION,
            provider_id: provider_id.into(),
            node_id: node_id.into(),
            producer_id: producer_id.into(),
            lease_id: lease_id.into(),
            issued_at_unix_ms,
            expires_at_unix_ms,
            catalog_digest,
            tools,
        };
        advertisement.validate(issued_at_unix_ms)?;
        Ok(advertisement)
    }

    pub fn validate(&self, now_unix_ms: u64) -> Result<()> {
        ensure!(
            self.schema_version == MCP_SCHEMA_VERSION,
            "unsupported MCP schema"
        );
        for (label, value) in [
            ("provider_id", &self.provider_id),
            ("node_id", &self.node_id),
            ("producer_id", &self.producer_id),
            ("lease_id", &self.lease_id),
        ] {
            ensure!(valid_identifier(value), "invalid {label}");
        }
        ensure!(!self.tools.is_empty(), "provider catalog is empty");
        ensure!(
            self.issued_at_unix_ms <= now_unix_ms.saturating_add(5_000),
            "lease is future-dated"
        );
        ensure!(
            self.expires_at_unix_ms > now_unix_ms,
            "provider lease expired"
        );
        ensure!(
            self.expires_at_unix_ms
                .saturating_sub(self.issued_at_unix_ms)
                <= MAX_PROVIDER_LEASE_MS,
            "provider lease exceeds {MAX_PROVIDER_LEASE_MS} ms"
        );
        let mut names = BTreeSet::new();
        for tool in &self.tools {
            tool.validate()?;
            ensure!(
                names.insert(tool.name.as_str()),
                "duplicate tool {}",
                tool.name
            );
        }
        ensure!(
            serde_json::to_vec(&self.tools)?.len() <= MAX_CATALOG_BYTES,
            "catalog is too large"
        );
        ensure!(
            self.catalog_digest == catalog_digest(&self.tools)?,
            "catalog digest mismatch"
        );
        Ok(())
    }

    pub fn into_message(self) -> Result<Message> {
        Ok(Message::new(PROVIDER_KIND, serde_json::to_value(self)?))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthorizationGrant {
    /// Compact, signed, audience-bound grant. It is authorization evidence,
    /// not a reusable provider credential.
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
pub struct Invocation {
    pub schema_version: u32,
    pub invocation_id: String,
    pub provider_id: String,
    pub tool_name: String,
    pub arguments: Value,
    pub authorization: AuthorizationGrant,
}

impl Invocation {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.schema_version == MCP_SCHEMA_VERSION,
            "unsupported MCP schema"
        );
        ensure!(
            valid_identifier(&self.invocation_id),
            "invalid invocation_id"
        );
        ensure!(valid_identifier(&self.provider_id), "invalid provider_id");
        ensure!(valid_mcp_name(&self.tool_name), "invalid tool_name");
        ensure!(
            self.arguments.is_object(),
            "tool arguments must be an object"
        );
        ensure!(
            serde_json::to_vec(&self.arguments)?.len() <= MAX_ARGUMENT_BYTES,
            "tool arguments are too large"
        );
        self.authorization.validate()
    }

    pub fn fingerprint(&self) -> Result<String> {
        self.validate()?;
        digest_json(&serde_json::json!({
            "provider_id": self.provider_id,
            "tool_name": self.tool_name,
            "arguments": self.arguments,
        }))
    }

    pub fn into_message(self) -> Result<Message> {
        self.validate()?;
        Ok(Message::new(INVOKE_KIND, serde_json::to_value(self)?))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Cancellation {
    pub schema_version: u32,
    pub invocation_id: String,
    #[serde(default)]
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Progress {
    pub schema_version: u32,
    pub invocation_id: String,
    pub sequence: u64,
    #[serde(default)]
    pub message: String,
    #[serde(default)]
    pub completed: Option<u64>,
    #[serde(default)]
    pub total: Option<u64>,
}

impl Progress {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.schema_version == MCP_SCHEMA_VERSION,
            "unsupported MCP schema"
        );
        ensure!(
            valid_identifier(&self.invocation_id),
            "invalid invocation_id"
        );
        ensure!(
            self.message.len() <= 16_384,
            "progress message is too large"
        );
        if let (Some(completed), Some(total)) = (self.completed, self.total) {
            ensure!(completed <= total, "progress exceeds total");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolResult {
    pub schema_version: u32,
    pub invocation_id: String,
    pub outcome: InvocationOutcome,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "status")]
pub enum InvocationOutcome {
    Success {
        result: Value,
    },
    Error {
        code: String,
        message: String,
        #[serde(default)]
        data: Option<Value>,
    },
    Cancelled {
        #[serde(default)]
        reason: String,
    },
}

impl ToolResult {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.schema_version == MCP_SCHEMA_VERSION,
            "unsupported MCP schema"
        );
        ensure!(
            valid_identifier(&self.invocation_id),
            "invalid invocation_id"
        );
        ensure!(
            serde_json::to_vec(&self.outcome)?.len() <= MAX_RESULT_BYTES,
            "tool result is too large"
        );
        if let InvocationOutcome::Error { code, message, .. } = &self.outcome {
            ensure!(valid_identifier(code), "invalid error code");
            ensure!(
                !message.is_empty() && message.len() <= 16_384,
                "invalid error message"
            );
        }
        Ok(())
    }
}

pub trait AuthorizationVerifier {
    type Claims;

    fn verify(
        &self,
        grant: &AuthorizationGrant,
        provider_id: &str,
        tool: &ToolDescriptor,
        now_unix_ms: u64,
    ) -> Result<Self::Claims>;
}

#[derive(Debug, Default)]
pub struct ProviderRegistry {
    providers: BTreeMap<String, ProviderAdvertisement>,
}

impl ProviderRegistry {
    pub fn upsert(&mut self, advertisement: ProviderAdvertisement, now_unix_ms: u64) -> Result<()> {
        advertisement.validate(now_unix_ms)?;
        if let Some(current) = self.providers.get(&advertisement.provider_id) {
            ensure!(
                current.node_id == advertisement.node_id,
                "provider ownership changed"
            );
            ensure!(
                advertisement.issued_at_unix_ms >= current.issued_at_unix_ms,
                "stale provider advertisement"
            );
        }
        self.providers
            .insert(advertisement.provider_id.clone(), advertisement);
        Ok(())
    }

    pub fn prune(&mut self, now_unix_ms: u64) {
        self.providers
            .retain(|_, provider| provider.expires_at_unix_ms > now_unix_ms);
    }

    pub fn provider(&self, provider_id: &str, now_unix_ms: u64) -> Option<&ProviderAdvertisement> {
        self.providers
            .get(provider_id)
            .filter(|provider| provider.expires_at_unix_ms > now_unix_ms)
    }

    pub fn active(&self, now_unix_ms: u64) -> impl Iterator<Item = &ProviderAdvertisement> {
        self.providers
            .values()
            .filter(move |provider| provider.expires_at_unix_ms > now_unix_ms)
    }

    /// All last-seen providers, including expired leases, for operational
    /// visibility. Callers must not use this iterator for routing.
    pub fn all(&self) -> impl Iterator<Item = &ProviderAdvertisement> {
        self.providers.values()
    }

    pub fn resolve_tool(
        &self,
        provider_id: &str,
        tool_name: &str,
        now_unix_ms: u64,
    ) -> Result<&ToolDescriptor> {
        self.provider(provider_id, now_unix_ms)
            .ok_or_else(|| anyhow::anyhow!("provider is unavailable"))?
            .tools
            .iter()
            .find(|tool| tool.name == tool_name)
            .ok_or_else(|| anyhow::anyhow!("tool is not advertised by provider"))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Claim {
    New,
    Duplicate,
}

#[derive(Debug)]
pub struct InvocationLedger {
    capacity: usize,
    entries: BTreeMap<String, LedgerEntry>,
}

#[derive(Debug)]
struct LedgerEntry {
    fingerprint: String,
    cancelled: bool,
}

impl InvocationLedger {
    pub fn new(capacity: usize) -> Result<Self> {
        ensure!(capacity > 0, "invocation ledger capacity must be positive");
        Ok(Self {
            capacity,
            entries: BTreeMap::new(),
        })
    }

    pub fn claim(&mut self, invocation: &Invocation) -> Result<Claim> {
        let fingerprint = invocation.fingerprint()?;
        if let Some(entry) = self.entries.get(&invocation.invocation_id) {
            ensure!(
                entry.fingerprint == fingerprint,
                "invocation id reused with different request"
            );
            return Ok(Claim::Duplicate);
        }
        ensure!(
            self.entries.len() < self.capacity,
            "invocation ledger is full"
        );
        self.entries.insert(
            invocation.invocation_id.clone(),
            LedgerEntry {
                fingerprint,
                cancelled: false,
            },
        );
        Ok(Claim::New)
    }

    pub fn cancel(&mut self, cancellation: &Cancellation) -> Result<bool> {
        ensure!(
            cancellation.schema_version == MCP_SCHEMA_VERSION,
            "unsupported MCP schema"
        );
        ensure!(
            valid_identifier(&cancellation.invocation_id),
            "invalid invocation_id"
        );
        ensure!(
            cancellation.reason.len() <= 16_384,
            "cancellation reason is too large"
        );
        let Some(entry) = self.entries.get_mut(&cancellation.invocation_id) else {
            return Ok(false);
        };
        entry.cancelled = true;
        Ok(true)
    }

    pub fn is_cancelled(&self, invocation_id: &str) -> bool {
        self.entries
            .get(invocation_id)
            .is_some_and(|entry| entry.cancelled)
    }

    pub fn complete(&mut self, invocation_id: &str) -> bool {
        self.entries.remove(invocation_id).is_some()
    }
}

pub fn catalog_digest(tools: &[ToolDescriptor]) -> Result<String> {
    for tool in tools {
        tool.validate()?;
    }
    digest_json(&serde_json::to_value(tools)?)
}

fn digest_json(value: &Value) -> Result<String> {
    let canonical = canonical_json(value)?;
    Ok(format!("sha256:{:x}", Sha256::digest(canonical)))
}

fn canonical_json(value: &Value) -> Result<Vec<u8>> {
    fn write(value: &Value, output: &mut Vec<u8>) -> Result<()> {
        match value {
            Value::Object(map) => {
                output.push(b'{');
                let mut entries = map.iter().collect::<Vec<_>>();
                entries.sort_by(|left, right| left.0.cmp(right.0));
                for (index, (key, value)) in entries.into_iter().enumerate() {
                    if index != 0 {
                        output.push(b',');
                    }
                    output.extend(serde_json::to_vec(key)?);
                    output.push(b':');
                    write(value, output)?;
                }
                output.push(b'}');
            }
            Value::Array(values) => {
                output.push(b'[');
                for (index, value) in values.iter().enumerate() {
                    if index != 0 {
                        output.push(b',');
                    }
                    write(value, output)?;
                }
                output.push(b']');
            }
            _ => output.extend(serde_json::to_vec(value)?),
        }
        Ok(())
    }
    let mut output = Vec::new();
    write(value, &mut output)?;
    Ok(output)
}

fn validate_schema(schema: &Value, label: &str) -> Result<()> {
    ensure!(schema.is_object(), "{label} schema must be an object");
    ensure!(
        serde_json::to_vec(schema)?.len() <= MAX_CATALOG_BYTES,
        "{label} schema is too large"
    );
    Ok(())
}

fn valid_identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':'))
}

fn valid_mcp_name(value: &str) -> bool {
    valid_identifier(value)
        || (!value.is_empty()
            && value.len() <= 128
            && value.bytes().all(|byte| {
                byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':' | b'/')
            }))
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
    Ok(serde_json::from_value(Value::Object(message.body.clone()))?)
}

pub fn require_tool_authorization<V: AuthorizationVerifier>(
    verifier: &V,
    registry: &ProviderRegistry,
    invocation: &Invocation,
    now_unix_ms: u64,
) -> Result<V::Claims> {
    invocation.validate()?;
    let tool =
        registry.resolve_tool(&invocation.provider_id, &invocation.tool_name, now_unix_ms)?;
    verifier.verify(
        &invocation.authorization,
        &invocation.provider_id,
        tool,
        now_unix_ms,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool(name: &str, mutation: MutationClass) -> ToolDescriptor {
        ToolDescriptor {
            name: name.into(),
            description: "test tool".into(),
            input_schema: serde_json::json!({"type":"object","properties":{"x":{"type":"integer"}}}),
            output_schema: None,
            mutation,
        }
    }

    fn advertisement(now: u64) -> ProviderAdvertisement {
        ProviderAdvertisement::new(
            "studio-ghidra",
            "mac-studio",
            "studio-mcp-provider",
            "lease-1",
            now,
            now + 30_000,
            vec![tool("decompile_function", MutationClass::ReadOnly)],
        )
        .unwrap()
    }

    fn invocation(id: &str, x: i64) -> Invocation {
        Invocation {
            schema_version: MCP_SCHEMA_VERSION,
            invocation_id: id.into(),
            provider_id: "studio-ghidra".into(),
            tool_name: "decompile_function".into(),
            arguments: serde_json::json!({"x":x}),
            authorization: AuthorizationGrant {
                encoded: "signed-grant".into(),
            },
        }
    }

    #[test]
    fn canonical_catalog_digest_ignores_object_key_order() {
        let left = vec![ToolDescriptor {
            input_schema: serde_json::from_str(r#"{"b":2,"a":1}"#).unwrap(),
            ..tool("x", MutationClass::ReadOnly)
        }];
        let right = vec![ToolDescriptor {
            input_schema: serde_json::from_str(r#"{"a":1,"b":2}"#).unwrap(),
            ..tool("x", MutationClass::ReadOnly)
        }];
        assert_eq!(
            catalog_digest(&left).unwrap(),
            catalog_digest(&right).unwrap()
        );
    }

    #[test]
    fn provider_leases_expire_and_cannot_change_owner() {
        let mut registry = ProviderRegistry::default();
        registry.upsert(advertisement(1_000), 1_000).unwrap();
        assert!(registry.provider("studio-ghidra", 30_999).is_some());
        assert!(registry.provider("studio-ghidra", 31_000).is_none());
        registry.prune(31_000);
        assert!(registry.providers.is_empty());

        let mut replacement = advertisement(40_000);
        replacement.node_id = "impostor".into();
        registry.upsert(advertisement(40_000), 40_000).unwrap();
        assert!(registry.upsert(replacement, 40_000).is_err());
    }

    #[test]
    fn invocation_ids_are_idempotent_but_not_reusable() {
        let mut ledger = InvocationLedger::new(2).unwrap();
        assert_eq!(ledger.claim(&invocation("call-1", 1)).unwrap(), Claim::New);
        assert_eq!(
            ledger.claim(&invocation("call-1", 1)).unwrap(),
            Claim::Duplicate
        );
        assert!(ledger.claim(&invocation("call-1", 2)).is_err());
        assert_eq!(ledger.claim(&invocation("call-2", 2)).unwrap(), Claim::New);
        assert!(ledger.claim(&invocation("call-3", 3)).is_err());
    }

    #[test]
    fn cancellation_is_correlated_and_idempotent() {
        let mut ledger = InvocationLedger::new(1).unwrap();
        ledger.claim(&invocation("call-1", 1)).unwrap();
        let cancel = Cancellation {
            schema_version: 1,
            invocation_id: "call-1".into(),
            reason: "user request".into(),
        };
        assert!(ledger.cancel(&cancel).unwrap());
        assert!(ledger.cancel(&cancel).unwrap());
        assert!(ledger.is_cancelled("call-1"));
        assert!(ledger.complete("call-1"));
        assert!(!ledger.is_cancelled("call-1"));
    }

    #[test]
    fn unknown_mutation_fails_closed() {
        assert!(!MutationClass::ReadOnly.requires_write_grant());
        assert!(MutationClass::Mutating.requires_write_grant());
        assert!(MutationClass::Unknown.requires_write_grant());
    }

    #[test]
    fn message_round_trip_preserves_contract() {
        let expected = advertisement(1_000);
        let message = expected.clone().into_message().unwrap();
        assert_eq!(
            decode_message::<ProviderAdvertisement>(&message, PROVIDER_KIND).unwrap(),
            expected
        );
    }

    struct TestVerifier;
    impl AuthorizationVerifier for TestVerifier {
        type Claims = &'static str;
        fn verify(
            &self,
            grant: &AuthorizationGrant,
            _: &str,
            tool: &ToolDescriptor,
            _: u64,
        ) -> Result<Self::Claims> {
            if grant.encoded == "allow-read" && !tool.mutation.requires_write_grant() {
                Ok("reader")
            } else {
                anyhow::bail!("denied")
            }
        }
    }

    #[test]
    fn authorization_is_checked_against_advertised_tool_policy() {
        let now = 1_000;
        let mut registry = ProviderRegistry::default();
        registry.upsert(advertisement(now), now).unwrap();
        let mut call = invocation("call-1", 1);
        call.authorization.encoded = "allow-read".into();
        assert_eq!(
            require_tool_authorization(&TestVerifier, &registry, &call, now).unwrap(),
            "reader"
        );
        call.authorization.encoded = "nope".into();
        assert!(require_tool_authorization(&TestVerifier, &registry, &call, now).is_err());
    }

    #[test]
    fn bounded_payloads_and_progress_fail_loudly() {
        let mut call = invocation("call-1", 1);
        call.arguments = serde_json::json!({"blob":"x".repeat(MAX_ARGUMENT_BYTES)});
        assert!(call.validate().is_err());
        let progress = Progress {
            schema_version: 1,
            invocation_id: "call-1".into(),
            sequence: 1,
            message: "working".into(),
            completed: Some(2),
            total: Some(1),
        };
        assert!(progress.validate().is_err());
    }
}
