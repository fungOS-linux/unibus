//! Relays carry encrypted packets, not application authorization.
//! Mycelium (or another ServiceDirectory) owns observation and discovery.
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use unibus::{ServiceDirectory, ServiceObservation};

pub const SERVICE_KIND: &str = "iroh-relay";
pub const CATALOG: &str = "unibus.iroh.gateways.v1";
pub const PEER_ALPN: &str = "unibus/peer/1";
pub mod bus;
pub mod directory;
#[cfg(feature = "fleet-identity")]
pub mod identity;
#[cfg(feature = "transport")]
pub mod transport;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Gateway {
    pub service_id: String,
    pub relay_urls: Vec<String>,
    pub observed_at_unix_s: u64,
    pub expires_at_unix_s: u64,
    pub provenance_provider: String,
    pub provenance_observer: String,
}

impl Gateway {
    pub fn from_observation(observation: ServiceObservation, now: u64) -> Result<Self> {
        observation.validate()?;
        ensure!(
            observation.kind == SERVICE_KIND,
            "not an Iroh relay observation"
        );
        ensure!(
            observation.is_fresh_at(now),
            "expired or future relay observation"
        );
        for endpoint in &observation.endpoints {
            let url = url::Url::parse(endpoint)?;
            ensure!(url.scheme() == "https", "relay discovery requires HTTPS");
            ensure!(
                url.host_str().is_some()
                    && url.username().is_empty()
                    && url.password().is_none()
                    && url.query().is_none()
                    && url.fragment().is_none()
                    && url.path() == "/",
                "relay URL must be a credential-free HTTPS origin"
            );
        }
        let mut relay_urls = observation.endpoints;
        relay_urls.sort();
        relay_urls.dedup();
        Ok(Self {
            service_id: observation.service_id,
            relay_urls,
            observed_at_unix_s: observation.observed_at_unix_s,
            expires_at_unix_s: observation.expires_at_unix_s,
            provenance_provider: observation.provenance_provider,
            provenance_observer: observation.provenance_observer,
        })
    }
}

/// Fail closed on invalid observations rather than masking directory corruption.
pub fn gateways(directory: &dyn ServiceDirectory, now: u64) -> Result<Vec<Gateway>> {
    let mut result = directory
        .services(SERVICE_KIND)?
        .into_iter()
        .filter(|observation| observation.is_fresh_at(now))
        .map(|observation| Gateway::from_observation(observation, now))
        .collect::<Result<Vec<_>>>()?;
    result.sort_by(|a, b| a.service_id.cmp(&b.service_id));
    ensure!(
        result.len() <= 64,
        "gateway catalog exceeds bounded capacity"
    );
    result.dedup_by(|a, b| a.service_id == b.service_id && a == b);
    for pair in result.windows(2) {
        ensure!(
            pair[0].service_id != pair[1].service_id,
            "conflicting relay observations"
        );
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn observation(endpoint: &str) -> ServiceObservation {
        ServiceObservation {
            service_id: "agora-iroh".into(),
            kind: SERVICE_KIND.into(),
            endpoints: vec![endpoint.into()],
            observed_at_unix_s: 100,
            expires_at_unix_s: 200,
            provenance_provider: "mycelium".into(),
            provenance_observer: "agora-one".into(),
            attributes: Default::default(),
        }
    }
    #[test]
    fn valid_origin_retains_provenance() {
        let gateway =
            Gateway::from_observation(observation("https://relay.example.net"), 150).unwrap();
        assert_eq!(gateway.provenance_provider, "mycelium");
        assert_eq!(gateway.expires_at_unix_s, 200);
    }
    #[test]
    fn rejects_credentials_and_insecure_origins() {
        for endpoint in [
            "http://relay.example.net",
            "https://secret@relay.example.net",
            "https://relay.example.net?token=secret",
            "https://relay.example.net/path",
            "https://relay.example.net#secret",
        ] {
            assert!(
                Gateway::from_observation(observation(endpoint), 150).is_err(),
                "{endpoint}"
            );
        }
    }
    #[test]
    fn leases_are_checked() {
        assert!(Gateway::from_observation(observation("https://relay.example.net"), 99).is_err());
        assert!(Gateway::from_observation(observation("https://relay.example.net"), 200).is_err());
    }
    #[test]
    fn catalog_is_replaceable_latest_value() {
        assert_eq!(unibus::Qos::of_kind(CATALOG), unibus::Qos::Snapshot);
    }
    #[test]
    fn directory_returns_only_current_gateway_observations() {
        let mut expired = observation("https://old.example.net");
        expired.service_id = "old-relay".into();
        expired.expires_at_unix_s = 140;
        let directory = unibus::StaticServiceDirectory::new(vec![
            expired,
            observation("https://relay.example.net"),
        ])
        .unwrap();
        let found = gateways(&directory, 150).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].service_id, "agora-iroh");
        assert_eq!(found[0].relay_urls, vec!["https://relay.example.net"]);
    }
    struct Conflicting;
    impl unibus::ServiceDirectory for Conflicting {
        fn services(
            &self,
            _: &str,
        ) -> std::result::Result<Vec<ServiceObservation>, unibus::NetworkEndpointError> {
            Ok(vec![
                observation("https://one.example.net"),
                observation("https://two.example.net"),
            ])
        }
    }
    #[test]
    fn conflicting_gateway_identity_fails_closed() {
        assert!(gateways(&Conflicting, 150).is_err());
    }
}
