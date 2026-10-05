use stream_descriptors::{EndpointAnnouncementV2, StreamDescriptorV2};

fn assert_descriptor_golden(source: &str) -> StreamDescriptorV2 {
    let expected: serde_json::Value = serde_json::from_str(source).unwrap();
    let descriptor: StreamDescriptorV2 = serde_json::from_value(expected.clone()).unwrap();
    descriptor.validate().unwrap();
    assert_eq!(serde_json::to_value(&descriptor).unwrap(), expected);
    descriptor
}

#[test]
fn radioman_spectrum_is_a_valid_golden_descriptor() {
    assert_descriptor_golden(include_str!("fixtures/radioman-spectrum-v2.json"));
}

#[test]
fn f1_telemetry_exercises_all_observation_shapes() {
    let descriptor = assert_descriptor_golden(include_str!("fixtures/f1-telemetry-v2.json"));
    assert_eq!(descriptor.observation.unwrap().values.len(), 5);
}

#[test]
fn environmental_snapshot_has_explicit_units_and_coalescing() {
    assert_descriptor_golden(include_str!("fixtures/environment-v2.json"));
}

#[test]
fn esp32_csi_features_are_bounded_latency_first_work() {
    let descriptor = assert_descriptor_golden(include_str!("fixtures/esp32-csi-features-v2.json"));
    let values = &descriptor.observation.unwrap().values;
    assert_eq!(values.len(), 2);
}

#[test]
fn endpoint_binding_is_separate_and_expires() {
    let source = include_str!("fixtures/radioman-endpoints-v2.json");
    let expected: serde_json::Value = serde_json::from_str(source).unwrap();
    let endpoints: EndpointAnnouncementV2 = serde_json::from_value(expected.clone()).unwrap();
    endpoints.validate(30_000).unwrap();
    assert!(endpoints.validate(31_000).is_err());
    assert_eq!(serde_json::to_value(endpoints).unwrap(), expected);
}

#[test]
fn unknown_fields_fail_closed() {
    let mut value: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/radioman-spectrum-v2.json")).unwrap();
    value["transport"] = serde_json::json!("udp");
    assert!(serde_json::from_value::<StreamDescriptorV2>(value).is_err());
}

#[test]
fn invalid_cross_facet_values_are_rejected() {
    let mut descriptor: StreamDescriptorV2 =
        serde_json::from_str(include_str!("fixtures/radioman-spectrum-v2.json")).unwrap();

    descriptor.delivery.maximum_age_ms = None;
    assert!(descriptor.validate().is_err());

    let mut descriptor: StreamDescriptorV2 =
        serde_json::from_str(include_str!("fixtures/radioman-spectrum-v2.json")).unwrap();
    descriptor.observation.as_mut().unwrap().values[0].unit = Some("$(exec)".into());
    assert!(descriptor.validate().is_err());

    let mut descriptor: StreamDescriptorV2 =
        serde_json::from_str(include_str!("fixtures/radioman-spectrum-v2.json")).unwrap();
    descriptor.observation.as_mut().unwrap().values[0].unit = Some("bananas".into());
    assert!(descriptor.validate().is_err());

    let mut descriptor: StreamDescriptorV2 =
        serde_json::from_str(include_str!("fixtures/environment-v2.json")).unwrap();
    descriptor.delivery.coalescing = None;
    assert!(descriptor.validate().is_err());
}

#[test]
fn unknown_observation_fields_fail_closed() {
    let mut value: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/radioman-spectrum-v2.json")).unwrap();
    value["observation"]["values"][0]["pipeline"] = serde_json::json!("shell");
    assert!(serde_json::from_value::<StreamDescriptorV2>(value).is_err());
}

#[test]
fn endpoint_references_cannot_smuggle_urls_or_credentials() {
    let mut endpoints: EndpointAnnouncementV2 =
        serde_json::from_str(include_str!("fixtures/radioman-endpoints-v2.json")).unwrap();
    endpoints.bindings[0].endpoint_reference = "rtsp://user:secret@camera/live".into();
    assert!(endpoints.validate(30_000).is_err());
}
