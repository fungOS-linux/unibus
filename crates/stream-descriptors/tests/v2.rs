use std::collections::BTreeMap;

use stream_descriptors::{
    ClockKind, ClockSupport, ConnectionRole, ConsumerProfileV2, EncodingSupport,
    EndpointAnnouncementV2, MediaLimits, OBSERVATION_SNAPSHOT_V2_KIND, ObservationDatum,
    ObservationSampleV2, StreamDescriptorV2, TransportSupport,
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
fn consumer_matching_is_exact_and_deterministic() {
    let descriptor: StreamDescriptorV2 =
        serde_json::from_str(include_str!("fixtures/radioman-spectrum-v2.json")).unwrap();
    let mut endpoints: EndpointAnnouncementV2 =
        serde_json::from_str(include_str!("fixtures/radioman-endpoints-v2.json")).unwrap();
    let mut earlier = endpoints.bindings[0].clone();
    earlier.endpoint_reference = "radioman/a-spectrum".into();
    endpoints.bindings.push(earlier);

    let consumer = ConsumerProfileV2 {
        consumer_id: "canvas/studio".into(),
        capabilities: vec!["rf.spectrum.consume".into()],
        encodings: vec![EncodingSupport {
            kind: "json-f32-dbfs".into(),
            parameters: BTreeMap::new(),
        }],
        clocks: vec![ClockSupport {
            kind: ClockKind::WallClock,
            maximum_rate_hz: None,
            maximum_timestamp_bits: None,
        }],
        transports: vec![TransportSupport {
            transport: "unibus".into(),
            profiles: vec!["envelope-v1".into()],
            roles: vec![ConnectionRole::Connectionless],
        }],
        media_limits: None,
    };
    let matches = consumer
        .compatible_bindings(&descriptor, &endpoints, 30_000)
        .unwrap();
    assert_eq!(matches.len(), 2);
    assert_eq!(matches[0].endpoint_reference, "radioman/a-spectrum");

    let mut denied = consumer;
    denied.capabilities.clear();
    let error = denied
        .compatible_bindings(&descriptor, &endpoints, 30_000)
        .unwrap_err();
    assert!(error.contains("rf.spectrum.consume"));
}

#[test]
fn media_matching_checks_codec_clock_geometry_and_role() {
    let descriptor: StreamDescriptorV2 = serde_json::from_value(serde_json::json!({
        "schema_version": 2,
        "stream_id": "camera/front-door/live",
        "source_resource": "camera/front-door",
        "semantic": {"kind": "video.camera"},
        "payload": {"contract": "media.video.frame.v1", "maximum_bytes": 8388608},
        "encoding": {"kind": "h264", "parameters": {"profile": "high", "level": "4.1"}},
        "clock": {"kind": "media", "rate_hz": 90000, "timestamp_bits": 32},
        "media": {"width": 1920, "height": 1080, "frame_rate_millihz": 30000},
        "delivery": {"class": "stream", "maximum_age_ms": 1000},
        "required_consumer_capabilities": ["media.video.decode.h264"],
        "provenance": {"producer": "frigate"}
    }))
    .unwrap();
    let endpoints: EndpointAnnouncementV2 = serde_json::from_value(serde_json::json!({
        "schema_version": 2,
        "stream_id": "camera/front-door/live",
        "generation": "boot-1",
        "bindings": [{
            "transport": "rtsp",
            "profile": "rtsp-1.0",
            "role": "connector",
            "endpoint_reference": "frigate/front-door",
            "requires": "media.transport.rtsp"
        }],
        "observed_at_unix_ms": 1000,
        "valid_until_unix_ms": 31000,
        "provenance": {"producer": "frigate"}
    }))
    .unwrap();
    let mut consumer = ConsumerProfileV2 {
        consumer_id: "gstreamer/studio".into(),
        capabilities: vec![
            "media.video.decode.h264".into(),
            "media.transport.rtsp".into(),
        ],
        encodings: vec![EncodingSupport {
            kind: "h264".into(),
            parameters: BTreeMap::from([
                ("profile".into(), "high".into()),
                ("level".into(), "4.1".into()),
            ]),
        }],
        clocks: vec![ClockSupport {
            kind: ClockKind::Media,
            maximum_rate_hz: Some(90_000),
            maximum_timestamp_bits: Some(32),
        }],
        transports: vec![TransportSupport {
            transport: "rtsp".into(),
            profiles: vec!["rtsp-1.0".into()],
            roles: vec![ConnectionRole::Connector],
        }],
        media_limits: Some(MediaLimits {
            maximum_width: Some(1920),
            maximum_height: Some(1080),
            maximum_frame_rate_millihz: Some(60_000),
            maximum_sample_rate_hz: None,
            maximum_channels: None,
            maximum_frames_per_packet: None,
        }),
    };
    assert_eq!(
        consumer
            .compatible_bindings(&descriptor, &endpoints, 30_000)
            .unwrap()
            .len(),
        1
    );

    consumer.media_limits.as_mut().unwrap().maximum_width = Some(1280);
    assert!(
        consumer
            .compatible_bindings(&descriptor, &endpoints, 30_000)
            .unwrap_err()
            .contains("width")
    );
}

#[test]
fn endpoint_references_cannot_smuggle_urls_or_credentials() {
    let mut endpoints: EndpointAnnouncementV2 =
        serde_json::from_str(include_str!("fixtures/radioman-endpoints-v2.json")).unwrap();
    endpoints.bindings[0].endpoint_reference = "rtsp://user:secret@camera/live".into();
    assert!(endpoints.validate(30_000).is_err());
}
