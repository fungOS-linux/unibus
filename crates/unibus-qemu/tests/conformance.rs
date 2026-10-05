use unibus::{AdapterConformanceFixture, verify_adapter_lifecycle};

#[test]
fn qemu_media_edge_obeys_the_common_adapter_lifecycle() {
    verify_adapter_lifecycle(&AdapterConformanceFixture::new(
        "edge-test",
        "qemu.agora-one",
        "qemu-media",
        ["stream.descriptor.v2", "stream.endpoint.v2", "input.desktop.v1"],
    ))
    .unwrap();
}
