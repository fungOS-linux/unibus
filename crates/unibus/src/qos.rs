//! QoS classification. One table, consulted by L1 so it can pick a delivery
//! mechanism (reliable stream, datagram, replaceable latest-value) without
//! ever understanding what a message means.

use std::fmt;

/// Delivery class for a routed message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Qos {
    /// Reliable, ordered, TTL-bounded, revision-idempotent. A reply must
    /// reach the producer or the failure must be loud.
    Command,
    /// Latency over completeness. Late data is dropped, not queued.
    Stream,
    /// Latest value wins. Stale decays to unknown, never to empty.
    Snapshot,
}

impl fmt::Display for Qos {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Self::Command => "command",
            Self::Stream => "stream",
            Self::Snapshot => "snapshot",
        };
        f.write_str(name)
    }
}

impl Qos {
    /// Classify by message kind. v1 JARVIS snake_case tags map to the same
    /// class as their v2 namespaced successors, so a mixed-version deployment
    /// cannot get different QoS for one message by which peer routed it.
    pub fn of_kind(kind: &str) -> Self {
        match kind {
            // v1 tags
            "audio_capture_chunk" | "audio_playback_chunk" | "start_audio_capture" => Self::Stream,
            "presence_update" | "publish_presence" | "voice_identity" | "audio_level" => {
                Self::Snapshot
            }
            // v2 kinds are exact contract-owned policy. Unknown names default
            // reliable; a prefix alone must never weaken delivery semantics.
            "audio.capture_chunk"
            | "audio.playback_chunk"
            | "audio.start_capture"
            | "input.gamepad_state"
            | "input.desktop_state.v1"
            | "rlcd.decision.v1"
            | "sensor.sample" => Self::Stream,
            "presence.snapshot"
            | "presence.update"
            | "audio.level"
            | "content.index"
            | "content.index_snapshot"
            | "canvas.workspace_state.v1"
            | "media.video_stream.v1" => Self::Snapshot,
            _ => Self::Command,
        }
    }

    /// Whether a QUIC implementation may carry this class as unreliable
    /// datagrams when the chunk fits the negotiated datagram size.
    pub fn datagram_capable(&self) -> bool {
        matches!(self, Self::Stream | Self::Snapshot)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn v1_and_v2_names_agree() {
        assert_eq!(Qos::of_kind("audio_playback_chunk"), Qos::Stream);
        assert_eq!(Qos::of_kind("audio.playback_chunk"), Qos::Stream);
        assert_eq!(Qos::of_kind("publish_presence"), Qos::Snapshot);
        assert_eq!(Qos::of_kind("presence.snapshot"), Qos::Snapshot);
    }

    #[test]
    fn everything_else_is_command() {
        assert_eq!(Qos::of_kind("canvas.command"), Qos::Command);
        assert_eq!(Qos::of_kind("heartbeat"), Qos::Command);
        assert_eq!(Qos::of_kind("totally.unknown_kind"), Qos::Command);
    }

    #[test]
    fn sensor_samples_stream() {
        assert_eq!(Qos::of_kind("sensor.sample"), Qos::Stream);
    }

    #[test]
    fn gamepad_state_is_latency_first() {
        assert_eq!(Qos::of_kind("input.gamepad_state"), Qos::Stream);
    }

    #[test]
    fn desktop_input_is_latency_first() {
        assert_eq!(Qos::of_kind("input.desktop_state.v1"), Qos::Stream);
    }

    #[test]
    fn rlcd_decisions_are_a_live_event_stream() {
        assert_eq!(Qos::of_kind("rlcd.decision.v1"), Qos::Stream);
    }

    #[test]
    fn audio_level_is_replaceable_latest_value() {
        assert_eq!(Qos::of_kind("audio_level"), Qos::Snapshot);
        assert_eq!(Qos::of_kind("audio.level"), Qos::Snapshot);
    }

    #[test]
    fn canvas_workspace_state_is_replaceable_latest_value() {
        assert_eq!(Qos::of_kind("canvas.workspace_state.v1"), Qos::Snapshot);
    }

    #[test]
    fn unknown_namespaced_kinds_fail_safe_to_command() {
        assert_eq!(Qos::of_kind("audio.novel"), Qos::Command);
        assert_eq!(Qos::of_kind("presence.novel"), Qos::Command);
        assert_eq!(Qos::of_kind("sensor.novel"), Qos::Command);
    }
}
