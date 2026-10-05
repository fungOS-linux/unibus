//! Read-only Bambu protocol owner. Vendor JSON never becomes a control grant.
use anyhow::{Result, ensure};
use serde_json::Value;
pub use unibus_device::printer::{
    LinkState, PrinterStatus, REPORT_MAX_AGE_MS, SNAPSHOT_TTL_MS, STATUS_CAPABILITY, STATUS_KIND,
    Sample, Telemetry,
};
pub fn valid_serial(serial: &str) -> bool {
    !serial.is_empty()
        && serial.len() <= 64
        && serial
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
}
pub fn report_topic(serial: &str) -> Result<String> {
    ensure!(valid_serial(serial), "invalid Bambu serial");
    Ok(format!("device/{serial}/report"))
}
#[derive(Clone)]
pub struct StatusAccumulator {
    serial: String,
    observer: String,
    revision: u64,
    telemetry: Telemetry,
    last_report: Option<u64>,
    received: Option<std::time::Instant>,
    link: LinkState,
}
impl StatusAccumulator {
    pub fn new(serial: String, observer: String) -> Result<Self> {
        ensure!(valid_serial(&serial), "invalid Bambu serial");
        ensure!(
            unibus_mqtt::valid_bridge_identifier(&observer),
            "invalid observer"
        );
        Ok(Self {
            serial,
            observer,
            revision: 0,
            telemetry: Telemetry::default(),
            last_report: None,
            received: None,
            link: LinkState::Waiting,
        })
    }
    /// Transactional delta reduction. A malformed report cannot partially mutate
    /// trusted state or renew freshness. No unspecified field becomes zero.
    pub fn apply(
        &mut self,
        topic: &str,
        payload: &[u8],
        retained: bool,
        duplicate: bool,
        now: u64,
    ) -> Result<bool> {
        ensure!(
            topic == report_topic(&self.serial)?,
            "report topic does not match enrolled device"
        );
        ensure!(
            payload.len() <= unibus_mqtt::subscriber::MAX_REPORT_BYTES,
            "Bambu report exceeds limit"
        );
        if retained || duplicate {
            return Ok(false);
        }
        let root: Value = serde_json::from_slice(payload)?;
        let Some(print) = root.get("print").and_then(Value::as_object) else {
            return Ok(false);
        };
        let mut next = self.telemetry.clone();
        let mut changed = false;
        if let Some(value) = print.get("subtask_id") {
            let id = string(value, 128)?;
            if next.job_id.as_ref().is_some_and(|old| old.value != id) {
                next.job = None;
                next.progress_percent = None;
                next.remaining_minutes = None;
                next.layer = None;
                next.layers = None;
            }
            next.job_id = Some(Sample {
                value: id,
                observed_at_unix_ms: now,
            });
            changed = true;
        }
        macro_rules! number {
            ($key:literal,$field:ident,$max:expr) => {
                if let Some(value) = print.get($key) {
                    next.$field = Some(Sample {
                        value: number(value, $max)?,
                        observed_at_unix_ms: now,
                    });
                    changed = true;
                }
            };
        }
        macro_rules! integer {
            ($key:literal,$field:ident,$max:expr) => {
                if let Some(value) = print.get($key) {
                    let n = number(value, $max as f64)?;
                    ensure!(n.fract() == 0.0, "Bambu integer field is fractional");
                    next.$field = Some(Sample {
                        value: n as u64,
                        observed_at_unix_ms: now,
                    });
                    changed = true;
                }
            };
        }
        macro_rules! text {
            ($key:literal,$field:ident,$max:expr) => {
                if let Some(value) = print.get($key) {
                    next.$field = Some(Sample {
                        value: string(value, $max)?,
                        observed_at_unix_ms: now,
                    });
                    changed = true;
                }
            };
        }
        text!("gcode_state", state, 64);
        text!("subtask_name", job, 256);
        text!("wifi_signal", wifi, 32);
        number!("mc_percent", progress_percent, 100.0);
        integer!("mc_remaining_time", remaining_minutes, 10080);
        integer!("layer_num", layer, 1_000_000);
        integer!("total_layer_num", layers, 1_000_000);
        number!("nozzle_temper", nozzle_c, 600.0);
        number!("nozzle_target_temper", nozzle_target_c, 600.0);
        number!("bed_temper", bed_c, 200.0);
        number!("bed_target_temper", bed_target_c, 200.0);
        number!("spd_mag", speed_percent, 1000.0);
        if changed {
            self.telemetry = next;
            self.revision = self
                .revision
                .checked_add(1)
                .ok_or_else(|| anyhow::anyhow!("status revision overflow"))?;
            self.last_report = Some(now);
            self.received = Some(std::time::Instant::now());
            self.link = LinkState::Live;
        }
        Ok(changed)
    }
    pub fn mark(&mut self, state: LinkState) {
        self.link = state;
    }
    pub fn snapshot(&self, now: u64) -> PrinterStatus {
        let link = if self.link == LinkState::Live
            && self
                .received
                .is_some_and(|t| t.elapsed().as_millis() > REPORT_MAX_AGE_MS as u128)
        {
            LinkState::Stale
        } else {
            self.link
        };
        PrinterStatus {
            schema_version: 1,
            device_id: format!("bambu:{}", self.serial),
            serial: self.serial.clone(),
            observer: self.observer.clone(),
            revision: self.revision,
            observed_at_unix_ms: now,
            valid_until_unix_ms: now.saturating_add(SNAPSHOT_TTL_MS),
            last_report_at_unix_ms: self.last_report,
            link,
            read_only: true,
            telemetry: self.telemetry.clone(),
        }
    }
}
fn string(value: &Value, max: usize) -> Result<String> {
    let s = value
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Bambu text field is not a string"))?;
    ensure!(
        s.len() <= max && !s.chars().any(char::is_control),
        "invalid Bambu text field"
    );
    Ok(s.to_owned())
}
fn number(value: &Value, max: f64) -> Result<f64> {
    let n = value
        .as_f64()
        .ok_or_else(|| anyhow::anyhow!("Bambu metric is not numeric"))?;
    ensure!(
        n.is_finite() && (0.0..=max).contains(&n),
        "Bambu metric out of range"
    );
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn disconnected_and_silent_reports_do_not_claim_live_status() {
        let mut s = state();
        s.apply(
            "device/SERIAL1/report",
            br#"{"print":{"nozzle_temper":210}}"#,
            false,
            false,
            100,
        )
        .unwrap();
        s.received = Some(std::time::Instant::now() - std::time::Duration::from_secs(61));
        assert_eq!(s.snapshot(61100).link, LinkState::Stale);
        s.mark(LinkState::Disconnected);
        assert_eq!(s.snapshot(61100).link, LinkState::Disconnected);
        assert_eq!(
            s.snapshot(61100)
                .telemetry
                .nozzle_c
                .unwrap()
                .observed_at_unix_ms,
            100
        );
        s.snapshot(61100).validate(61100).unwrap();
        assert!(s.snapshot(61100).validate(67100).is_err());
    }
    fn state() -> StatusAccumulator {
        StatusAccumulator::new("SERIAL1".into(), "bambu-observer".into()).unwrap()
    }
    #[test]
    fn merges_deltas_without_renewing_missing_fields() {
        let mut s = state();
        s.apply(
            "device/SERIAL1/report",
            br#"{"print":{"nozzle_temper":210,"mc_percent":20}}"#,
            false,
            false,
            100,
        )
        .unwrap();
        s.apply(
            "device/SERIAL1/report",
            br#"{"print":{"mc_percent":21}}"#,
            false,
            false,
            200,
        )
        .unwrap();
        let snap = s.snapshot(200);
        assert_eq!(snap.telemetry.nozzle_c.unwrap().observed_at_unix_ms, 100);
        assert_eq!(snap.telemetry.progress_percent.unwrap().value, 21.0);
        assert_eq!(snap.revision, 2);
    }
    #[test]
    fn invalid_report_does_not_partially_mutate_or_refresh_state() {
        let mut s = state();
        assert!(
            s.apply(
                "device/SERIAL1/report",
                br#"{"print":{"nozzle_temper":200,"mc_percent":101}}"#,
                false,
                false,
                100
            )
            .is_err()
        );
        assert_eq!(s.snapshot(200).revision, 0);
        assert!(s.snapshot(200).last_report_at_unix_ms.is_none());
    }
    #[test]
    fn retained_duplicate_other_device_and_command_replies_never_claim_live_data() {
        let mut s = state();
        let report = br#"{"print":{"mc_percent":10}}"#;
        assert!(
            !s.apply("device/SERIAL1/report", report, true, false, 100)
                .unwrap()
        );
        assert!(
            !s.apply("device/SERIAL1/report", report, false, true, 100)
                .unwrap()
        );
        assert!(
            s.apply("device/OTHER/report", report, false, false, 100)
                .is_err()
        );
        assert!(
            !s.apply(
                "device/SERIAL1/report",
                br#"{"print":{"command":"pause","result":"success"}}"#,
                false,
                false,
                100
            )
            .unwrap()
        );
        assert_eq!(s.snapshot(100).link, LinkState::Waiting);
    }
    #[test]
    fn new_job_does_not_inherit_previous_job_progress() {
        let mut s = state();
        s.apply(
            "device/SERIAL1/report",
            br#"{"print":{"subtask_id":"one","mc_percent":90}}"#,
            false,
            false,
            100,
        )
        .unwrap();
        s.apply(
            "device/SERIAL1/report",
            br#"{"print":{"subtask_id":"two"}}"#,
            false,
            false,
            200,
        )
        .unwrap();
        assert!(s.snapshot(200).telemetry.progress_percent.is_none());
    }
}
