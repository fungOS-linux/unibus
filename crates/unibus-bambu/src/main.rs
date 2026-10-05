use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;
use std::{
    io::Read,
    net::SocketAddr,
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, Instant},
};
use unibus::{
    Message,
    client::{Client, ClientConfig},
};
use unibus_bambu::{LinkState, STATUS_CAPABILITY, STATUS_KIND, StatusAccumulator, report_topic};
use unibus_mqtt::subscriber::{ReportEvent, ReportSubscriber, parse_pin};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    bus: ClientConfig,
    destination: String,
    serial: String,
    mqtt_endpoint: SocketAddr,
    certificate_sha256: String,
    access_code_file: PathBuf,
}
impl Config {
    fn validate(&self) -> Result<()> {
        report_topic(&self.serial)?;
        parse_pin(&self.certificate_sha256)?;
        ensure!(
            unibus_mqtt::valid_bridge_identifier(&self.destination),
            "invalid status destination"
        );
        ensure!(
            self.bus.schema_version == unibus::SCHEMA_VERSION_V2
                && self.bus.capabilities.iter().any(|v| v == STATUS_CAPABILITY),
            "Bambu status requires schema v2 and printer.status capability"
        );
        ensure!(
            self.bus
                .hub_endpoint
                .parse::<SocketAddr>()
                .is_ok_and(|a| a.ip().is_loopback()),
            "remote bus must use an authenticated tunnel to a loopback endpoint"
        );
        ensure!(
            !self.mqtt_endpoint.ip().is_unspecified()
                && !self.mqtt_endpoint.ip().is_multicast()
                && self.mqtt_endpoint.port() > 0,
            "invalid printer endpoint"
        );
        ensure!(
            self.access_code_file.is_absolute(),
            "access_code_file must be absolute"
        );
        Ok(())
    }
}
fn access_code(path: &Path) -> Result<String> {
    let file = std::fs::File::open(path).context("open private Bambu access-code file")?;
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file() && metadata.len() <= 256,
        "invalid access-code file"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        ensure!(
            metadata.permissions().mode() & 0o077 == 0,
            "access-code file must not be accessible by group/others (chmod 600)"
        );
    }
    let mut code = String::new();
    file.take(257).read_to_string(&mut code)?;
    let code = code.trim().to_owned();
    ensure!(
        !code.is_empty() && code.len() <= 128 && code.bytes().all(|b| b.is_ascii_alphanumeric()),
        "invalid Bambu access-code format"
    );
    Ok(code)
}
fn publish(bus: &Client, destination: &str, state: &StatusAccumulator) -> Result<()> {
    let snapshot = state.snapshot(unibus::envelope::now_ms());
    let mut envelope = bus.envelope(
        Some(destination),
        Message::new(STATUS_KIND, serde_json::to_value(snapshot)?),
    )?;
    envelope.ttl_ms = Some(unibus_bambu::SNAPSHOT_TTL_MS);
    bus.send(&envelope, &[])
}
fn discover() -> Result<()> {
    let output =
        Command::new(std::env::var_os("MYCELIUM_BIN").unwrap_or_else(|| "mycelium".into()))
            .args(["topology", "--json"])
            .output()?;
    ensure!(
        output.status.success(),
        "Mycelium topology discovery failed"
    );
    let topology: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    let advertisements = topology
        .get("advertisements")
        .and_then(|v| v.as_object())
        .context("topology missing advertisements")?;
    let now = unibus::envelope::now_ms() / 1000;
    let devices: Vec<_> = advertisements
        .values()
        .filter(|v| {
            v.get("service_type").and_then(|v| v.as_str())
                == Some("urn:bambulab-com:device:3dprinter:1")
                && v.get("last_seen")
                    .and_then(|v| v.as_u64())
                    .is_some_and(|seen| {
                        seen <= now.saturating_add(2)
                            && seen.saturating_add(
                                v.get("ttl")
                                    .and_then(|v| v.as_u64())
                                    .unwrap_or(900)
                                    .min(900),
                            ) > now
                    })
        })
        .collect();
    println!(
        "{}",
        serde_json::to_string_pretty(
            &serde_json::json!({"schema_version":1,"authorization":"none; enrollment required","printers":devices})
        )?
    );
    Ok(())
}
fn main() -> Result<()> {
    let path = std::env::args_os()
        .nth(1)
        .context("usage: unibus-bambu <config.json> | discover")?;
    if path == "discover" {
        return discover();
    }
    let mut config: Config = serde_json::from_slice(&std::fs::read(path)?)?;
    config.validate()?;
    let secret = access_code(&config.access_code_file)?;
    config.bus.read_timeout_ms = 1;
    let mut state = StatusAccumulator::new(config.serial.clone(), config.bus.producer_id.clone())?;
    let bus = Client::connect(config.bus)?;
    publish(&bus, &config.destination, &state)?;
    let topic = report_topic(&config.serial)?;
    let mut mqtt = ReportSubscriber::connect(
        config.mqtt_endpoint,
        &bus.config().producer_id,
        &topic,
        "bblp",
        &secret,
        parse_pin(&config.certificate_sha256)?,
    )?;
    let mut next = Instant::now() + Duration::from_secs(1);
    eprintln!(
        "Bambu read-only observer started for {}; certificate pinned; no printer command path",
        config.serial
    );
    loop {
        match mqtt.poll(Duration::from_millis(50)) {
            Ok(Some(ReportEvent::Report {
                topic,
                payload,
                retained,
                duplicate,
            })) => {
                if let Err(error) = state.apply(
                    &topic,
                    &payload,
                    retained,
                    duplicate,
                    unibus::envelope::now_ms(),
                ) {
                    state.mark(LinkState::InvalidReport);
                    eprintln!("Bambu report rejected: {error:#}");
                }
            }
            Ok(_) => {}
            Err(error) => {
                state.mark(LinkState::Disconnected);
                publish(&bus, &config.destination, &state)
                    .context("publish disconnected Bambu status")?;
                return Err(error);
            }
        }
        if Instant::now() >= next {
            publish(&bus, &config.destination, &state)?;
            next = Instant::now() + Duration::from_secs(1);
        }
        if let Some((event, _)) = bus.try_read()? {
            if event.message.kind == unibus::kinds::V1_REPLY
                && event.message.body.get("success").and_then(|v| v.as_bool()) == Some(false)
            {
                bail!(
                    "Unibus rejected Bambu status publication: {:?}",
                    event.message.body
                );
            }
            // Deliberately no command dispatcher or raw-MQTT publisher.
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn private_access_code_is_bounded_and_never_returned_in_errors() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("code");
        std::fs::write(&path, "TEST1234").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        assert_eq!(access_code(&path).unwrap(), "TEST1234");
        std::fs::write(&path, "password-with-\n-controls").unwrap();
        let error = access_code(&path).unwrap_err().to_string();
        assert!(!error.contains("password-with"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
            assert!(access_code(&path).is_err());
        }
    }
}
