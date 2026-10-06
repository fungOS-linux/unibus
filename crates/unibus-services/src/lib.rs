//! Explicit-source, read-only inventory client. No discovery or process control.

use std::{
    io::Read,
    net::IpAddr,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, ensure};
use reqwest::{Url, blocking::Client, redirect::Policy};
use serde::{Serialize, de::DeserializeOwned};
use stream_descriptors::service::{HealthState, ServiceCatalog, ServiceHealth};

const MAX_DOCUMENT: u64 = 1024 * 1024;

#[derive(Debug, Serialize)]
pub struct Inspection {
    pub catalog: Option<ServiceCatalog>,
    pub health: ServiceHealth,
    pub effective_state: HealthState,
}

/// HTTP is permitted only on an explicit private/loopback IP. Across sites,
/// use an authenticated private route or local tunnel; never expose the proxy.
pub fn validate_source(source: &str) -> Result<Url> {
    let url = Url::parse(source)?;
    ensure!(
        url.scheme() == "http"
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none()
            && url.path() == "/",
        "source must be a bare private HTTP origin"
    );
    let host = url.host_str().context("missing source host")?;
    let ip: IpAddr = host
        .trim_matches(['[', ']'])
        .parse()
        .context("source must use an explicit IP, not DNS")?;
    let private = match ip {
        IpAddr::V4(ip) => {
            ip.is_private()
                || ip.is_loopback()
                || (ip.octets()[0] == 100 && (64..=127).contains(&ip.octets()[1]))
        }
        IpAddr::V6(ip) => ip.is_loopback() || ip.is_unique_local(),
    };
    ensure!(private, "source must be private or loopback");
    Ok(url)
}

fn document<T: DeserializeOwned>(client: &Client, base: &Url, path: &str) -> Result<(u16, T)> {
    let response = client.get(base.join(path)?).send()?;
    let status = response.status().as_u16();
    ensure!(
        status == 200 || status == 503,
        "service endpoint returned HTTP {status}"
    );
    let mut body = Vec::new();
    response.take(MAX_DOCUMENT + 1).read_to_end(&mut body)?;
    ensure!(
        body.len() as u64 <= MAX_DOCUMENT,
        "service document exceeds size limit"
    );
    Ok((status, serde_json::from_slice(&body)?))
}

pub fn inspect(source: &str) -> Result<Inspection> {
    let base = validate_source(source)?;
    let client = Client::builder()
        .no_proxy()
        .redirect(Policy::none())
        .timeout(Duration::from_secs(5))
        .connect_timeout(Duration::from_secs(2))
        .build()?;
    let (health_status, health): (_, ServiceHealth) = document(&client, &base, "v1/health")?;
    health.validate().map_err(anyhow::Error::msg)?;
    let now: u64 = SystemTime::now()
        .duration_since(UNIX_EPOCH)?
        .as_millis()
        .try_into()?;
    let effective_state = health.effective_state(now);
    ensure!(
        health_status != 503 || effective_state != HealthState::Healthy,
        "unavailable service reports healthy evidence"
    );
    // Stale health remains visible even when its catalog has been withdrawn.
    let catalog = if effective_state == HealthState::Stale {
        None
    } else {
        let (status, catalog): (_, ServiceCatalog) = document(&client, &base, "v1/catalog")?;
        ensure!(status == 200, "catalog unavailable");
        catalog.validate().map_err(anyhow::Error::msg)?;
        ensure!(
            catalog.is_current(now),
            "catalog evidence expired or is future-dated"
        );
        ensure!(
            catalog.service_id == health.service_id,
            "catalog and health belong to different services"
        );
        Some(catalog)
    };
    Ok(Inspection {
        catalog,
        health,
        effective_state,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_sources_reject_credentials_redirect_targets_and_public_hosts() {
        for source in [
            "http://192.168.2.74:8088",
            "http://127.0.0.1:8088",
            "http://[::1]:8088",
            "http://100.85.154.96:8088",
        ] {
            assert!(validate_source(source).is_ok(), "{source}");
        }
        for source in [
            "http://example.com",
            "http://8.8.8.8",
            "http://user:pass@192.168.2.74",
            "http://192.168.2.74/?token=secret",
            "http://192.168.2.74/#fragment",
            "http://192.168.2.74/admin",
            "file:///etc/passwd",
            "https://192.168.2.74",
        ] {
            assert!(validate_source(source).is_err(), "{source}");
        }
    }

    #[test]
    fn healthy_inspection_checks_catalog_provenance_and_lists_resources() {
        use std::{io::Write, net::TcpListener};
        use stream_descriptors::service::{
            SERVICE_CATALOG_KIND, SERVICE_HEALTH_KIND, ServiceResource,
        };
        for mismatched in [false, true] {
            let now: u64 = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_millis()
                .try_into()
                .unwrap();
            let health = ServiceHealth {
                contract: SERVICE_HEALTH_KIND.into(),
                service_id: "radio.one".into(),
                state: HealthState::Healthy,
                reason: None,
                observed_at_unix_ms: now,
                valid_until_unix_ms: now + 60_000,
            };
            let catalog = ServiceCatalog {
                contract: SERVICE_CATALOG_KIND.into(),
                service_id: if mismatched {
                    "radio.other"
                } else {
                    "radio.one"
                }
                .into(),
                adapter_kind: "radioman".into(),
                owner_reference: "radio-pi".into(),
                presenter_reference: "radio-pi".into(),
                observed_at_unix_ms: now,
                valid_until_unix_ms: now + 60_000,
                resources: vec![ServiceResource {
                    id: "rtl-sdr.1".into(),
                    kind: "radio.receiver".into(),
                    display_name: "RTL-SDR".into(),
                    stream_ids: vec![],
                    operations: vec![],
                }],
            };
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let address = listener.local_addr().unwrap();
            let server = std::thread::spawn(move || {
                for (path, body) in [
                    ("/v1/health", serde_json::to_string(&health).unwrap()),
                    ("/v1/catalog", serde_json::to_string(&catalog).unwrap()),
                ] {
                    let (mut stream, _) = listener.accept().unwrap();
                    let mut request = [0; 4096];
                    let length = stream.read(&mut request).unwrap();
                    assert!(
                        std::str::from_utf8(&request[..length])
                            .unwrap()
                            .starts_with(&format!("GET {path} "))
                    );
                    write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body).unwrap();
                }
            });
            let result = inspect(&format!("http://{address}"));
            if mismatched {
                assert!(result.is_err());
            } else {
                let result = result.unwrap();
                assert_eq!(result.effective_state, HealthState::Healthy);
                assert_eq!(result.catalog.unwrap().resources[0].id, "rtl-sdr.1");
            }
            server.join().unwrap();
        }
    }

    #[test]
    fn expired_health_is_visible_without_reading_withdrawn_catalog() {
        use std::{io::Write, net::TcpListener};
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0; 4096];
            let length = stream.read(&mut request).unwrap();
            assert!(
                std::str::from_utf8(&request[..length])
                    .unwrap()
                    .starts_with("GET /v1/health ")
            );
            let body = serde_json::to_string(&ServiceHealth {
                contract: "unibus.service.health.v1".into(),
                service_id: "hdhomerun.12345678".into(),
                state: HealthState::Stale,
                reason: Some("catalog_expired".into()),
                observed_at_unix_ms: 100,
                valid_until_unix_ms: 200,
            })
            .unwrap();
            write!(stream, "HTTP/1.1 503 Service Unavailable\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body).unwrap();
        });
        let result = inspect(&format!("http://{address}")).unwrap();
        assert_eq!(result.effective_state, HealthState::Stale);
        assert!(result.catalog.is_none());
        server.join().unwrap();
    }
}
