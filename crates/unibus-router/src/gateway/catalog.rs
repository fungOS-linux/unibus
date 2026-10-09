//! Optional file publisher using the existing provider-neutral service schema.
use super::GatewayCatalogSink;
use anyhow::{Context, Result, ensure};
use fpl_resource_observation::{
    Confidence, Provenance, SCHEMA_VERSION, Service, ServiceCatalog, ServiceId, ServiceObservation,
};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::{Read, Write},
    path::PathBuf,
};
use unibus::gateway::{GATEWAY_SERVICE_KIND, GatewayLease, identifier};

const MAX_DOCUMENT: usize = 65536;
pub struct OwnerCatalogFile {
    path: PathBuf,
    node: String,
}
impl OwnerCatalogFile {
    pub fn new(path: PathBuf, node: String) -> Result<Self> {
        identifier(&node)?;
        ensure!(
            node.len() <= 128 && path.is_absolute() && path.file_name().is_some(),
            "owner catalog requires explicit absolute file path and bounded node"
        );
        Ok(Self { path, node })
    }
}
pub fn catalog(node: &str, leases: &[GatewayLease], now: u64) -> Result<ServiceCatalog> {
    identifier(node)?;
    ensure!(node.len() <= 128, "owner catalog node exceeds bound");
    let mut observations = Vec::new();
    let mut seen = BTreeSet::new();
    ensure!(leases.len() <= 128, "gateway catalog exceeds bound");
    for lease in leases {
        lease.validate_at(now)?;
        lease.require_owner(node)?;
        let digest = Sha256::digest(serde_json::to_vec(&lease.resource)?);
        let hash: String = digest.iter().map(|b| format!("{b:02x}")).collect();
        let id = format!("service/{node}/unibus-gateway/{hash}");
        ensure!(
            seen.insert(id.clone()),
            "duplicate gateway resource in catalog"
        );
        let endpoints = lease.connection_hints()?;
        let mut protocols = BTreeSet::from(["unibus".into()]);
        protocols.extend(
            endpoints
                .iter()
                .filter_map(|e| e.split_once("://").map(|(s, _)| s.to_owned())),
        );
        observations.push(ServiceObservation {
            schema_version: SCHEMA_VERSION,
            service_id: ServiceId(id),
            observed_at: lease.observed_at_unix_s,
            expires_at: lease.expires_at_unix_s,
            provenance: Provenance {
                provider: "unibus-gateway-owner".into(),
                observer: node.into(),
                source_id: Some(lease.source_service_id.clone()),
            },
            confidence: Confidence::Derived,
            value: Service {
                kind: GATEWAY_SERVICE_KIND.into(),
                endpoints,
                protocols,
                formats: BTreeSet::new(),
                attributes: BTreeMap::from([
                    ("node_id".into(), node.into()),
                    ("gateway_lease".into(), serde_json::to_string(lease)?),
                ]),
            },
        });
        ensure!(
            serde_json::to_vec(observations.last().context("missing owner observation")?)?.len()
                <= 8192,
            "owner observation exceeds byte bound"
        );
    }
    let mut catalog = ServiceCatalog {
        schema_version: SCHEMA_VERSION,
        observations,
    };
    catalog.normalize();
    ensure!(
        serde_json::to_vec(&catalog)?.len() <= MAX_DOCUMENT,
        "owner catalog exceeds byte bound"
    );
    Ok(catalog)
}
impl GatewayCatalogSink for OwnerCatalogFile {
    fn publish(&self, leases: &[GatewayLease], now: u64) -> Result<()> {
        let document = catalog(&self.node, leases, now)?;
        // A configured path is dedicated to this publisher. Never replace
        // another application's catalog merely because it shares a directory.
        match std::fs::File::open(&self.path) {
            Ok(file) => {
                let mut bytes = Vec::new();
                file.take(MAX_DOCUMENT as u64 + 1).read_to_end(&mut bytes)?;
                ensure!(
                    bytes.len() <= MAX_DOCUMENT,
                    "existing owner file exceeds bound"
                );
                let previous: ServiceCatalog = serde_json::from_slice(&bytes)
                    .context("existing owner file is not a catalog")?;
                ensure!(
                    previous.schema_version == SCHEMA_VERSION,
                    "existing owner catalog has unsupported schema"
                );
                ensure!(
                    previous
                        .observations
                        .iter()
                        .all(|o| o.provenance.observer == self.node
                            && o.provenance.provider == "unibus-gateway-owner"
                            && o.value.kind == GATEWAY_SERVICE_KIND),
                    "refusing to overwrite another owner catalog"
                );
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        let parent = self.path.parent().context("owner file has no parent")?;
        let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            temporary
                .as_file()
                .set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
        temporary.write_all(&serde_json::to_vec(&document)?)?;
        temporary.as_file().sync_all()?;
        temporary.persist(&self.path).map_err(|e| e.error)?;
        #[cfg(unix)]
        std::fs::File::open(parent)?.sync_all()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn publisher_can_clear_its_file_but_not_another_owner() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("application-services.json");
        let publisher = OwnerCatalogFile::new(path.clone(), "peer".into()).unwrap();
        publisher.publish(&[], 100).unwrap();
        let doc: ServiceCatalog = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert!(doc.observations.is_empty());
        std::fs::write(&path,serde_json::to_vec(&serde_json::json!({"schema_version":1,"observations":[{"schema_version":1,"service_id":"other","observed_at":100,"expires_at":160,"provenance":{"provider":"other","observer":"peer"},"confidence":"strong","value":{"kind":"mcp"}}]})).unwrap()).unwrap();
        assert!(publisher.publish(&[], 100).is_err());
    }
}
