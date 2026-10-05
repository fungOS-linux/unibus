//! Generic typed stream demand and event contracts.
//!
//! A producer owns the meaning of a stream and its payload contract. Unibus
//! owns only the stable identity, bounded demand lease, and latency-first
//! envelope used to move samples. This lets consumers such as Canvas widgets
//! declare what they need without either side knowing the other's internals.

use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

pub const STREAM_INTEREST_KIND: &str = "unibus.stream.interest.v1";
pub const STREAM_INTEREST_RELEASE_KIND: &str = "unibus.stream.interest.release.v1";
pub const STREAM_EVENT_KIND: &str = "unibus.stream.event.v1";

const MAX_STREAMS_PER_INTEREST: usize = 128;
const MAX_STREAM_LEASE_MS: u64 = 5 * 60 * 1_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StreamDirection {
    Consume,
    Publish,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StreamImportance {
    Needed,
    Wanted,
}

/// A component's transport-neutral declaration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StreamRequirement {
    pub stream_id: String,
    pub contract: String,
    pub direction: StreamDirection,
    pub importance: StreamImportance,
}

/// Renewable demand for a set of canonical stream ids.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StreamInterest {
    pub lease_id: String,
    pub consumer_id: String,
    pub stream_ids: Vec<String>,
    pub issued_at_unix_ms: u64,
    pub expires_at_unix_ms: u64,
}

/// Immediate cancellation of one renewable demand lease.
///
/// Expiry remains the crash-safe fallback. A release lets visibility-aware
/// consumers stop upstream work as soon as their view leaves the screen.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StreamInterestRelease {
    pub lease_id: String,
    pub consumer_id: String,
}

/// One bounded, typed event on a canonical stream.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StreamEvent {
    pub stream_id: String,
    pub contract: String,
    pub sequence: u64,
    pub observed_at_unix_ms: u64,
    pub value: serde_json::Value,
}

impl StreamRequirement {
    pub fn validate(&self) -> Result<()> {
        validate_id(&self.stream_id, "stream id")?;
        validate_id(&self.contract, "stream contract")
    }
}

impl StreamInterest {
    pub fn validate(&self) -> Result<()> {
        validate_id(&self.lease_id, "stream lease id")?;
        validate_id(&self.consumer_id, "stream consumer id")?;
        ensure!(!self.stream_ids.is_empty(), "stream interest is empty");
        ensure!(
            self.stream_ids.len() <= MAX_STREAMS_PER_INTEREST,
            "stream interest exceeds {MAX_STREAMS_PER_INTEREST} ids"
        );
        let mut previous = None;
        for stream_id in &self.stream_ids {
            validate_id(stream_id, "stream id")?;
            ensure!(
                previous.is_none_or(|value: &String| value < stream_id),
                "stream ids must be sorted and unique"
            );
            previous = Some(stream_id);
        }
        ensure!(
            self.issued_at_unix_ms < self.expires_at_unix_ms,
            "stream interest expires before it starts"
        );
        ensure!(
            self.expires_at_unix_ms - self.issued_at_unix_ms <= MAX_STREAM_LEASE_MS,
            "stream interest exceeds maximum lease"
        );
        Ok(())
    }

    pub fn active_at(&self, now_unix_ms: u64) -> bool {
        self.issued_at_unix_ms <= now_unix_ms && now_unix_ms < self.expires_at_unix_ms
    }
}

impl StreamInterestRelease {
    pub fn validate(&self) -> Result<()> {
        validate_id(&self.lease_id, "stream lease id")?;
        validate_id(&self.consumer_id, "stream consumer id")
    }
}

impl StreamEvent {
    pub fn validate(&self) -> Result<()> {
        validate_id(&self.stream_id, "stream id")?;
        validate_id(&self.contract, "stream contract")?;
        ensure!(self.sequence > 0, "stream event sequence must be positive");
        ensure!(
            self.observed_at_unix_ms > 0,
            "stream event timestamp must be positive"
        );
        ensure!(
            serde_json::to_vec(&self.value)?.len() <= 64 * 1024,
            "stream event value exceeds 64 KiB"
        );
        Ok(())
    }
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

    #[test]
    fn interest_is_bounded_sorted_and_expiring() {
        let interest = StreamInterest {
            lease_id: "canvas:studio-mix".into(),
            consumer_id: "canvas-neo".into(),
            stream_ids: vec!["studio/mix/levels".into(), "studio/transport".into()],
            issued_at_unix_ms: 1_000,
            expires_at_unix_ms: 31_000,
        };
        interest.validate().unwrap();
        assert!(interest.active_at(30_999));
        assert!(!interest.active_at(31_000));
    }

    #[test]
    fn event_rejects_unbounded_or_unidentified_payloads() {
        let event = StreamEvent {
            stream_id: "studio/mix/control".into(),
            contract: "synesthesia.mix.control.v1".into(),
            sequence: 1,
            observed_at_unix_ms: 1,
            value: serde_json::json!({"strip": 1, "gain": 0.5}),
        };
        event.validate().unwrap();
        let mut invalid = event;
        invalid.stream_id = "not a stream?".into();
        assert!(invalid.validate().is_err());
    }

    #[test]
    fn release_names_exactly_one_consumer_owned_lease() {
        let release = StreamInterestRelease {
            lease_id: "canvas:studio-mix".into(),
            consumer_id: "canvas-neo".into(),
        };
        release.validate().unwrap();
        let mut invalid = release;
        invalid.lease_id = "not a lease?".into();
        assert!(invalid.validate().is_err());
    }
}
