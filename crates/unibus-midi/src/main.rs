use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{TrySendError, sync_channel};

use anyhow::{Context, Result, bail, ensure};
use midir::{Ignore, MidiInput, MidiInputPort, MidiOutput, MidiOutputPort};
use serde::Deserialize;
use tracing::{debug, info, warn};
use unibus::Message;
use unibus::client::{Client as UnibusClient, ClientConfig};
use unibus_midi::{
    MIDI_INPUT_CAPABILITY, MIDI_OUTPUT_CAPABILITY, MIDI_PACKET_CONTRACT, MIDI_SCHEMA_VERSION,
    MidiPacket, valid_identifier,
};

const DEFAULT_QUEUE_CAPACITY: usize = 1024;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct BridgeConfig {
    bus: ClientConfig,
    device: DeviceConfig,
    #[serde(default = "default_queue_capacity")]
    queue_capacity: usize,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case", tag = "direction", deny_unknown_fields)]
enum DeviceConfig {
    Input {
        port: String,
        #[serde(default)]
        additional_ports: Vec<String>,
        destination: String,
    },
    Output {
        port: String,
        #[serde(default)]
        channel_mode: OutputChannelMode,
    },
}

#[derive(Debug, Default, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum OutputChannelMode {
    #[default]
    Preserve,
    All,
}

fn default_queue_capacity() -> usize {
    DEFAULT_QUEUE_CAPACITY
}

impl BridgeConfig {
    fn read(path: &Path) -> Result<Self> {
        let bytes = std::fs::read(path)
            .with_context(|| format!("read MIDI bridge config {}", path.display()))?;
        let config = serde_json::from_slice::<Self>(&bytes)
            .with_context(|| format!("parse MIDI bridge config {}", path.display()))?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<()> {
        ensure!(self.queue_capacity > 0, "queue_capacity must be positive");
        ensure!(
            self.bus.schema_version == unibus::SCHEMA_VERSION_V2,
            "MIDI bridge requires Unibus schema_version 2"
        );
        let required_capability = match &self.device {
            DeviceConfig::Input {
                port,
                additional_ports,
                destination,
            } => {
                ensure!(!port.trim().is_empty(), "MIDI input port is empty");
                ensure!(
                    additional_ports.iter().all(|port| !port.trim().is_empty()),
                    "additional MIDI input port is empty"
                );
                ensure!(
                    valid_identifier(destination),
                    "invalid MIDI destination producer"
                );
                MIDI_INPUT_CAPABILITY
            }
            DeviceConfig::Output { port, .. } => {
                ensure!(!port.trim().is_empty(), "MIDI output port is empty");
                MIDI_OUTPUT_CAPABILITY
            }
        };
        ensure!(
            self.bus
                .capabilities
                .iter()
                .any(|capability| capability == required_capability),
            "bus capabilities must include {required_capability}"
        );
        Ok(())
    }
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let Some(argument) = std::env::args_os().nth(1) else {
        bail!("usage: unibus-midi <bridge.json> | --list");
    };
    if argument == "--list" {
        return list_ports();
    }
    run(BridgeConfig::read(Path::new(&argument))?)
}

fn run(config: BridgeConfig) -> Result<()> {
    match config.device {
        DeviceConfig::Input {
            port,
            additional_ports,
            destination,
        } => {
            let mut ports = vec![port];
            ports.extend(additional_ports);
            run_input(config.bus, &ports, destination, config.queue_capacity)
        }
        DeviceConfig::Output { port, channel_mode } => run_output(config.bus, &port, channel_mode),
    }
}

fn run_input(
    bus_config: ClientConfig,
    port_queries: &[String],
    destination: String,
    queue_capacity: usize,
) -> Result<()> {
    let producer_id = bus_config.producer_id.clone();
    let bus = Arc::new(UnibusClient::connect(bus_config)?);
    let (sender, receiver) = sync_channel::<(u64, Vec<u8>)>(queue_capacity);
    let sequence = Arc::new(AtomicU64::new(0));

    let writer_bus = bus.clone();
    let writer_origin = producer_id.clone();
    let writer = std::thread::Builder::new()
        .name("unibus-midi-writer".into())
        .spawn(move || -> Result<()> {
            while let Ok((timestamp, payload)) = receiver.recv() {
                let packet = MidiPacket {
                    schema_version: MIDI_SCHEMA_VERSION,
                    origin: writer_origin.clone(),
                    sequence: sequence.fetch_add(1, Ordering::Relaxed),
                    source_timestamp_micros: timestamp,
                };
                packet.validate(&payload)?;
                let mut envelope = writer_bus.envelope(
                    Some(&destination),
                    Message::new(MIDI_PACKET_CONTRACT, serde_json::to_value(&packet)?),
                )?;
                envelope.payload_bytes = payload.len();
                if let Some((channel, note, velocity)) = note_on(&payload) {
                    debug!(
                        sequence = packet.sequence,
                        channel, note, velocity, "MIDI note captured"
                    );
                }
                writer_bus.send(&envelope, &payload)?;
            }
            bail!("MIDI input channel closed")
        })?;

    let mut connections = Vec::with_capacity(port_queries.len());
    for (index, port_query) in port_queries.iter().enumerate() {
        let mut midi = MidiInput::new(&format!("unibus-midi-input-{index}"))?;
        midi.ignore(Ignore::None);
        let port = select_input_port(&midi, port_query)?;
        let port_name = midi.port_name(&port)?;
        let port_sender = sender.clone();
        let connection = midi
            .connect(
                &port,
                &format!("unibus-midi-capture-{index}"),
                move |timestamp, bytes, ()| match port_sender.try_send((timestamp, bytes.to_vec()))
                {
                    Ok(()) => {}
                    Err(TrySendError::Full(_)) => warn!("MIDI capture queue full; packet dropped"),
                    Err(TrySendError::Disconnected(_)) => {}
                },
                (),
            )
            .map_err(|error| anyhow::anyhow!("connect MIDI input {port_name}: {error}"))?;
        info!(port = %port_name, %producer_id, "MIDI input bridge started");
        connections.push(connection);
    }
    drop(sender);
    writer
        .join()
        .map_err(|_| anyhow::anyhow!("MIDI writer thread panicked"))?
}

fn run_output(
    bus_config: ClientConfig,
    port_query: &str,
    channel_mode: OutputChannelMode,
) -> Result<()> {
    let bus = UnibusClient::connect(bus_config)?;
    let midi = MidiOutput::new("unibus-midi-output")?;
    let port = select_output_port(&midi, port_query)?;
    let port_name = midi.port_name(&port)?;
    let mut connection = midi
        .connect(&port, "unibus-midi-playback")
        .map_err(|error| anyhow::anyhow!("connect MIDI output: {error}"))?;
    info!(port = %port_name, ?channel_mode, "MIDI output bridge started");
    loop {
        let Some((envelope, payload)) = bus.try_read()? else {
            continue;
        };
        if matches!(
            unibus::kinds::classify(&envelope.message.kind),
            unibus::kinds::Control::Reply
        ) {
            ensure!(
                envelope
                    .message
                    .body
                    .get("success")
                    .and_then(|value| value.as_bool())
                    == Some(true),
                "hub rejected MIDI publication: {:?}",
                envelope.message.body
            );
            continue;
        }
        if envelope.message.kind == unibus::kinds::NODE_STATE {
            continue;
        }
        if envelope.message.kind != MIDI_PACKET_CONTRACT {
            warn!(kind = %envelope.message.kind, "ignoring unexpected Unibus message");
            continue;
        }
        let packet: MidiPacket =
            serde_json::from_value(serde_json::Value::Object(envelope.message.body))?;
        packet.validate(&payload)?;
        if let Some((channel, note, velocity)) = note_on(&payload) {
            debug!(
                sequence = packet.sequence,
                channel,
                note,
                velocity,
                transport_ms =
                    unibus::envelope::now_ms().saturating_sub(envelope.issued_at_unix_ms),
                "MIDI note delivered"
            );
        }
        send_midi(&mut connection, &payload, channel_mode)?;
    }
}

fn send_midi(
    connection: &mut midir::MidiOutputConnection,
    payload: &[u8],
    channel_mode: OutputChannelMode,
) -> Result<()> {
    let messages = expanded_messages(payload, channel_mode);
    ensure!(!messages.is_empty(), "empty MIDI payload");
    for message in messages {
        connection.send(&message)?;
    }
    Ok(())
}

fn expanded_messages(payload: &[u8], channel_mode: OutputChannelMode) -> Vec<Vec<u8>> {
    let Some(status) = payload.first().copied() else {
        return Vec::new();
    };
    if channel_mode != OutputChannelMode::All || !(0x80..=0xef).contains(&status) {
        return vec![payload.to_vec()];
    }
    let message_kind = status & 0xf0;
    (0..16)
        .map(|channel| {
            let mut message = payload.to_vec();
            message[0] = message_kind | channel;
            message
        })
        .collect()
}

fn note_on(payload: &[u8]) -> Option<(u8, u8, u8)> {
    let [status, note, velocity, ..] = payload else {
        return None;
    };
    (*status & 0xf0 == 0x90 && *velocity > 0).then_some((*status & 0x0f, *note, *velocity))
}

fn list_ports() -> Result<()> {
    let input = MidiInput::new("unibus-midi-list-input")?;
    println!("MIDI inputs:");
    for port in input.ports() {
        println!("  {}", input.port_name(&port)?);
    }
    let output = MidiOutput::new("unibus-midi-list-output")?;
    println!("MIDI outputs:");
    for port in output.ports() {
        println!("  {}", output.port_name(&port)?);
    }
    Ok(())
}

fn select_input_port(midi: &MidiInput, query: &str) -> Result<MidiInputPort> {
    select_unique(midi.ports(), query, |port| midi.port_name(port))
}

fn select_output_port(midi: &MidiOutput, query: &str) -> Result<MidiOutputPort> {
    select_unique(midi.ports(), query, |port| midi.port_name(port))
}

fn select_unique<T, F>(ports: Vec<T>, query: &str, mut name: F) -> Result<T>
where
    F: FnMut(&T) -> Result<String, midir::PortInfoError>,
{
    let query_lower = query.to_lowercase();
    let mut matches = Vec::new();
    let mut available = Vec::new();
    for port in ports {
        let port_name = name(&port)?;
        if query == "*" || port_name.to_lowercase().contains(&query_lower) {
            matches.push((port_name.clone(), port));
        }
        available.push(port_name);
    }
    match matches.len() {
        1 => Ok(matches.pop().unwrap().1),
        0 => bail!(
            "no MIDI port contains {query:?}; available ports: {}",
            available.join(", ")
        ),
        _ => bail!(
            "MIDI port query {query:?} is ambiguous: {}",
            matches
                .iter()
                .map(|(name, _)| name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(json: &str) -> BridgeConfig {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn input_config_requires_destination_and_capability() {
        let config = config(
            r#"{
                "bus":{"installation_id":"home","producer_id":"mac-midi","hub_endpoint":"127.0.0.1:18790","token_env":"UNIBUS_MIDI_TOKEN","zones":["studio"],"capabilities":["midi.input"],"schema_version":2},
                "device":{"direction":"input","port":"Keystation","destination":"pi-midi"}
            }"#,
        );
        config.validate().unwrap();
    }

    #[test]
    fn output_config_requires_output_capability() {
        let mut config = config(
            r#"{
                "bus":{"installation_id":"home","producer_id":"pi-midi","hub_endpoint":"127.0.0.1:18790","token_env":"UNIBUS_MIDI_TOKEN","zones":["studio"],"capabilities":["midi.input"],"schema_version":2},
                "device":{"direction":"output","port":"Scarlett"}
            }"#,
        );
        assert!(config.validate().is_err());
        config.bus.capabilities = vec![MIDI_OUTPUT_CAPABILITY.into()];
        config.validate().unwrap();
    }

    #[test]
    fn wildcard_requires_exactly_one_port() {
        let selected = select_unique(vec!["keyboard"], "*", |port| Ok((*port).to_owned()))
            .expect("single port should match wildcard");
        assert_eq!(selected, "keyboard");
        assert!(select_unique(vec!["one", "two"], "*", |port| Ok((*port).to_owned())).is_err());
    }

    #[test]
    fn note_on_excludes_note_off_and_zero_velocity() {
        assert_eq!(note_on(&[0x92, 60, 7]), Some((2, 60, 7)));
        assert_eq!(note_on(&[0x82, 60, 7]), None);
        assert_eq!(note_on(&[0x92, 60, 0]), None);
        assert_eq!(note_on(&[0x90, 60]), None);
    }

    #[test]
    fn all_channel_mode_fans_out_channel_voice_messages() {
        let messages = expanded_messages(&[0x92, 60, 100], OutputChannelMode::All);
        assert_eq!(messages.len(), 16);
        assert_eq!(messages[0], [0x90, 60, 100]);
        assert_eq!(messages[15], [0x9f, 60, 100]);

        let note_off = expanded_messages(&[0x82, 60, 0], OutputChannelMode::All);
        assert_eq!(note_off.len(), 16);
        assert_eq!(note_off[15], [0x8f, 60, 0]);
    }

    #[test]
    fn preserve_and_system_messages_are_not_duplicated() {
        assert_eq!(
            expanded_messages(&[0x95, 64, 80], OutputChannelMode::Preserve),
            vec![vec![0x95, 64, 80]]
        );
        assert_eq!(
            expanded_messages(&[0xf8], OutputChannelMode::All),
            vec![vec![0xf8]]
        );
    }
}
