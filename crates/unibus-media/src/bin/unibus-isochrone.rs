//! Read-only publisher for the existing Isochrone agent and health files.
use anyhow::{Context, Result, bail, ensure};
use isochrone::control::{
    AgentTarget, ReceiverHealth, Request, Response, RoutePhase, RouteRole, call,
};
use serde::Deserialize;
use std::{io::Read, path::PathBuf, time::Duration};
use unibus::{
    Message,
    client::{Client, ClientConfig},
};
use unibus_media::{
    ENDPOINT_ANNOUNCEMENT_V2_KIND, IsochroneEncoding, IsochroneRtpStream, STREAM_DESCRIPTOR_V2_KIND,
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    bus: ClientConfig,
    agent: AgentTarget,
    state_dir: PathBuf,
    zone: String,
    destination: String,
}

fn read_health(path: PathBuf) -> Result<Option<ReceiverHealth>> {
    let file = match std::fs::File::open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("open receiver health"),
    };
    let mut bytes = Vec::new();
    file.take(16_385).read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= 16_384, "receiver health exceeds bound");
    let health: ReceiverHealth = serde_json::from_slice(&bytes)?;
    ensure!(
        health.schema_version == 1,
        "unsupported receiver health schema"
    );
    Ok(Some(health))
}

fn main() -> Result<()> {
    let path = std::env::args()
        .nth(1)
        .context("usage: unibus-isochrone <config.json>")?;
    let config: Config = serde_json::from_slice(&std::fs::read(path)?)?;
    ensure!(
        config.agent.address.ip().is_loopback(),
        "health publisher requires a local Isochrone agent"
    );
    ensure!(config.state_dir.is_absolute(), "state_dir must be absolute");
    let bus = Client::connect(config.bus)?;
    loop {
        let Response::Routes { node_id, routes } = call(&config.agent, Request::Routes)? else {
            bail!("Isochrone agent did not return its route catalog");
        };
        ensure!(
            node_id == config.agent.node_id,
            "Isochrone node identity mismatch"
        );
        ensure!(routes.len() <= 256, "route catalog exceeds bound");
        let now = unibus::envelope::now_ms();
        for route in routes {
            ensure!(
                !route.route_id.is_empty()
                    && route.route_id.len() <= 128
                    && route
                        .route_id
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || b"-_.".contains(&byte)),
                "invalid route identity"
            );
            let generation = route
                .process_id
                .map(|pid| format!("process-{pid}"))
                .unwrap_or_else(|| "inactive".into());
            let health = if route.role == RouteRole::Receiver {
                read_health(
                    config
                        .state_dir
                        .join(format!("{}.receiver.json", route.route_id)),
                )?
            } else {
                None
            };
            // Preserve the receiver's capture timestamp; polling must not
            // rejuvenate an old health document.
            let fresh_health = health.as_ref().filter(|health| {
                health.observed_at_unix_ms <= now && now - health.observed_at_unix_ms <= 3000
            });
            let body = serde_json::json!({
                "node_id": node_id, "route_id": route.route_id, "endpoint_id": route.endpoint_id,
                "generation": generation, "role": route.role, "phase": route.phase,
                "observed_at_unix_ms": now, "valid_until_unix_ms": now + 3000,
                "receiver_health": fresh_health,
            });
            let mut envelope = bus.envelope(
                Some(&config.destination),
                Message::new("media.audio.isochrone.status.v1", body),
            )?;
            envelope.ttl_ms = Some(3000);
            bus.send(&envelope, &[])?;
            if route.phase != RoutePhase::Running {
                continue;
            }
            let projection = IsochroneRtpStream {
                stream_id: format!("audio/{node_id}/{}", route.route_id),
                source_resource: format!("audio/{node_id}/{}", route.endpoint_id),
                zone: config.zone.clone(),
                endpoint_reference: format!("isochrone/{node_id}/{}", route.route_id),
                generation,
                producer: "unibus-isochrone".into(),
                encoding: IsochroneEncoding::L24,
                // Agent send/receive currently select this exact baseline.
                sample_rate_hz: 48000,
                channels: 2,
                frames_per_packet: 48,
                observed_at_unix_ms: now,
                valid_until_unix_ms: now + 3000,
            }
            .project_v2()?;
            for (kind, value) in [
                (
                    STREAM_DESCRIPTOR_V2_KIND,
                    serde_json::to_value(projection.descriptor)?,
                ),
                (
                    ENDPOINT_ANNOUNCEMENT_V2_KIND,
                    serde_json::to_value(projection.endpoints)?,
                ),
            ] {
                let mut envelope =
                    bus.envelope(Some(&config.destination), Message::new(kind, value))?;
                envelope.ttl_ms = Some(3000);
                bus.send(&envelope, &[])?;
            }
        }
        std::thread::sleep(Duration::from_secs(1));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn missing_health_stays_unknown() {
        let path = std::env::temp_dir().join(format!(
            "unibus-isochrone-health-{}.json",
            std::process::id()
        ));
        assert!(read_health(path).unwrap().is_none());
    }

    #[test]
    fn loss_and_restart_use_the_real_isochrone_receiver() {
        use isochrone::{Packetizer, Receiver};
        let format = isochrone_core::StreamFormat::aes67_48k_stereo();
        let mut source = Packetizer::new(format, 7);
        let mut receiver = Receiver::new(format, Duration::from_millis(1));
        let samples = vec![0.25; 96];
        receiver.ingest(&source.packet(&samples).unwrap());
        source.skip(1);
        receiver.ingest(&source.packet(&samples).unwrap());
        receiver.render(144, Duration::from_millis(3));
        assert_eq!(receiver.metrics().sequence_gaps, 1);
        assert!(receiver.metrics().concealed_frames >= 48);
        let mut restarted = Packetizer::new(format, 8);
        receiver.ingest(&restarted.packet(&samples).unwrap());
        assert_eq!(receiver.metrics().stream_changes, 1);
        assert!(receiver.ready());
    }
}
