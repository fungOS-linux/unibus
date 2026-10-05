//! Capability-gated recording and deterministic replay for typed Unibus data.
//!
//! The newline-delimited format is intentionally boring and recoverable. A
//! header freezes semantic descriptor and provenance snapshots; subsequent
//! records preserve original envelopes and capture offsets. Object-store
//! ownership stays outside this crate.

use std::{
    collections::BTreeSet,
    io::{BufRead, Write},
};

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use unibus::Envelope;

pub const RECORD_CAPABILITY: &str = "unibus.record.write";
pub const REPLAY_CAPABILITY: &str = "unibus.record.replay";
pub const FORMAT_VERSION: u32 = 1;
pub const MAX_METADATA_BYTES: usize = 1024 * 1024;
pub const MAX_RECORDS: u64 = 10_000_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordingAuthority {
    capabilities: BTreeSet<String>,
}

impl RecordingAuthority {
    pub fn new(capabilities: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self {
            capabilities: capabilities.into_iter().map(Into::into).collect(),
        }
    }

    fn require(&self, capability: &str) -> Result<()> {
        ensure!(
            self.capabilities.contains(capability),
            "missing capability {capability}"
        );
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecordingHeader {
    pub format_version: u32,
    pub recording_id: String,
    pub installation_id: String,
    pub created_at_unix_ms: u64,
    #[serde(default)]
    pub descriptor_snapshots: Vec<Value>,
    #[serde(default)]
    pub provenance_snapshots: Vec<Value>,
}

impl RecordingHeader {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.format_version == FORMAT_VERSION,
            "unsupported recording format"
        );
        validate_id(&self.recording_id, "recording id")?;
        validate_id(&self.installation_id, "installation id")?;
        ensure!(
            self.descriptor_snapshots.len() <= 1024,
            "too many descriptor snapshots"
        );
        ensure!(
            self.provenance_snapshots.len() <= 1024,
            "too many provenance snapshots"
        );
        ensure!(
            serde_json::to_vec(self)?.len() <= MAX_METADATA_BYTES,
            "recording metadata is too large"
        );
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecordedEnvelope {
    pub sequence: u64,
    pub captured_offset_ms: u64,
    pub envelope: Envelope,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "record_type", rename_all = "snake_case")]
enum RecordLine {
    Header { header: RecordingHeader },
    Envelope { record: RecordedEnvelope },
}

pub struct Recorder<W> {
    writer: W,
    header: RecordingHeader,
    next_sequence: u64,
}

impl<W: Write> Recorder<W> {
    pub fn new(
        mut writer: W,
        header: RecordingHeader,
        authority: &RecordingAuthority,
    ) -> Result<Self> {
        authority.require(RECORD_CAPABILITY)?;
        header.validate()?;
        write_line(
            &mut writer,
            &RecordLine::Header {
                header: header.clone(),
            },
        )?;
        Ok(Self {
            writer,
            header,
            next_sequence: 0,
        })
    }

    pub fn record(&mut self, captured_at_unix_ms: u64, envelope: Envelope) -> Result<()> {
        ensure!(
            self.next_sequence < MAX_RECORDS,
            "recording reached its record bound"
        );
        ensure!(
            envelope.installation_id == self.header.installation_id,
            "recording installation mismatch"
        );
        ensure!(
            captured_at_unix_ms >= self.header.created_at_unix_ms,
            "capture predates recording"
        );
        let record = RecordedEnvelope {
            sequence: self.next_sequence,
            captured_offset_ms: captured_at_unix_ms - self.header.created_at_unix_ms,
            envelope,
        };
        write_line(&mut self.writer, &RecordLine::Envelope { record })?;
        self.next_sequence += 1;
        Ok(())
    }

    pub fn into_inner(self) -> W {
        self.writer
    }
}

fn write_line(writer: &mut impl Write, value: &RecordLine) -> Result<()> {
    serde_json::to_writer(&mut *writer, value)?;
    writer.write_all(b"\n")?;
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReplayRate {
    /// Recorded duration multiplier numerator.
    pub numerator: u32,
    /// Recorded duration multiplier denominator.
    pub denominator: u32,
}

impl ReplayRate {
    pub const REALTIME: Self = Self {
        numerator: 1,
        denominator: 1,
    };

    fn scale(self, value: u64) -> Result<u64> {
        ensure!(
            self.numerator > 0 && self.denominator > 0,
            "invalid replay rate"
        );
        let scaled = u128::from(value)
            .checked_mul(u128::from(self.numerator))
            .context("replay time overflow")?
            / u128::from(self.denominator);
        scaled.try_into().context("replay time overflow")
    }
}

#[derive(Debug, Clone)]
pub struct ReplayPolicy {
    pub replay_start_unix_ms: u64,
    pub rate: ReplayRate,
    /// Empty means no message contract is authorized.
    pub allowed_message_kinds: BTreeSet<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ReplayEvent {
    pub sequence: u64,
    pub delay_ms: u64,
    pub envelope: Envelope,
}

pub fn read_replay(
    reader: impl BufRead,
    authority: &RecordingAuthority,
    policy: &ReplayPolicy,
) -> Result<(RecordingHeader, Vec<ReplayEvent>)> {
    authority.require(REPLAY_CAPABILITY)?;
    ensure!(
        policy.rate.numerator > 0 && policy.rate.denominator > 0,
        "invalid replay rate"
    );
    let mut lines = reader.lines();
    let first = lines.next().context("recording is empty")??;
    ensure!(
        first.len() <= MAX_METADATA_BYTES,
        "recording header is too large"
    );
    let RecordLine::Header { header } =
        serde_json::from_str(&first).context("decode recording header")?
    else {
        bail!("recording does not begin with a header")
    };
    header.validate()?;

    let mut events = Vec::new();
    let mut expected_sequence = 0_u64;
    let mut previous_offset = 0_u64;
    for line in lines {
        let line = line.context("read recording line")?;
        ensure!(
            line.len() <= unibus::MAX_FRAME as usize * 2,
            "recording line is too large"
        );
        let RecordLine::Envelope { mut record } = serde_json::from_str(&line)
            .with_context(|| format!("decode recording sequence {expected_sequence}"))?
        else {
            bail!("recording contains a second header")
        };
        ensure!(
            record.sequence == expected_sequence,
            "non-contiguous recording sequence"
        );
        ensure!(
            record.captured_offset_ms >= previous_offset,
            "recording time moved backwards"
        );
        ensure!(
            record.envelope.installation_id == header.installation_id,
            "recording installation mismatch"
        );
        ensure!(
            policy
                .allowed_message_kinds
                .contains(&record.envelope.message.kind),
            "message contract {} is not authorized for replay",
            record.envelope.message.kind
        );
        let delay_ms = policy.rate.scale(record.captured_offset_ms)?;
        record.envelope.issued_at_unix_ms = policy
            .replay_start_unix_ms
            .checked_add(delay_ms)
            .context("replay timestamp overflow")?;
        events.push(ReplayEvent {
            sequence: record.sequence,
            delay_ms,
            envelope: record.envelope,
        });
        expected_sequence += 1;
        previous_offset = record.captured_offset_ms;
        ensure!(
            expected_sequence <= MAX_RECORDS,
            "recording exceeds record bound"
        );
    }
    Ok((header, events))
}

fn validate_id(value: &str, label: &str) -> Result<()> {
    ensure!(!value.is_empty() && value.len() <= 128, "invalid {label}");
    ensure!(
        value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric()
                || matches!(byte, b'.' | b'-' | b'_' | b':' | b'/')),
        "invalid {label}"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Cursor;
    use unibus::{Message, SCHEMA_VERSION_V2};

    fn header() -> RecordingHeader {
        RecordingHeader {
            format_version: FORMAT_VERSION,
            recording_id: "test/session-1".into(),
            installation_id: "home".into(),
            created_at_unix_ms: 1_000,
            descriptor_snapshots: vec![json!({"stream_id":"camera/front"})],
            provenance_snapshots: vec![json!({"producer":"qemu"})],
        }
    }

    fn envelope(kind: &str, ttl_ms: u64) -> Envelope {
        Envelope {
            schema_version: SCHEMA_VERSION_V2,
            installation_id: "home".into(),
            producer_id: "source".into(),
            destination: None,
            target_screen: None,
            request_id: "recorded-1".into(),
            issued_at_unix_ms: 1_000,
            ttl_ms: Some(ttl_ms),
            payload_bytes: 0,
            token: String::new(),
            message: Message::new(kind, json!({"value":1})),
        }
    }

    #[test]
    fn recording_requires_explicit_capability() {
        assert!(
            Recorder::new(
                Vec::new(),
                header(),
                &RecordingAuthority::new([] as [String; 0])
            )
            .is_err()
        );
    }

    #[test]
    fn replay_is_ordered_scaled_and_rebased_without_hiding_staleness() {
        let write = RecordingAuthority::new([RECORD_CAPABILITY]);
        let mut recorder = Recorder::new(Vec::new(), header(), &write).unwrap();
        recorder
            .record(1_000, envelope("stream.endpoint.v2", 100))
            .unwrap();
        recorder
            .record(1_100, envelope("sensor.observation.v2", 1_000))
            .unwrap();
        let bytes = recorder.into_inner();
        let policy = ReplayPolicy {
            replay_start_unix_ms: 10_000,
            rate: ReplayRate {
                numerator: 1,
                denominator: 2,
            },
            allowed_message_kinds: BTreeSet::from([
                "stream.endpoint.v2".into(),
                "sensor.observation.v2".into(),
            ]),
        };
        let (_, events) = read_replay(
            Cursor::new(bytes),
            &RecordingAuthority::new([REPLAY_CAPABILITY]),
            &policy,
        )
        .unwrap();
        assert_eq!(
            events
                .iter()
                .map(|event| event.delay_ms)
                .collect::<Vec<_>>(),
            [0, 50]
        );
        assert!(events[0].envelope.validate("home", 10_100).is_ok());
        assert!(events[0].envelope.validate("home", 10_101).is_err());
    }

    #[test]
    fn truncated_and_unknown_contracts_fail_closed() {
        let write = RecordingAuthority::new([RECORD_CAPABILITY]);
        let mut recorder = Recorder::new(Vec::new(), header(), &write).unwrap();
        recorder
            .record(1_000, envelope("unknown.contract.v1", 100))
            .unwrap();
        let mut bytes = recorder.into_inner();
        let policy = ReplayPolicy {
            replay_start_unix_ms: 10_000,
            rate: ReplayRate::REALTIME,
            allowed_message_kinds: BTreeSet::new(),
        };
        assert!(
            read_replay(
                Cursor::new(&bytes),
                &RecordingAuthority::new([REPLAY_CAPABILITY]),
                &policy
            )
            .is_err()
        );
        bytes.truncate(bytes.len() - 3);
        assert!(
            read_replay(
                Cursor::new(bytes),
                &RecordingAuthority::new([REPLAY_CAPABILITY]),
                &policy
            )
            .is_err()
        );
    }
}
