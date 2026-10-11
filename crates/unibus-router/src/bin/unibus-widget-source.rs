//! Trusted-config telemetry collector. No browser-supplied commands or URLs.
use anyhow::{Context, Result, ensure};
use coherence_widget_contract::{MAX_HISTORY, MESSAGE_KIND, Metric, Snapshot};
use serde::Deserialize;
use std::{
    collections::BTreeMap,
    io::Read,
    process::{Command, Stdio},
    sync::mpsc,
    time::{Duration, Instant},
};
use unibus::{
    Message,
    client::{Client, ClientConfig},
    envelope::now_ms,
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    client: ClientConfig,
    zone: String,
    poll_seconds: u64,
    feeds: Vec<Feed>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Feed {
    id: String,
    sources: Vec<Source>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Source {
    command: Vec<String>,
    metrics: Vec<Selection>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Selection {
    id: String,
    label: String,
    unit: String,
    /// An exact Prometheus series selector, or a CSV column index.
    #[serde(default)]
    series: Option<String>,
    #[serde(default)]
    column: Option<usize>,
    #[serde(default)]
    subtract: Option<String>,
    #[serde(default)]
    maximum_series: Option<String>,
    #[serde(default)]
    maximum: Option<f32>,
    #[serde(default)]
    maximum_column: Option<usize>,
    #[serde(default = "one")]
    divisor: f32,
}
fn one() -> f32 {
    1.
}
fn scalar(text: &str, selector: &str) -> Option<f32> {
    let mut values = text.lines().filter_map(|line| {
        let line = line.trim();
        if !line.starts_with(selector)
            || !line
                .as_bytes()
                .get(selector.len())
                .is_some_and(u8::is_ascii_whitespace)
        {
            return None;
        }
        line[selector.len()..]
            .split_whitespace()
            .next()?
            .parse::<f32>()
            .ok()
            .filter(|v| v.is_finite())
    });
    let first = values.next()?;
    if values.next().is_some() {
        return None;
    }
    Some(first)
}
fn selected(text: &str, metric: &Selection) -> (Option<f32>, Option<f32>) {
    let value = match (&metric.series, metric.column) {
        (Some(series), None) => scalar(text, series),
        (None, Some(column)) if text.lines().count() == 1 => text
            .trim()
            .split(',')
            .nth(column)
            .and_then(|s| s.trim().parse::<f32>().ok())
            .filter(|v| v.is_finite()),
        _ => None,
    };
    let value = if let Some(subtract) = &metric.subtract {
        value.zip(scalar(text, subtract)).map(|(a, b)| a - b)
    } else {
        value
    };
    let maximum = metric
        .maximum
        .or_else(|| {
            metric
                .maximum_column
                .and_then(|column| {
                    text.trim()
                        .split(',')
                        .nth(column)?
                        .trim()
                        .parse::<f32>()
                        .ok()
                })
                .map(|v| v / metric.divisor)
        })
        .or_else(|| {
            metric
                .maximum_series
                .as_deref()
                .and_then(|s| scalar(text, s))
                .map(|v| v / metric.divisor)
        })
        .filter(|v| *v > 0. && v.is_finite());
    (
        value.map(|v| v / metric.divisor).filter(|v| v.is_finite()),
        maximum,
    )
}
fn collect(arguments: &[String]) -> Result<String> {
    ensure!(
        !arguments.is_empty() && arguments.len() <= 32,
        "invalid collector command"
    );
    let mut command = Command::new(&arguments[0]);
    command
        .args(&arguments[1..])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .stdin(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command.spawn().context("start configured collector")?;
    let mut stdout = child.stdout.take().context("collector stdout missing")?;
    let (send, receive) = mpsc::sync_channel(1);
    let reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let result = stdout
            .by_ref()
            .take(256 * 1024 + 1)
            .read_to_end(&mut bytes)
            .map(|_| bytes);
        let _ = send.send(result);
    });
    let started = Instant::now();
    let output = loop {
        if let Ok(output) = receive.try_recv() {
            break Some(output);
        }
        if started.elapsed() > Duration::from_secs(5) {
            break None;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    #[cfg(unix)]
    if output.is_none() {
        if let Some(pid) = rustix::process::Pid::from_raw(child.id() as i32) {
            match rustix::process::kill_process_group(pid, rustix::process::Signal::KILL) {
                Ok(()) | Err(rustix::io::Errno::SRCH) => (),
                Err(error) => return Err(error.into()),
            }
        }
    }
    // Reap on all paths; never join a potentially blocked reader on a timeout.
    let mut success = false;
    loop {
        if let Some(status) = child.try_wait()? {
            success = status.success();
            break;
        }
        if started.elapsed() > Duration::from_secs(5) {
            #[cfg(unix)]
            {
                if let Some(pid) = rustix::process::Pid::from_raw(child.id() as i32) {
                    rustix::process::kill_process_group(pid, rustix::process::Signal::KILL)?;
                }
            }
            #[cfg(not(unix))]
            child.kill()?;
            child.wait()?;
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    ensure!(success, "collector failed or timed out");
    let bytes = output.context("collector timed out")??;
    reader
        .join()
        .map_err(|_| anyhow::anyhow!("collector reader panicked"))?;
    ensure!(
        bytes.len() <= 256 * 1024,
        "collector output exceeds byte budget"
    );
    String::from_utf8(bytes).context("collector output is not UTF-8")
}
fn main() -> Result<()> {
    let path = std::env::args_os()
        .nth(1)
        .context("usage: unibus-widget-source CONFIG")?;
    let metadata = std::fs::metadata(&path)?;
    ensure!(
        metadata.len() <= 64 * 1024,
        "source config exceeds byte budget"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        ensure!(
            metadata.permissions().mode() & 0o077 == 0,
            "source config must be private"
        );
    }
    let mut config: Config = serde_json::from_slice(&std::fs::read(&path)?)?;
    ensure!(
        (5..=30).contains(&config.poll_seconds)
            && config.feeds.len() <= 16
            && !config.feeds.is_empty(),
        "invalid polling cadence or feed count"
    );
    config
        .client
        .resolve_token_file(std::path::Path::new(&path));
    ensure!(
        config.client.schema_version == 2 && config.client.zones.contains(&config.zone),
        "widget sources require schema v2 and an explicit granted zone"
    );
    for feed in &config.feeds {
        ensure!(
            coherence_widget_contract::identifier(&feed.id)
                && feed.sources.len() <= 8
                && feed.sources.iter().map(|s| s.metrics.len()).sum::<usize>() <= 16,
            "invalid feed bounds"
        );
        for source in &feed.sources {
            for metric in &source.metrics {
                ensure!(
                    metric.divisor.is_finite() && metric.divisor > 0.,
                    "invalid metric divisor"
                );
            }
        }
    }
    let client = Client::connect(config.client)?;
    let mut history = BTreeMap::<(String, String), Vec<f32>>::new();
    loop {
        let start = Instant::now();
        for feed in &config.feeds {
            let mut metrics = Vec::new();
            for source in &feed.sources {
                let output = match collect(&source.command) {
                    Ok(text) => text,
                    Err(_) => {
                        eprintln!("widget {} collector unavailable", feed.id);
                        String::new()
                    }
                };
                for selection in &source.metrics {
                    let (value, maximum) = selected(&output, selection);
                    let samples = history
                        .entry((feed.id.clone(), selection.id.clone()))
                        .or_default();
                    if let Some(value) = value {
                        samples.push(value);
                        if samples.len() > MAX_HISTORY {
                            samples.remove(0);
                        }
                    }
                    metrics.push(Metric {
                        id: selection.id.clone(),
                        label: selection.label.clone(),
                        unit: selection.unit.clone(),
                        value,
                        maximum,
                        history: samples.clone(),
                    });
                }
            }
            let now = now_ms();
            let snapshot = Snapshot {
                schema_version: 1,
                widget_id: feed.id.clone(),
                observed_at_unix_ms: now,
                expires_at_unix_ms: now + config.poll_seconds * 3000,
                metrics,
            };
            snapshot.validate().map_err(anyhow::Error::msg)?;
            let envelope = client.envelope(
                Some(&config.zone),
                Message::new(MESSAGE_KIND, serde_json::to_value(snapshot)?),
            )?;
            client.send(&envelope, &[])?;
        }
        std::thread::sleep(
            Duration::from_secs(config.poll_seconds).saturating_sub(start.elapsed()),
        );
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn exact_series_only_and_missing_is_not_zero() {
        assert_eq!(scalar("vm_count 0\nvm_count_extra 9", "vm_count"), Some(0.));
        assert_eq!(scalar("vm_count_extra 9", "vm_count"), None);
        assert_eq!(scalar("vm_count NaN", "vm_count"), None);
        assert_eq!(scalar("vm_count 1\nvm_count 2", "vm_count"), None);
        assert_eq!(
            scalar("load{interval=\"1m\"} 3", "load{interval=\"1m\"}"),
            Some(3.)
        );
    }
}
