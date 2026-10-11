//! Application-owned lease using the shared observation contract, no Mycelium
//! runtime dependency and no copied peer database or DNS-name authority.
use anyhow::{Result, ensure};
use fpl_resource_observation::{
    Confidence, Provenance, SCHEMA_VERSION, Service, ServiceCatalog, ServiceId, ServiceObservation,
};
use serde::Deserialize;
use std::{
    collections::{BTreeMap, BTreeSet},
    io::{Read, Write},
    net::SocketAddr,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Advertisement {
    pub node_id: String,
    pub path: PathBuf,
}
impl Advertisement {
    pub fn validate(&self, address: SocketAddr) -> Result<()> {
        ensure!(
            !self.node_id.is_empty()
                && self.node_id.len() <= 128
                && self
                    .node_id
                    .bytes()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || b"-_.:".contains(&c)),
            "invalid advertisement node identity"
        );
        ensure!(
            self.path.is_absolute() && self.path.extension().is_some_and(|ext| ext == "json"),
            "advertisement needs an absolute JSON owner path"
        );
        ensure!(
            !address.ip().is_loopback()
                && !address.ip().is_unspecified()
                && !address.ip().is_multicast(),
            "loopback viewer cannot advertise a remote entrance"
        );
        Ok(())
    }
    fn catalog(&self, address: SocketAddr, now: u64) -> ServiceCatalog {
        ServiceCatalog {
            schema_version: SCHEMA_VERSION,
            observations: vec![ServiceObservation {
                schema_version: SCHEMA_VERSION,
                service_id: ServiceId(format!("service/{}/unibus/viewer", self.node_id)),
                observed_at: now,
                expires_at: now.saturating_add(120),
                provenance: Provenance {
                    provider: "unibus-viewer".into(),
                    observer: self.node_id.clone(),
                    source_id: None,
                },
                confidence: Confidence::Strong,
                value: Service {
                    kind: "unibus-viewer".into(),
                    endpoints: BTreeSet::from([format!("https://{address}")]),
                    protocols: BTreeSet::from(["https".into(), "mtls".into()]),
                    formats: BTreeSet::from(["html".into(), "json".into()]),
                    attributes: BTreeMap::from([
                        ("node_id".into(), self.node_id.clone()),
                        ("authentication".into(), "mtls-grant".into()),
                    ]),
                },
            }],
        }
    }
    fn publish(&self, address: SocketAddr) -> Result<()> {
        self.validate(address)?;
        let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
        let catalog = self.catalog(address, now);
        if let Ok(metadata) = std::fs::symlink_metadata(&self.path) {
            ensure!(
                metadata.is_file() && !metadata.file_type().is_symlink(),
                "refusing nonregular owner document"
            );
            let mut bytes = Vec::new();
            std::fs::File::open(&self.path)?
                .take(65537)
                .read_to_end(&mut bytes)?;
            ensure!(
                bytes.len() <= 65536,
                "existing owner document exceeds bound"
            );
            let existing: ServiceCatalog = serde_json::from_slice(&bytes)?;
            ensure!(
                existing.observations.len() == 1
                    && existing.observations[0].service_id == catalog.observations[0].service_id
                    && existing.observations[0].provenance.observer == self.node_id
                    && existing.observations[0].provenance.provider == "unibus-viewer",
                "refusing another application's owner document"
            );
        }
        let parent = self
            .path
            .parent()
            .ok_or_else(|| anyhow::anyhow!("owner path needs parent"))?;
        // Directory provisioning stays in the deployment runbook, not this process.
        let mut file = tempfile::NamedTempFile::new_in(parent)?;
        file.write_all(&serde_json::to_vec(&catalog)?)?;
        file.as_file().sync_all()?;
        file.persist(&self.path)?;
        Ok(())
    }
}
pub(super) fn start(
    ad: Advertisement,
    address: SocketAddr,
    config_path: PathBuf,
) -> Result<Arc<AtomicBool>> {
    ad.publish(address)?;
    let alive = Arc::new(AtomicBool::new(true));
    let running = alive.clone();
    std::thread::Builder::new()
        .name("unibus-viewer-lease".into())
        .spawn(move || {
            loop {
                std::thread::park_timeout(Duration::from_secs(30));
                if !running.load(Ordering::Acquire) {
                    break;
                }
                // Stop renewal on disable or any policy change. The prior lease expires;
                // never restart a removed advertisement silently or guess a new route.
                let same = super::read_config(&config_path).ok().is_some_and(|c| {
                    c.listen == address
                        && c.advertisement
                            .is_some_and(|a| a.path == ad.path && a.node_id == ad.node_id)
                });
                if !same {
                    tracing::warn!("viewer advertisement configuration changed; renewal stopped");
                    break;
                }
                if let Err(error) = ad.publish(address) {
                    tracing::error!(%error,"viewer advertisement renewal failed");
                    break;
                }
            }
        })?;
    Ok(alive)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bounded_identity_and_no_loopback_promotion() {
        let ad = Advertisement {
            node_id: "neo".into(),
            path: "/private/application-services.d/unibus-viewer.json".into(),
        };
        assert!(ad.validate("127.0.0.1:8444".parse().unwrap()).is_err());
        ad.validate("192.0.2.12:8444".parse().unwrap()).unwrap();
        let c = ad.catalog("192.0.2.12:8444".parse().unwrap(), 1000);
        assert_eq!(c.observations[0].expires_at, 1120);
        assert_eq!(c.observations[0].provenance.observer, "neo");
    }
    #[test]
    fn renewal_preserves_ownership_and_refuses_foreign_files() {
        let dir = tempfile::tempdir().unwrap();
        let ad = Advertisement {
            node_id: "neo".into(),
            path: dir.path().join("unibus-viewer.json"),
        };
        let address = "192.0.2.12:8444".parse().unwrap();
        ad.publish(address).unwrap();
        ad.publish(address).unwrap();
        let other = Advertisement {
            node_id: "studio".into(),
            path: ad.path.clone(),
        };
        assert!(other.publish(address).is_err());
        let c: ServiceCatalog = serde_json::from_slice(&std::fs::read(ad.path).unwrap()).unwrap();
        assert_eq!(c.observations[0].provenance.observer, "neo");
    }
}
