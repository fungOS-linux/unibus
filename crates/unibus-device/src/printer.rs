//! Presentation-neutral, read-only printer status. Discovery is not authority.
use serde::{Deserialize, Serialize};
pub const STATUS_KIND: &str = "printer.status.v1";
pub const STATUS_CAPABILITY: &str = "printer.status";
pub const REPORT_MAX_AGE_MS: u64 = 60_000;
pub const SNAPSHOT_TTL_MS: u64 = 6000;
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Sample<T> {
    pub value: T,
    pub observed_at_unix_ms: u64,
}
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Telemetry {
    pub state: Option<Sample<String>>,
    pub job: Option<Sample<String>>,
    pub job_id: Option<Sample<String>>,
    pub progress_percent: Option<Sample<f64>>,
    pub remaining_minutes: Option<Sample<u64>>,
    pub layer: Option<Sample<u64>>,
    pub layers: Option<Sample<u64>>,
    pub nozzle_c: Option<Sample<f64>>,
    pub nozzle_target_c: Option<Sample<f64>>,
    pub bed_c: Option<Sample<f64>>,
    pub bed_target_c: Option<Sample<f64>>,
    pub speed_percent: Option<Sample<f64>>,
    pub wifi: Option<Sample<String>>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LinkState {
    Waiting,
    Live,
    Stale,
    Disconnected,
    InvalidReport,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PrinterStatus {
    pub schema_version: u32,
    pub device_id: String,
    pub serial: String,
    pub observer: String,
    pub revision: u64,
    pub observed_at_unix_ms: u64,
    pub valid_until_unix_ms: u64,
    pub last_report_at_unix_ms: Option<u64>,
    pub link: LinkState,
    pub read_only: bool,
    pub telemetry: Telemetry,
}
impl PrinterStatus {
    /// Validate before projecting untrusted data into a UI. Individual samples
    /// retain their original times even when a new snapshot is emitted.
    pub fn validate(&self, now: u64) -> Result<(), String> {
        let text = |s: &str, max| s.len() <= max && !s.chars().any(char::is_control);
        if self.schema_version != 1
            || !self.read_only
            || self.serial.is_empty()
            || self.serial.len() > 64
            || !self
                .serial
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-')
            || !text(&self.device_id, 128)
            || self.device_id.is_empty()
            || !text(&self.observer, 128)
            || self.observer.is_empty()
            || self.observed_at_unix_ms > now.saturating_add(2000)
            || self.valid_until_unix_ms <= now
            || self.valid_until_unix_ms <= self.observed_at_unix_ms
            || self.valid_until_unix_ms - self.observed_at_unix_ms > SNAPSHOT_TTL_MS
            || self
                .last_report_at_unix_ms
                .is_some_and(|t| t > self.observed_at_unix_ms)
        {
            return Err("invalid or expired printer status".into());
        }
        let stamp = |t| t <= self.observed_at_unix_ms;
        macro_rules! number {
            ($field:ident,$max:expr) => {
                if self.telemetry.$field.as_ref().is_some_and(|s| {
                    !stamp(s.observed_at_unix_ms)
                        || !s.value.is_finite()
                        || s.value < 0.0
                        || s.value > $max
                }) {
                    return Err(concat!("invalid printer ", stringify!($field)).into());
                }
            };
        }
        number!(progress_percent, 100.0);
        number!(nozzle_c, 600.0);
        number!(nozzle_target_c, 600.0);
        number!(bed_c, 200.0);
        number!(bed_target_c, 200.0);
        number!(speed_percent, 1000.0);
        macro_rules! integer {
            ($field:ident,$max:expr) => {
                if self
                    .telemetry
                    .$field
                    .as_ref()
                    .is_some_and(|s| !stamp(s.observed_at_unix_ms) || s.value > $max)
                {
                    return Err("invalid printer count".into());
                }
            };
        }
        integer!(layer, 1_000_000);
        integer!(layers, 1_000_000);
        integer!(remaining_minutes, 10_080);
        macro_rules! string {
            ($field:ident,$max:expr) => {
                if self
                    .telemetry
                    .$field
                    .as_ref()
                    .is_some_and(|s| !stamp(s.observed_at_unix_ms) || !text(&s.value, $max))
                {
                    return Err("invalid printer text".into());
                }
            };
        }
        string!(state, 64);
        string!(job, 256);
        string!(job_id, 128);
        string!(wifi, 32);
        Ok(())
    }
}
