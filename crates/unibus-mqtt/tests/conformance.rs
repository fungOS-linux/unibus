use unibus::{AdapterConformanceFixture, verify_adapter_lifecycle};

#[test]
fn mqtt_edge_obeys_the_common_adapter_lifecycle() {
    verify_adapter_lifecycle(&AdapterConformanceFixture::new(
        "edge-test",
        "mqtt.local",
        "mqtt",
        ["mqtt.publication.v1", "mqtt.subscription.v1"],
    ))
    .unwrap();
}
