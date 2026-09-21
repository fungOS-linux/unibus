//! Local Unix-datagram ingress for phone, HID, and agent controller adapters.
//! The ingress owns identity and lease timestamps; callers can only supply
//! bounded controller state.

#[cfg(not(unix))]
fn main() {
    eprintln!("unibus-gamepad-socket requires a Unix domain socket");
    std::process::exit(1);
}

#[cfg(unix)]
fn main() -> anyhow::Result<()> {
    unix::run()
}

#[cfg(unix)]
mod unix {
    use anyhow::{Context, Result, ensure};
    use serde::Deserialize;
    use std::{
        fs::Permissions,
        os::{unix::fs::PermissionsExt, unix::net::UnixDatagram},
        path::PathBuf,
        time::{SystemTime, UNIX_EPOCH},
    };
    use unibus::{
        Message,
        client::{Client, ClientConfig},
    };
    use unibus_input::{
        GAMEPAD_STATE_CONTRACT, GamepadAxes, GamepadState, INPUT_SCHEMA_VERSION,
        MAX_GAMEPAD_LEASE_MS,
    };

    #[derive(Debug, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Config {
        bus: ClientConfig,
        socket_path: PathBuf,
        session_id: String,
        controller_id: String,
        player_slot: u8,
        #[serde(default = "default_lease_ms")]
        lease_ms: u64,
    }

    fn default_lease_ms() -> u64 {
        120
    }

    #[derive(Debug, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct LocalReport {
        sequence: u64,
        buttons: u32,
        #[serde(default)]
        axes: GamepadAxes,
    }

    struct SocketGuard(PathBuf);
    impl Drop for SocketGuard {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    pub fn run() -> Result<()> {
        let config_path = std::env::args()
            .nth(1)
            .context("usage: unibus-gamepad-socket <config.json>")?;
        let config: Config = serde_json::from_slice(
            &std::fs::read(&config_path).with_context(|| format!("read {config_path}"))?,
        )?;
        ensure!(
            (1..=MAX_GAMEPAD_LEASE_MS).contains(&config.lease_ms),
            "lease_ms must be 1..={MAX_GAMEPAD_LEASE_MS}"
        );
        ensure!(
            config.socket_path.is_absolute(),
            "socket_path must be absolute"
        );
        ensure!(
            !config.socket_path.exists(),
            "refusing to replace existing socket path {}",
            config.socket_path.display()
        );
        if let Some(parent) = config.socket_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let socket = UnixDatagram::bind(&config.socket_path)
            .with_context(|| format!("bind {}", config.socket_path.display()))?;
        std::fs::set_permissions(&config.socket_path, Permissions::from_mode(0o600))?;
        let _guard = SocketGuard(config.socket_path.clone());
        let bus_config = config.bus;
        let mut client: Option<Client> = None;
        let mut bytes = [0_u8; 8192];
        loop {
            let count = socket.recv(&mut bytes)?;
            let report: LocalReport =
                serde_json::from_slice(&bytes[..count]).context("decode local gamepad report")?;
            let now = now_ms()?;
            let state = GamepadState {
                schema_version: INPUT_SCHEMA_VERSION,
                session_id: config.session_id.clone(),
                controller_id: config.controller_id.clone(),
                player_slot: config.player_slot,
                sequence: report.sequence,
                sampled_at_unix_ms: now,
                valid_until_unix_ms: now + config.lease_ms,
                buttons: report.buttons,
                axes: report.axes,
            };
            state.validate().map_err(anyhow::Error::msg)?;
            let body = serde_json::to_value(&state)?;
            loop {
                if client.is_none() {
                    match Client::connect(bus_config.clone()) {
                        Ok(value) => client = Some(value),
                        Err(error) => {
                            eprintln!("gamepad bus reconnect: {error:#}");
                            std::thread::sleep(std::time::Duration::from_millis(500));
                            continue;
                        }
                    }
                }
                let result = (|| -> Result<()> {
                    let connected = client.as_ref().context("missing gamepad bus client")?;
                    let envelope = connected.envelope(
                        None,
                        Message::new(GAMEPAD_STATE_CONTRACT, body.clone()),
                    )?;
                    connected.send(&envelope, &[])
                })();
                match result {
                    Ok(()) => break,
                    Err(error) => {
                        eprintln!("gamepad bus disconnected: {error:#}");
                        client = None;
                        std::thread::sleep(std::time::Duration::from_millis(250));
                    }
                }
            }
        }
    }

    fn now_ms() -> Result<u64> {
        Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis() as u64)
    }
}
