//! Mycelium's application-owned Iroh observations, narrowed to selected room hosts.
use anyhow::{Result, ensure};
use serde::Serialize;
use std::{collections::BTreeSet, net::SocketAddr};
use unibus::ServiceDirectory;

pub const ROOM_ALPN: &str = "fpl-holodeck-room/1";
const PROVIDER: &str = "mycelium-iroh-owner-lease";

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RoomHost {
    pub service_id: String,
    pub owner_node: String,
    pub endpoint_id: String,
    pub direct_addresses: Vec<SocketAddr>,
    pub relay_urls: Vec<String>,
    pub expires_at_unix_s: u64,
}

/// Mycelium finds endpoints; the caller chooses which pinned keys it will dial.
/// A service observation never grants room admission.
pub fn selected_hosts(
    directory: &dyn ServiceDirectory,
    now_unix_s: u64,
    trusted_endpoint_ids: &BTreeSet<String>,
) -> Result<Vec<RoomHost>> {
    ensure!(
        trusted_endpoint_ids.len() <= 16
            && trusted_endpoint_ids.iter().all(|key| key.len() == 64
                && key.bytes().any(|b| b != b'0')
                && key
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))),
        "trusted room endpoint IDs invalid"
    );
    let mut hosts = Vec::new();
    for service in directory.services("iroh")? {
        service.validate()?;
        if !service.is_fresh_at(now_unix_s) {
            continue;
        }
        let alpns: BTreeSet<String> = serde_json::from_str(
            service
                .attributes
                .get("alpns")
                .ok_or_else(|| anyhow::anyhow!("Iroh ALPNs missing"))?,
        )?;
        if !alpns.contains(ROOM_ALPN) {
            continue;
        }
        ensure!(
            service.provenance_provider == PROVIDER,
            "room endpoint has untrusted provenance"
        );
        ensure!(
            service.expires_at_unix_s - service.observed_at_unix_s <= 300,
            "room endpoint lease too long"
        );
        ensure!(
            service.endpoints.len() == 1,
            "room endpoint identity is ambiguous"
        );
        let endpoint_id = service.endpoints[0]
            .strip_prefix("iroh://")
            .ok_or_else(|| anyhow::anyhow!("room endpoint has invalid scheme"))?;
        ensure!(
            endpoint_id.len() == 64
                && endpoint_id.bytes().any(|b| b != b'0')
                && endpoint_id
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
            "room endpoint has invalid key"
        );
        if !trusted_endpoint_ids.contains(endpoint_id) {
            continue;
        }
        let owner_node = service
            .attributes
            .get("node_id")
            .ok_or_else(|| anyhow::anyhow!("room endpoint owner missing"))?;
        ensure!(
            service
                .service_id
                .starts_with(&format!("service/{owner_node}/iroh/"))
                && service.provenance_observer == *owner_node,
            "room endpoint owner provenance mismatch"
        );
        ensure!(
            service
                .attributes
                .get("endpoint_id")
                .is_some_and(|v| v == endpoint_id)
                && service
                    .attributes
                    .get("authentication")
                    .is_some_and(|v| v == "application-owned"),
            "room endpoint identity metadata mismatch"
        );
        let direct: BTreeSet<SocketAddr> = serde_json::from_str(
            service
                .attributes
                .get("direct_addresses")
                .ok_or_else(|| anyhow::anyhow!("room direct addresses missing"))?,
        )?;
        let relay: BTreeSet<String> = serde_json::from_str(
            service
                .attributes
                .get("relay_urls")
                .ok_or_else(|| anyhow::anyhow!("room relay URLs missing"))?,
        )?;
        ensure!(
            !direct.is_empty() || !relay.is_empty(),
            "room endpoint has no route hints"
        );
        ensure!(
            direct.len() <= 16
                && relay.len() <= 4
                && direct
                    .iter()
                    .all(|a| a.port() > 0 && !a.ip().is_unspecified() && !a.ip().is_multicast()),
            "room endpoint route hints invalid"
        );
        for origin in &relay {
            let url = url::Url::parse(origin)?;
            ensure!(
                url.scheme() == "https"
                    && url.host_str().is_some()
                    && url.username().is_empty()
                    && url.password().is_none()
                    && url.query().is_none()
                    && url.fragment().is_none()
                    && url.path() == "/",
                "room relay URL must be credential-free HTTPS"
            );
        }
        hosts.push(RoomHost {
            service_id: service.service_id,
            owner_node: owner_node.clone(),
            endpoint_id: endpoint_id.into(),
            direct_addresses: direct.into_iter().collect(),
            relay_urls: relay.into_iter().collect(),
            expires_at_unix_s: service.expires_at_unix_s,
        });
    }
    hosts.sort_by(|a, b| a.service_id.cmp(&b.service_id));
    ensure!(
        hosts.len() <= 16
            && hosts
                .windows(2)
                .all(|pair| pair[0].service_id != pair[1].service_id)
            && hosts.iter().enumerate().all(|(i, host)| hosts[..i]
                .iter()
                .all(|other| other.endpoint_id != host.endpoint_id)),
        "room endpoint catalog exceeds capacity or conflicts"
    );
    Ok(hosts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use unibus::{ServiceObservation, StaticServiceDirectory};

    fn service() -> ServiceObservation {
        let key = "a".repeat(64);
        ServiceObservation {
            service_id: "service/agora/iroh/holodeck-room".into(),
            kind: "iroh".into(),
            endpoints: vec![format!("iroh://{key}")],
            observed_at_unix_s: 100,
            expires_at_unix_s: 200,
            provenance_provider: PROVIDER.into(),
            provenance_observer: "agora".into(),
            attributes: BTreeMap::from([
                ("node_id".into(), "agora".into()),
                ("endpoint_id".into(), key),
                ("authentication".into(), "application-owned".into()),
                ("alpns".into(), format!("[\"{ROOM_ALPN}\"]")),
                ("direct_addresses".into(), "[\"192.168.2.67:4000\"]".into()),
                (
                    "relay_urls".into(),
                    "[\"https://relay.example.net/\"]".into(),
                ),
            ]),
        }
    }
    #[test]
    fn only_fresh_pinned_room_endpoints_are_selected() {
        let value = service();
        let pin = BTreeSet::from(["a".repeat(64)]);
        let directory = StaticServiceDirectory::new(vec![value]).unwrap();
        assert_eq!(selected_hosts(&directory, 150, &pin).unwrap().len(), 1);
        assert!(selected_hosts(&directory, 200, &pin).unwrap().is_empty());
        assert!(
            selected_hosts(&directory, 150, &BTreeSet::new())
                .unwrap()
                .is_empty()
        );
    }
    #[test]
    fn valid_key_with_wrong_owner_or_route_fails() {
        let pin = BTreeSet::from(["a".repeat(64)]);
        let mut value = service();
        value.provenance_observer = "other".into();
        let directory = StaticServiceDirectory::new(vec![value]).unwrap();
        assert!(selected_hosts(&directory, 150, &pin).is_err());
        let mut value = service();
        value.attributes.insert(
            "relay_urls".into(),
            "[\"https://token@relay.example.net/\"]".into(),
        );
        let directory = StaticServiceDirectory::new(vec![value]).unwrap();
        assert!(selected_hosts(&directory, 150, &pin).is_err());
    }
}
