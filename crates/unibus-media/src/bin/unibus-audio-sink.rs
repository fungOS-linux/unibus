use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::PathBuf;
use std::{os::unix::net::UnixDatagram, time::Duration};
use unibus::client::{Client, ClientConfig};
use unibus_media::{AUDIO_PLAYBACK_CHUNK_CONTRACT, AudioPlaybackChunk, PcmFormat};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    bus: ClientConfig,
    socket_path: PathBuf,
    #[serde(default = "default_stream")]
    stream_id: String,
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
    let mut expected_sequence = None;
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
        if chunk.sequence != 0
            && expected_sequence.is_some_and(|expected| expected != chunk.sequence)
        {
            eprintln!(
                "unibus-audio-sink: sequence gap expected={} received={}",
                expected_sequence.unwrap(),
                chunk.sequence
            );
        }
        expected_sequence = Some(chunk.sequence.wrapping_add(1));
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
                // This is a latency-first stream. If the local analyzer cannot
                // drain one 10 ms report in time, drop it rather than queue old
                // audio or tear down the Unibus route. macOS reports a full
                // Unix datagram receive buffer as ENOBUFS (55).
            }
            Err(error) => return Err(error).context("send local Hyperspace PCM"),
        }
    }
}
