use std::collections::BTreeMap;

use stream_descriptors::{
    EndpointAnnouncementV2, OBSERVATION_SNAPSHOT_V2_KIND, ObservationDatum, ObservationSampleV2,
    StreamDescriptorV2,
};

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
fn environmental_sample_validates_without_producer_specific_parsing() {
    let descriptor: StreamDescriptorV2 =
        serde_json::from_str(include_str!("fixtures/environment-v2.json")).unwrap();
    assert_eq!(
        descriptor.observation_message_kind().unwrap(),
        OBSERVATION_SNAPSHOT_V2_KIND
    );
    assert_eq!(descriptor.scalar_observations().count(), 2);

    let sample = ObservationSampleV2 {
        schema_version: 2,
        stream_id: descriptor.stream_id.clone(),
        sequence: 7,
        observed_at_unix_ms: 1_000,
        values: BTreeMap::from([
            (
                "temperature".into(),
                ObservationDatum::Scalar { value: 21.5 },
            ),
            (
                "relative_humidity".into(),
                ObservationDatum::Scalar { value: 44.0 },
            ),
        ]),
    };
    sample.validate_against(&descriptor, 1_100).unwrap();

    let mut undeclared = sample.clone();
    undeclared.values.insert(
        "room_name".into(),
        ObservationDatum::State {
            value: "office".into(),
        },
    );
    assert!(undeclared.validate_against(&descriptor, 1_100).is_err());
    assert!(sample.validate_against(&descriptor, 100_001).is_err());
}

#[test]
fn csi_sample_enforces_vector_bound_and_finite_values() {
    let descriptor: StreamDescriptorV2 =
        serde_json::from_str(include_str!("fixtures/esp32-csi-features-v2.json")).unwrap();
    let sample = ObservationSampleV2 {
        schema_version: 2,
        stream_id: descriptor.stream_id.clone(),
        sequence: 1,
        observed_at_unix_ms: 1_000,
        values: BTreeMap::from([(
            "amplitude".into(),
            ObservationDatum::Vector {
                values: vec![-31.0, -29.5, -30.25],
            },
        )]),
    };
    sample.validate_against(&descriptor, 1_050).unwrap();

    let mut invalid = sample;
    invalid.values.insert(
        "amplitude".into(),
        ObservationDatum::Vector {
            values: vec![f64::NAN],
        },
    );
    assert!(invalid.validate_against(&descriptor, 1_050).is_err());
}

#[test]
fn endpoint_references_cannot_smuggle_urls_or_credentials() {
    let mut endpoints: EndpointAnnouncementV2 =
        serde_json::from_str(include_str!("fixtures/radioman-endpoints-v2.json")).unwrap();
    endpoints.bindings[0].endpoint_reference = "rtsp://user:secret@camera/live".into();
    assert!(endpoints.validate(30_000).is_err());
}
