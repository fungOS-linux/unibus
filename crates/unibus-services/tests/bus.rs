//! Real router/SDK proof: scoped publication, consumer retention and read-only
//! observer grants. No mock transport and no physical-device mutations.
use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use serde_json::json;
use stream_descriptors::service::{
    HealthState, SERVICE_CATALOG_KIND, SERVICE_HEALTH_KIND, ServiceCatalog, ServiceHealth,
};
use unibus::client::ClientConfig;
use unibus_sdk::{Publish, Session};
use unibus_services::bus::{BusInventory, SourceBinding};

#[test]
fn live_bus_fans_out_catalogs_with_exact_grants_and_no_observer_control() {
    let directory = tempfile::tempdir().unwrap();
    for producer in ["pi-catalog", "observer"] {
        unibus_router::auth::issue_token(
            &directory.path().join(format!("{producer}.token")),
            &unibus_router::auth::generate_token().unwrap(),
        )
        .unwrap();
    }
    let config = serde_json::from_value(json!({"installation_id":"test","listen":"127.0.0.1:0",
        "routes":[{"kind":SERVICE_CATALOG_KIND,"target":{"mode":"any_holder","name":"services.observe"}},
            {"kind":SERVICE_HEALTH_KIND,"target":{"mode":"any_holder","name":"services.observe"}}],
        "nodes":[{"producer_id":"pi-catalog","token_file":directory.path().join("pi-catalog.token"),
            "zones":["default"],"capabilities":[],"publishes":[SERVICE_CATALOG_KIND,SERVICE_HEALTH_KIND]},
            {"producer_id":"observer","token_file":directory.path().join("observer.token"),
                "zones":["default"],"capabilities":["services.observe"],"publishes":[]}] })).unwrap();
    let hub = Arc::new(unibus_router::Hub::new(config).unwrap());
    let address = hub.start().unwrap();
    let client = |producer: &str, capabilities: Vec<&str>| -> ClientConfig {
        serde_json::from_value(json!({"installation_id":"test","producer_id":producer,
            "hub_endpoint":address.to_string(),"token_file":directory.path().join(format!("{producer}.token")),
            "zones":["default"],"capabilities":capabilities,"schema_version":2})).unwrap()
    };
    let publisher = Session::connect(client("pi-catalog", vec![])).unwrap();
    let now = unibus::envelope::now_ms();
    let catalog = ServiceCatalog {
        contract: SERVICE_CATALOG_KIND.into(),
        service_id: "hdhomerun.12345678".into(),
        adapter_kind: "hdhomerun".into(),
        owner_reference: "tls/owner".into(),
        presenter_reference: "lan/pi/8088".into(),
        observed_at_unix_ms: now,
        valid_until_unix_ms: now + 60_000,
        resources: vec![],
    };
    let health = ServiceHealth {
        contract: SERVICE_HEALTH_KIND.into(),
        service_id: catalog.service_id.clone(),
        state: HealthState::Healthy,
        reason: None,
        observed_at_unix_ms: now,
        valid_until_unix_ms: now + 60_000,
    };
    let idle_request = publisher
        .publish(Publish {
            destination: None,
            kind: SERVICE_CATALOG_KIND.into(),
            body: serde_json::to_value(&catalog).unwrap(),
        })
        .unwrap();
    let timeout = Instant::now() + Duration::from_secs(3);
    loop {
        if let Some(event) = publisher.poll()
            && event.kind == unibus::kinds::V1_REPLY
            && event.body["request_id"] == idle_request
        {
            assert_eq!(event.body["success"], false);
            assert_eq!(
                event.body["message"],
                unibus::kinds::REPLY_NO_ELIGIBLE_NODES
            );
            break;
        }
        assert!(
            Instant::now() < timeout,
            "missing explicit idle-demand receipt"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    let observer = Session::connect(client("observer", vec!["services.observe"])).unwrap();
    let mut inventory = BusInventory::new(vec![SourceBinding {
        producer_id: "pi-catalog".into(),
        service_id: catalog.service_id.clone(),
        presenter_reference: catalog.presenter_reference.clone(),
        owner_reference: catalog.owner_reference.clone(),
    }])
    .unwrap();
    for (kind, body) in [
        (SERVICE_CATALOG_KIND, serde_json::to_value(catalog).unwrap()),
        (SERVICE_HEALTH_KIND, serde_json::to_value(health).unwrap()),
    ] {
        publisher
            .publish(Publish {
                destination: None,
                kind: kind.into(),
                body,
            })
            .unwrap();
    }
    let timeout = Instant::now() + Duration::from_secs(3);
    loop {
        while let Some(event) = observer.poll() {
            inventory
                .observe(&event, unibus::envelope::now_ms())
                .unwrap();
        }
        if inventory
            .rows(now)
            .first()
            .is_some_and(|row| row.effective_state == HealthState::Healthy)
        {
            break;
        }
        assert!(
            Instant::now() < timeout,
            "catalog/health did not traverse real router"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(
        inventory.rows(now + 60_000)[0].effective_state,
        HealthState::Stale
    );
    let request = observer
        .publish(Publish {
            destination: None,
            kind: SERVICE_CATALOG_KIND.into(),
            body: json!({}),
        })
        .unwrap();
    let timeout = Instant::now() + Duration::from_secs(3);
    loop {
        if let Some(event) = observer.poll()
            && event.kind == unibus::kinds::V1_REPLY
            && event.body["request_id"] == request
        {
            assert_eq!(event.body["success"], false);
            break;
        }
        assert!(
            Instant::now() < timeout,
            "missing explicit observer grant denial"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}
