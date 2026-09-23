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
    /// How long to wait before the first reconnect attempt after a failure,
    /// and the ceiling that backoff grows to. Bounded so a bus that comes
    /// back is picked up promptly rather than after an ever-lengthening wait.
    const RECONNECT_BACKOFF_MIN: Duration = Duration::from_millis(250);
    const RECONNECT_BACKOFF_MAX: Duration = Duration::from_secs(5);

    use anyhow::{Context, Result, ensure};
    use serde::Deserialize;
    use std::{
        fs::Permissions,
        os::{unix::fs::PermissionsExt, unix::net::UnixDatagram},
        path::PathBuf,
        time::{Duration, SystemTime, UNIX_EPOCH},
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
        let mut bytes = [0_u8; 8192];
        let mut undelivered = 0_u64;

        // Small on purpose. The queue exists to decouple draining from a
        // blocking publisher, not to buffer history: every entry past the
        // newest is a lease that has already expired.
        let (outbound, inbound) = std::sync::mpsc::sync_channel::<GamepadState>(64);
        let publisher_config = bus_config.clone();
        std::thread::Builder::new()
            .name("gamepad-bus-publisher".into())
            .spawn(move || publish_forever(&publisher_config, &inbound))
            .context("spawn the gamepad bus publisher")?;
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
            // Hand off and go straight back to draining. Publishing must
            // never happen on this thread: `Client::connect` blocks, and with
            // `read_timeout_ms` at 60s one stalled connect stops `recv` for a
            // minute. That is what filled the sender's datagram buffer and
            // made its sends fail with `ENOBUFS` -- which killed a speedy
            // agent outright and left this bridge 284,976 reconnects deep,
            // having delivered nothing for two days.
            //
            // A gamepad report is a lease: it carries `valid_until_unix_ms`
            // and expires `lease_ms` after sampling (120ms in the speedy
            // configs). If the publisher is behind, the queued report is
            // already worthless and the newest one supersedes it, so a full
            // queue drops rather than blocks.
            if outbound.try_send(state).is_err() {
                undelivered += 1;
                if undelivered.is_power_of_two() {
                    eprintln!(
                        "gamepad bus behind: {undelivered} report(s) dropped rather than \
                         stop draining the socket"
                    );
                }
            }
        }
    }

    /// Owns the bus connection and may block freely -- it is not on the path
    /// that drains the socket.
    fn publish_forever(bus_config: &ClientConfig, inbound: &std::sync::mpsc::Receiver<GamepadState>) {
        let mut client: Option<Client> = None;
        let mut backoff = RECONNECT_BACKOFF_MIN;
        while let Ok(state) = inbound.recv() {
            let Ok(body) = serde_json::to_value(&state) else {
                continue;
            };
            if client.is_none() {
                match Client::connect(bus_config.clone()) {
                    Ok(value) => {
                        client = Some(value);
                        backoff = RECONNECT_BACKOFF_MIN;
                    }
                    Err(error) => {
                        eprintln!("gamepad bus reconnect: {error:#}");
                        std::thread::sleep(backoff);
                        backoff = (backoff * 2).min(RECONNECT_BACKOFF_MAX);
                        continue;
                    }
                }
            }
            if let Some(connected) = client.as_ref() {
                let sent = connected
                    .envelope(None, Message::new(GAMEPAD_STATE_CONTRACT, body))
                    .and_then(|envelope| connected.send(&envelope, &[]));
                if let Err(error) = sent {
                    eprintln!("gamepad bus disconnected: {error:#}");
                    client = None;
                }
            }
        }
    }

    fn now_ms() -> Result<u64> {
        Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis() as u64)
    }
}
