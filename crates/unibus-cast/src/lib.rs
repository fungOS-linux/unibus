//! Cast session control plane, not a protocol receiver or media transport.
use serde::{Deserialize, Serialize};
pub mod runtime;
pub mod status;
use stream_descriptors::ClockCorrelationV1;

pub const SESSION_KIND: &str = "media.cast.session.v1";
pub const COMMAND_KIND: &str = "media.cast.command.v1";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Backend {
    AirplayAudio,
    AirplayMirroring,
    GoogleCast,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Connecting,
    Playing,
    Ended,
    Failed,
}

/// References existing stream descriptors; never carries executable pipelines,
/// credentials, endpoint URLs or PCM/video bytes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Routes {
    pub video_stream_id: Option<String>,
    pub audio_stream_id: Option<String>,
    pub isochrone_route_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Session {
    pub schema_version: u32,
    pub session_id: String,
    pub generation: String,
    pub revision: u64,
    pub backend: Backend,
    pub phase: Phase,
    pub muted: bool,
    pub routes: Routes,
    /// Optional producer timeline; a clock mapping alone does not prove A/V sync.
    pub source_clock: Option<ClockCorrelationV1>,
    pub observed_at_unix_ms: u64,
    pub valid_until_unix_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum Action {
    Stop,
    Mute { muted: bool },
    Route { routes: Routes },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Command {
    pub schema_version: u32,
    pub request_id: String,
    pub session_id: String,
    pub generation: String,
    pub expected_revision: u64,
    pub expires_at_unix_ms: u64,
    pub action: Action,
}

fn id(value: &str) -> Result<(), String> {
    if value.is_empty() || value.len() > 256 || value.chars().any(char::is_control) {
        return Err("invalid identifier".into());
    }
    Ok(())
}

impl Routes {
    pub fn validate(&self) -> Result<(), String> {
        for value in [
            &self.video_stream_id,
            &self.audio_stream_id,
            &self.isochrone_route_id,
        ]
        .into_iter()
        .flatten()
        {
            id(value)?;
        }
        if self.isochrone_route_id.is_some() && self.audio_stream_id.is_none() {
            return Err("Isochrone route requires an audio stream".into());
        }
        Ok(())
    }
}

impl Session {
    pub fn validate(&self, now_ms: u64) -> Result<(), String> {
        if self.schema_version != 1 {
            return Err("unsupported session schema".into());
        }
        id(&self.session_id)?;
        id(&self.generation)?;
        self.routes.validate()?;
        if self.observed_at_unix_ms > now_ms
            || self.valid_until_unix_ms <= now_ms
            || self
                .valid_until_unix_ms
                .saturating_sub(self.observed_at_unix_ms)
                > 30_000
        {
            return Err("session lease is stale or invalid".into());
        }
        if self.backend == Backend::AirplayAudio && self.routes.video_stream_id.is_some() {
            return Err("audio-only backend cannot expose video".into());
        }
        if let Some(clock) = &self.source_clock {
            clock.validate()?;
            if clock.observed_at_unix_ms > now_ms || clock.valid_until_unix_ms <= now_ms {
                return Err("source clock is stale".into());
            }
        }
        Ok(())
    }

    /// Admission only: caller must authenticate/authorize first and persist
    /// request-id deduplication and revision changes atomically with execution.
    pub fn admit(&self, command: &Command, now_ms: u64) -> Result<(), String> {
        self.validate(now_ms)?;
        id(&command.request_id)?;
        if command.schema_version != 1
            || command.session_id != self.session_id
            || command.generation != self.generation
            || command.expected_revision != self.revision
        {
            return Err("command schema or session revision mismatch".into());
        }
        if command.expires_at_unix_ms <= now_ms
            || command.expires_at_unix_ms > self.valid_until_unix_ms
        {
            return Err("command deadline invalid".into());
        }
        if matches!(self.phase, Phase::Ended | Phase::Failed) {
            return Err("session no longer accepts commands".into());
        }
        if let Action::Route { routes } = &command.action {
            routes.validate()?;
            if self.backend == Backend::AirplayAudio && routes.video_stream_id.is_some() {
                return Err("audio-only backend cannot route video".into());
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn session() -> Session {
        Session {
            schema_version: 1,
            session_id: "cast/one".into(),
            generation: "boot-1".into(),
            revision: 4,
            backend: Backend::AirplayMirroring,
            phase: Phase::Playing,
            muted: false,
            routes: Routes {
                video_stream_id: Some("video/one".into()),
                audio_stream_id: Some("audio/one".into()),
                isochrone_route_id: Some("speakers".into()),
            },
            source_clock: None,
            observed_at_unix_ms: 1000,
            valid_until_unix_ms: 4000,
        }
    }
    fn command() -> Command {
        Command {
            schema_version: 1,
            request_id: "req-1".into(),
            session_id: "cast/one".into(),
            generation: "boot-1".into(),
            expected_revision: 4,
            expires_at_unix_ms: 3000,
            action: Action::Mute { muted: true },
        }
    }
    #[test]
    fn round_trip_and_admission() {
        let s = session();
        assert_eq!(
            serde_json::from_str::<Session>(&serde_json::to_string(&s).unwrap()).unwrap(),
            s
        );
        assert!(s.admit(&command(), 2000).is_ok());
    }
    #[test]
    fn stale_restarted_and_revision_commands_rejected() {
        let s = session();
        let mut c = command();
        c.expected_revision = 3;
        assert!(s.admit(&c, 2000).is_err());
        c = command();
        c.generation = "boot-old".into();
        assert!(s.admit(&c, 2000).is_err());
        assert!(s.admit(&command(), 3000).is_err());
        assert!(s.admit(&command(), 4000).is_err());
    }
    #[test]
    fn stopped_session_and_unbounded_deadline_rejected() {
        let mut s = session();
        s.phase = Phase::Ended;
        assert!(s.admit(&command(), 2000).is_err());
        s.phase = Phase::Playing;
        let mut c = command();
        c.expires_at_unix_ms = 5000;
        assert!(s.admit(&c, 2000).is_err());
    }
    #[test]
    fn audio_and_routing_boundaries() {
        let mut s = session();
        s.backend = Backend::AirplayAudio;
        assert!(s.validate(2000).is_err());
        s.routes.video_stream_id = None;
        assert!(s.validate(2000).is_ok());
        let mut c = command();
        c.action = Action::Route {
            routes: session().routes,
        };
        assert!(s.admit(&c, 2000).is_err());
        s.routes.audio_stream_id = None;
        assert!(s.validate(2000).is_err());
    }
    #[test]
    fn unknown_fields_and_identifiers_rejected() {
        let mut value = serde_json::to_value(session()).unwrap();
        value["pipeline"] = "exec".into();
        assert!(serde_json::from_value::<Session>(value).is_err());
        let mut s = session();
        s.session_id = "x".repeat(257);
        assert!(s.validate(2000).is_err());
    }
    #[test]
    fn source_clock_must_be_valid_and_fresh() {
        use stream_descriptors::{ClockQuality, ClockSynchronizationMethod};
        let mut s = session();
        s.source_clock = Some(ClockCorrelationV1 {
            schema_version: 1,
            mapping_id: "clock-1".into(),
            source_clock_id: "source".into(),
            source_generation: "epoch-1".into(),
            source_rate_hz: 90_000,
            source_timestamp_bits: 32,
            anchor_source_ticks: 0,
            anchor_unix_ns: 1_000_000_000,
            rate_adjustment_ppb: 0,
            uncertainty_ns: 1_000_000,
            synchronization_method: ClockSynchronizationMethod::Measured,
            quality: ClockQuality::Estimated,
            observed_at_unix_ms: 1000,
            valid_until_unix_ms: 3000,
        });
        assert!(s.validate(2000).is_ok());
        assert!(s.validate(3000).is_err());
        s.source_clock.as_mut().unwrap().source_rate_hz = 0;
        assert!(s.validate(2000).is_err());
    }
}
