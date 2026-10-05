//! Small synchronous Unibus client shared by edge processes.
//!
//! A single writer lock keeps registration, heartbeats, and payload frames
//! from interleaving. Readers time out so services can perform housekeeping.

use std::net::TcpStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

use crate::{Envelope, FrameError, Message, SCHEMA_VERSION_V1, framing, kinds};

const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(3);
const DEFAULT_READ_TIMEOUT_MS: u64 = 10_000;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClientConfig {
    pub installation_id: String,
    pub producer_id: String,
    pub hub_endpoint: String,
    #[serde(default)]
    pub token_file: Option<PathBuf>,
    #[serde(default)]
    pub token_env: Option<String>,
    pub zones: Vec<String>,
    #[serde(default)]
    pub capabilities: Vec<String>,
    #[serde(default)]
    pub screen_anchors: Vec<String>,
    #[serde(default = "default_schema")]
    pub schema_version: u32,
    #[serde(default = "default_read_timeout_ms")]
    pub read_timeout_ms: u64,
}

fn default_schema() -> u32 {
    SCHEMA_VERSION_V1
}

fn default_read_timeout_ms() -> u64 {
    DEFAULT_READ_TIMEOUT_MS
}

impl ClientConfig {
    pub fn token(&self) -> Result<String> {
        let token = match (&self.token_file, &self.token_env) {
            (Some(path), None) => std::fs::read_to_string(path)
                .with_context(|| format!("read hub token {}", path.display()))?,
            (None, Some(name)) => {
                ensure!(valid_env_name(name), "invalid hub token environment name");
                std::env::var(name).with_context(|| format!("read hub token environment {name}"))?
            }
            _ => anyhow::bail!("exactly one hub token source is required"),
        };
        let token = token.trim().to_owned();
        ensure!(!token.is_empty(), "hub token is empty");
        Ok(token)
    }
}

fn valid_env_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    matches!(bytes.next(), Some(b'A'..=b'Z' | b'_'))
        && bytes.all(|byte| matches!(byte, b'A'..=b'Z' | b'0'..=b'9' | b'_'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_source_is_explicit_and_environment_names_are_bounded() {
        assert!(valid_env_name("UNIBUS_SURF_TOKEN"));
        assert!(!valid_env_name("unibus-token"));
        let missing: ClientConfig = serde_json::from_str(
            r#"{"installation_id":"home","producer_id":"surf","hub_endpoint":"127.0.0.1:1","zones":["default"]}"#,
        )
        .unwrap();
        assert!(missing.token().unwrap_err().to_string().contains("exactly one"));
    }
}

pub struct Client {
    config: ClientConfig,
    reader: Mutex<TcpStream>,
    writer: Arc<Mutex<TcpStream>>,
    stop: Arc<AtomicBool>,
}

impl Client {
    pub fn connect(config: ClientConfig) -> Result<Self> {
        ensure!(
            !config.zones.is_empty(),
            "client must serve at least one zone"
        );
        let addr: std::net::SocketAddr = config
            .hub_endpoint
            .parse()
            .context("hub_endpoint must be ip:port")?;
        let stream = TcpStream::connect_timeout(&addr, Duration::from_secs(5))
            .with_context(|| format!("connect hub at {addr}"))?;
        stream.set_nodelay(true)?;
        ensure!(
            (1..=60_000).contains(&config.read_timeout_ms),
            "read_timeout_ms must be 1..=60000"
        );
        stream.set_read_timeout(Some(Duration::from_millis(config.read_timeout_ms)))?;
        stream.set_write_timeout(Some(Duration::from_secs(5)))?;
        let token = config.token()?;
        let mut register = Envelope::new(
            &config.installation_id,
            &config.producer_id,
            None,
            &token,
            Message::new(
                kinds::V1_REGISTER,
                serde_json::json!({
                    "capabilities": config.capabilities,
                    "zones": config.zones,
                    "screen_anchors": config.screen_anchors,
                }),
            ),
        );
        register.schema_version = config.schema_version;
        let registration_request = register.request_id.clone();
        let writer = Arc::new(Mutex::new(stream.try_clone()?));
        framing::write_frame(&mut *writer.lock().unwrap(), &register, &[])?;
        let mut reader = stream.try_clone()?;
        loop {
            let (reply, _) = framing::read_frame(&mut reader)?;
            let is_registration_reply = reply.message.kind == kinds::V1_REPLY
                && reply
                    .message
                    .body
                    .get("request_id")
                    .and_then(|value| value.as_str())
                    == Some(registration_request.as_str());
            if !is_registration_reply {
                continue;
            }
            ensure!(
                reply
                    .message
                    .body
                    .get("success")
                    .and_then(|value| value.as_bool())
                    == Some(true),
                "hub rejected registration: {:?}",
                reply.message.body
            );
            break;
        }

        let stop = Arc::new(AtomicBool::new(false));
        {
            let installation = config.installation_id.clone();
            let producer = config.producer_id.clone();
            let schema = config.schema_version;
            let writer = writer.clone();
            let stop = stop.clone();
            thread::Builder::new()
                .name("unibus-client-heartbeat".into())
                .spawn(move || {
                    while !stop.load(Ordering::Acquire) {
                        thread::sleep(HEARTBEAT_INTERVAL);
                        if stop.load(Ordering::Acquire) {
                            break;
                        }
                        let mut beat = Envelope::new(
                            &installation,
                            &producer,
                            None,
                            &token,
                            Message::new(kinds::V1_HEARTBEAT, serde_json::Value::Null),
                        );
                        beat.schema_version = schema;
                        let Ok(mut guard) = writer.lock() else { break };
                        if framing::write_frame(&mut *guard, &beat, &[]).is_err() {
                            stop.store(true, Ordering::Release);
                        }
                    }
                })?;
        }
        Ok(Self {
            config,
            reader: Mutex::new(reader),
            writer,
            stop,
        })
    }

    pub fn envelope(&self, destination: Option<&str>, message: Message) -> Result<Envelope> {
        let mut envelope = Envelope::new(
            &self.config.installation_id,
            &self.config.producer_id,
            destination,
            &self.config.token()?,
            message,
        );
        envelope.schema_version = self.config.schema_version;
        Ok(envelope)
    }

    pub fn config(&self) -> &ClientConfig {
        &self.config
    }

    pub fn send(&self, envelope: &Envelope, payload: &[u8]) -> Result<()> {
        let mut envelope = envelope.clone();
        envelope.schema_version = self.config.schema_version;
        framing::write_frame(&mut *self.writer.lock().unwrap(), &envelope, payload)?;
        Ok(())
    }

    /// False after the heartbeat writer observes a broken transport. Edge
    /// processes should rebuild and re-register the whole client session.
    pub fn is_healthy(&self) -> bool {
        !self.stop.load(Ordering::Acquire)
    }

    pub fn try_read(&self) -> Result<Option<(Envelope, Vec<u8>)>> {
        ensure!(
            !self.stop.load(Ordering::Acquire),
            "heartbeat writer stopped"
        );
        match framing::read_frame(&mut *self.reader.lock().unwrap()) {
            Ok(frame) => Ok(Some(frame)),
            Err(FrameError::Io(error))
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) || matches!(error.raw_os_error(), Some(11 | 35)) =>
            {
                Ok(None)
            }
            Err(error) => Err(error.into()),
        }
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
    }
}
