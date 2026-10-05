//! Opaque MIDI-over-Unibus contract.

use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

pub mod inventory;

pub const MIDI_PACKET_CONTRACT: &str = "midi.packet.v1";
pub const MIDI_INPUT_CAPABILITY: &str = "midi.input";
pub const MIDI_OUTPUT_CAPABILITY: &str = "midi.output";
pub const MIDI_SCHEMA_VERSION: u32 = 1;
pub const MAX_MIDI_PACKET_BYTES: usize = 65_536;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MidiPacket {
    pub schema_version: u32,
    pub origin: String,
    pub sequence: u64,
    /// Timestamp supplied by the local MIDI API, in microseconds. It is
    /// diagnostic metadata; output is sent immediately rather than replayed.
    pub source_timestamp_micros: u64,
}

impl MidiPacket {
    pub fn validate(&self, payload: &[u8]) -> Result<()> {
        ensure!(
            self.schema_version == MIDI_SCHEMA_VERSION,
            "unsupported MIDI packet schema {}",
            self.schema_version
        );
        ensure!(valid_identifier(&self.origin), "invalid MIDI origin");
        ensure!(!payload.is_empty(), "empty MIDI packet");
        ensure!(
            payload.len() <= MAX_MIDI_PACKET_BYTES,
            "MIDI packet exceeds {MAX_MIDI_PACKET_BYTES} bytes"
        );
        Ok(())
    }
}

pub fn valid_identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packet_round_trips_and_validates() {
        let packet = MidiPacket {
            schema_version: MIDI_SCHEMA_VERSION,
            origin: "mac-studio-midi".into(),
            sequence: 42,
            source_timestamp_micros: 1234,
        };
        packet.validate(&[0x90, 60, 127]).unwrap();
        let encoded = serde_json::to_vec(&packet).unwrap();
        assert_eq!(
            serde_json::from_slice::<MidiPacket>(&encoded).unwrap(),
            packet
        );
    }

    #[test]
    fn empty_and_oversized_packets_fail_loudly() {
        let packet = MidiPacket {
            schema_version: MIDI_SCHEMA_VERSION,
            origin: "source".into(),
            sequence: 0,
            source_timestamp_micros: 0,
        };
        assert!(packet.validate(&[]).is_err());
        assert!(
            packet
                .validate(&vec![0; MAX_MIDI_PACKET_BYTES + 1])
                .is_err()
        );
    }
}
