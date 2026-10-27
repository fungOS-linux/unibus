//! Bounded OpenAPI/RPC target index for Unibus connectors.
//!
//! This crate deliberately does not mint grants or capabilities. It turns an
//! external API description into stable, reviewable *targets* that an ACL may
//! reference. Authority remains in Mycelium; enforcement remains in Unibus.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

pub const SCHEMA_VERSION: u32 = 1;
pub const MAX_TARGETS: usize = 4096;
pub const MAX_DOCUMENT: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceKind {
    Openapi,
    Rpc,
    Manual,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Source {
    pub kind: SourceKind,
    pub location: String,
    /// Lowercase hex SHA-256 of the source document.
    pub digest: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "transport", rename_all = "snake_case")]
pub enum Transport {
    Http { method: String, path: String },
    Rpc { service: String, method: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Target {
    /// Stable, namespaced id. Unibus ACLs reference this.
    pub id: String,
    /// Unibus message kind. Usually equal to `id`.
    pub kind: String,
    pub operation_id: String,
    #[serde(flatten)]
    pub transport: Transport,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_schema: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_schema: Option<String>,
    /// Presentation hint only. It is not an ACL decision.
    pub risk: Risk,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Risk {
    Read,
    Write,
    Destructive,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TargetIndex {
    pub schema_version: u32,
    pub source: Source,
    pub targets: Vec<Target>,
}

impl TargetIndex {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.schema_version == SCHEMA_VERSION,
            "unsupported target index schema"
        );
        ensure!(
            !self.source.location.is_empty() && self.source.location.len() <= 512,
            "invalid source location"
        );
        ensure!(
            self.source.digest.len() == 64
                && self
                    .source
                    .digest
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit()),
            "invalid source digest"
        );
        ensure!(
            !self.targets.is_empty() && self.targets.len() <= MAX_TARGETS,
            "target index must contain 1..={MAX_TARGETS} targets"
        );
        let mut ids = BTreeSet::new();
        for target in &self.targets {
            validate_target(target)?;
            ensure!(ids.insert(&target.id), "duplicate target id: {}", target.id);
        }
        ensure!(
            serde_json::to_vec(self)?.len() <= MAX_DOCUMENT,
            "target index exceeds {MAX_DOCUMENT} bytes"
        );
        Ok(())
    }

    pub fn get(&self, id: &str) -> Option<&Target> {
        self.targets.iter().find(|target| target.id == id)
    }

    pub fn by_kind(&self) -> BTreeMap<&str, &Target> {
        self.targets
            .iter()
            .map(|target| (target.kind.as_str(), target))
            .collect()
    }

    pub fn to_json(&self) -> Result<String> {
        self.validate()?;
        Ok(serde_json::to_string_pretty(self)?)
    }
}

fn validate_target(target: &Target) -> Result<()> {
    ensure_namespaced(&target.id, "target id")?;
    ensure_namespaced(&target.kind, "target kind")?;
    ensure!(
        !target.operation_id.is_empty()
            && target.operation_id.len() <= 128
            && !target.operation_id.chars().any(char::is_control),
        "invalid operation id for {}",
        target.id
    );
    match &target.transport {
        Transport::Http { method, path } => {
            ensure!(
                matches!(
                    method.as_str(),
                    "GET" | "HEAD" | "POST" | "PUT" | "PATCH" | "DELETE" | "OPTIONS"
                ),
                "unsupported HTTP method: {method}"
            );
            ensure!(
                path.starts_with('/') && path.len() <= 2048 && !path.contains("://"),
                "invalid HTTP path: {path}"
            );
        }
        Transport::Rpc { service, method } => {
            ensure!(
                !service.is_empty()
                    && service.len() <= 256
                    && service.contains('.')
                    && service.chars().all(|character| {
                        character.is_ascii_alphanumeric()
                            || character == '.'
                            || character == '_'
                            || character == '-'
                    }),
                "invalid RPC service: {service}"
            );
            ensure!(
                !method.is_empty()
                    && method.len() <= 128
                    && method.chars().all(|character| {
                        character.is_ascii_alphanumeric() || character == '_' || character == '-'
                    }),
                "invalid RPC method: {method}"
            );
        }
    }
    for schema in [&target.request_schema, &target.response_schema]
        .into_iter()
        .flatten()
    {
        ensure!(
            !schema.is_empty() && schema.len() <= 512 && !schema.chars().any(char::is_control),
            "invalid schema reference"
        );
    }
    Ok(())
}

fn ensure_namespaced(value: &str, field: &str) -> Result<()> {
    ensure!(
        !value.is_empty()
            && value.len() <= 128
            && value.contains('.')
            && value.split('.').all(|segment| {
                !segment.is_empty()
                    && segment.chars().all(|character| {
                        character.is_ascii_lowercase()
                            || character.is_ascii_digit()
                            || character == '_'
                            || character == '-'
                    })
            }),
        "invalid {field}: {value}"
    );
    Ok(())
}

/// Build an index from an OpenAPI 3.x YAML or JSON document.
///
/// `service` is the namespace for target ids, e.g. `billing`.
pub fn from_openapi(service: &str, location: &str, document: &str) -> Result<TargetIndex> {
    ensure_namespaced(&format!("{service}.probe"), "service namespace")?;
    let root: Value = serde_yaml::from_str(document)
        .or_else(|_| serde_json::from_str(document))
        .map_err(|error| anyhow::anyhow!("parse OpenAPI document: {error}"))?;
    let version = root["openapi"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("not an OpenAPI 3 document"))?;
    ensure!(
        version.starts_with("3."),
        "unsupported OpenAPI version: {version}"
    );
    let paths = root["paths"]
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("OpenAPI document has no paths"))?;

    let mut targets = Vec::new();
    let mut ids = BTreeSet::new();
    for (path, item) in paths {
        let Some(item) = item.as_object() else {
            continue;
        };
        for (method, operation) in item {
            let method = method.to_ascii_uppercase();
            if !matches!(
                method.as_str(),
                "GET" | "HEAD" | "POST" | "PUT" | "PATCH" | "DELETE" | "OPTIONS"
            ) {
                continue;
            }
            let operation_id = operation["operationId"]
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| format!("{}{}", method.to_ascii_lowercase(), slug_path(path)));
            let id = format!("{service}.{}", slug(&operation_id));
            ensure!(
                ids.insert(id.clone()),
                "duplicate OpenAPI operation id: {operation_id}"
            );
            let request_schema = operation
                .pointer("/requestBody/content/application~1json/schema/$ref")
                .and_then(Value::as_str)
                .map(str::to_owned);
            let response_schema = operation
                .pointer("/responses/200/content/application~1json/schema/$ref")
                .and_then(Value::as_str)
                .or_else(|| {
                    operation
                        .pointer("/responses/201/content/application~1json/schema/$ref")
                        .and_then(Value::as_str)
                })
                .map(str::to_owned);
            targets.push(Target {
                kind: id.clone(),
                id,
                operation_id,
                transport: Transport::Http {
                    method: method.clone(),
                    path: path.clone(),
                },
                request_schema,
                response_schema,
                risk: match method.as_str() {
                    "GET" | "HEAD" | "OPTIONS" => Risk::Read,
                    "DELETE" => Risk::Destructive,
                    _ => Risk::Write,
                },
            });
        }
    }
    targets.sort_by(|left, right| left.id.cmp(&right.id));
    let digest = Sha256::digest(document.as_bytes());
    let index = TargetIndex {
        schema_version: SCHEMA_VERSION,
        source: Source {
            kind: SourceKind::Openapi,
            location: location.to_owned(),
            digest: digest.iter().map(|byte| format!("{byte:02x}")).collect(),
        },
        targets,
    };
    index.validate()?;
    Ok(index)
}

fn slug(value: &str) -> String {
    let mut output = String::new();
    let mut dash = true;
    let mut previous_lower_or_digit = false;
    for character in value.chars() {
        if character.is_ascii_uppercase() && previous_lower_or_digit && !dash {
            output.push('-');
        }
        if character.is_ascii_alphanumeric() {
            output.extend(character.to_lowercase());
            dash = false;
            previous_lower_or_digit = character.is_ascii_lowercase() || character.is_ascii_digit();
        } else if !dash {
            output.push('-');
            dash = true;
            previous_lower_or_digit = false;
        }
    }
    output.trim_matches('-').to_owned()
}

fn slug_path(path: &str) -> String {
    let segments: Vec<String> = path
        .split('/')
        .filter(|segment| !segment.is_empty())
        .map(|segment| {
            if let Some(parameter) = segment.strip_prefix('{').and_then(|s| s.strip_suffix('}')) {
                format!("by-{parameter}")
            } else {
                segment.to_owned()
            }
        })
        .collect();
    format!(".{}", slug(&segments.join("-")))
}

/// Build an RPC target manually. Descriptor ingestion can populate this later
/// without changing the index shape.
pub fn rpc_target(
    service: &str,
    rpc_service: &str,
    rpc_method: &str,
    operation_id: &str,
    request_schema: Option<String>,
    response_schema: Option<String>,
    risk: Risk,
) -> Result<Target> {
    let id = format!("{}.{}", slug(service), slug(operation_id));
    let target = Target {
        kind: id.clone(),
        id,
        operation_id: operation_id.to_owned(),
        transport: Transport::Rpc {
            service: rpc_service.to_owned(),
            method: rpc_method.to_owned(),
        },
        request_schema,
        response_schema,
        risk,
    };
    validate_target(&target)?;
    Ok(target)
}

#[cfg(test)]
mod tests {
    use super::*;

    const OPENAPI: &str = include_str!("../tests/fixtures/billing.openapi.yaml");

    #[test]
    fn openapi_produces_stable_targets_without_grants() {
        let index = from_openapi("billing", "billing.openapi.yaml", OPENAPI).unwrap();
        assert_eq!(
            index
                .targets
                .iter()
                .map(|t| t.id.as_str())
                .collect::<Vec<_>>(),
            vec![
                "billing.create-invoice",
                "billing.delete-invoice",
                "billing.get-invoice",
                "billing.list-invoices"
            ]
        );
        assert_eq!(
            index.targets[0].transport,
            Transport::Http {
                method: "POST".into(),
                path: "/v1/invoices".into()
            }
        );
        assert_eq!(
            index.get("billing.delete-invoice").unwrap().risk,
            Risk::Destructive
        );
        assert_eq!(index.source.digest.len(), 64);
        assert!(index.get("billing.get-invoice").is_some());
        // The index contains no authority shape at all.
        let json = serde_json::to_string(&index).unwrap();
        assert!(!json.contains("grant"));
        assert!(!json.contains("capabilit"));
    }

    #[test]
    fn duplicate_operation_ids_are_rejected() {
        let document = r#"
openapi: 3.1.0
paths:
  /a:
    get:
      operationId: same
  /b:
    get:
      operationId: same
"#;
        assert!(from_openapi("demo", "demo.yaml", document).is_err());
    }

    #[test]
    fn rpc_targets_use_the_same_index_shape() {
        let target = rpc_target(
            "billing",
            "billing.v1.Invoices",
            "Get",
            "getInvoice",
            None,
            Some("billing.invoice.v1".into()),
            Risk::Read,
        )
        .unwrap();
        assert_eq!(target.id, "billing.get-invoice");
        assert_eq!(target.kind, target.id);
    }
}
