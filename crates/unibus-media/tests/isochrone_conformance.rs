use isochrone_core::{Encoding, StreamFormat, packet};
use unibus_media::{IsochroneEncoding, IsochroneRtpStream};

#[test]
fn advertised_audio_matches_isochrones_wire_geometry_and_conversion() {
    for (encoding, projected, tolerance) in [
        (Encoding::L16, IsochroneEncoding::L16, 0.0001),
        (Encoding::L24, IsochroneEncoding::L24, 0.000001),
    ] {
        for frames in [6, 48, 240] {
            let format = StreamFormat {
                sample_rate_hz: 48000,
                channels: 2,
                encoding,
                frames_per_packet: frames,
            };
            let stream = IsochroneRtpStream {
                stream_id: "audio/studio/cue".into(),
                source_resource: "audio/mixtrack/headphones".into(),
                zone: "studio".into(),
                endpoint_reference: "isochrone/studio/cue".into(),
                generation: "session-1".into(),
                producer: "isochrone-agent".into(),
                encoding: projected,
                sample_rate_hz: format.sample_rate_hz,
                channels: format.channels,
                frames_per_packet: format.frames_per_packet,
                observed_at_unix_ms: 1000,
                valid_until_unix_ms: 31000,
            };
            let projection = stream.project_v2().unwrap();
            let media = projection.descriptor.media.unwrap();
            assert_eq!(media.frames_per_packet, Some(format.frames_per_packet));
            assert_eq!(media.sample_rate_hz, Some(format.sample_rate_hz));
            assert_eq!(
                projection.descriptor.clock.unwrap().rate_hz,
                Some(u64::from(format.sample_rate_hz))
            );
            let samples: Vec<f32> = (0..frames * 2)
                .map(|index| if index % 2 == 0 { 0.25 } else { -0.5 })
                .collect();
            let mut wire = Vec::new();
            packet::encode_samples(&samples, encoding, &mut wire);
            assert_eq!(wire.len(), format.payload_bytes());
            assert_eq!(
                packet::frames_in_payload(wire.len(), 2, encoding).unwrap(),
                frames as usize
            );
            let mut decoded = Vec::new();
            packet::decode_samples(&wire, encoding, &mut decoded).unwrap();
            assert!(
                decoded
                    .iter()
                    .zip(&samples)
                    .all(|(actual, expected)| (actual - expected).abs() < tolerance)
            );
            assert!(packet::frames_in_payload(wire.len() - 1, 2, encoding).is_err());
            assert!(projection.endpoints.validate(31000).is_err());
        }
    }
}
