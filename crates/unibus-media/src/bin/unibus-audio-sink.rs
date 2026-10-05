use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::PathBuf;
use std::{os::unix::net::UnixDatagram, time::Duration};
use unibus::client::{Client, ClientConfig};
use unibus_media::audio_ingress::AudioIngress;
use unibus_media::{AUDIO_PLAYBACK_CHUNK_CONTRACT, AudioPlaybackChunk, PcmFormat};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    bus: ClientConfig,
    socket_path: PathBuf,
    #[serde(default = "default_stream")]
    stream_id: String,
    #[serde(default = "default_maximum_age")]
    maximum_age_ms: u64,
}

fn default_maximum_age() -> u64 {
    100
}

fn default_stream() -> String {
    "home-playback".into()
}

fn main() -> Result<()> {
    let path = std::env::args()
        .nth(1)
        .context("usage: unibus-audio-sink <config.json>")?;
    let config: Config =
        serde_json::from_slice(&std::fs::read(&path).with_context(|| format!("read {path}"))?)?;
    let audio = UnixDatagram::unbound()?;
    audio.set_write_timeout(Some(Duration::from_millis(20)))?;
    let client = Client::connect(config.bus)?;
    let mut ingress = AudioIngress::new(config.stream_id.clone(), config.maximum_age_ms)?;
    loop {
        let Some((envelope, payload)) = client.try_read()? else {
            continue;
        };
        if envelope.message.kind != AUDIO_PLAYBACK_CHUNK_CONTRACT {
            continue;
        }
        let chunk: AudioPlaybackChunk =
            serde_json::from_value(serde_json::Value::Object(envelope.message.body))?;
        chunk.validate(payload.len()).map_err(anyhow::Error::msg)?;
        if chunk.stream_id != config.stream_id || chunk.format != PcmFormat::S16Le {
            continue;
        }
        if !ingress.admit(&chunk, payload.len(), unibus::envelope::now_ms())? {
            continue;
        }
        match audio.send_to(&payload, &config.socket_path) {
            Ok(written) if written == payload.len() => {}
            Ok(written) => eprintln!(
                "unibus-audio-sink: short local datagram {written}/{}",
                payload.len()
            ),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound
                        | std::io::ErrorKind::ConnectionRefused
                        | std::io::ErrorKind::WouldBlock
                        | std::io::ErrorKind::TimedOut
                ) || error.raw_os_error() == Some(55) =>
            {
                ingress.local_drop();
                let dropped = ingress.metrics().local_drops;
                if dropped.is_power_of_two() {
                    eprintln!("unibus-audio-sink: local drops={dropped}");
                }
                // This is a latency-first stream. If the local analyzer cannot
                // drain one 10 ms report in time, drop it rather than queue old
                // audio or tear down the Unibus route. macOS reports a full
                // Unix datagram receive buffer as ENOBUFS (55).
            }
            Err(error) => return Err(error).context("send local Hyperspace PCM"),
        }
    }
}
