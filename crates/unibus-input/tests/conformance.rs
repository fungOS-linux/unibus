use unibus::{AdapterConformanceFixture, verify_adapter_lifecycle};

#[test]
fn hid_input_edge_obeys_the_common_adapter_lifecycle() {
    verify_adapter_lifecycle(&AdapterConformanceFixture::new(
        "edge-test",
        "hid.local",
        "hid",
        ["input.device.v1", "input.event.v1"],
    ))
    .unwrap();
}
