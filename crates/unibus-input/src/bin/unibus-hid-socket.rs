//! Local, focus-scoped desktop HID ingress.
//!
//! Canvas and other UI hosts write absolute state to a mode-0600 Unix
//! datagram. This process owns controller identity, timestamps, leases, target
//! authorization, and Unibus publication.

#[cfg(not(unix))]
fn main() {
    eprintln!("unibus-hid-socket requires a Unix domain socket");
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
        collections::BTreeSet,
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
        DESKTOP_STATE_CONTRACT, DesktopInputState, INPUT_SCHEMA_VERSION, MAX_DESKTOP_LEASE_MS,
    };

    #[derive(Debug, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Config {
        bus: ClientConfig,
        socket_path: PathBuf,
        destination: String,
        session_id: String,
        controller_id: String,
        authorized_targets: BTreeSet<String>,
        #[serde(default = "default_lease_ms")]
        lease_ms: u64,
    }

    const fn default_lease_ms() -> u64 {
        250
    }

    #[derive(Debug, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct LocalReport {
        target_id: String,
        sequence: u64,
        #[serde(default)]
        keys: Vec<u16>,
        pointer_x: u16,
        pointer_y: u16,
        pointer_buttons: u8,
        #[serde(default)]
        scroll_x: i16,
        #[serde(default)]
        scroll_y: i16,
    }

    struct SocketGuard(PathBuf);
    impl Drop for SocketGuard {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    pub fn run() -> Result<()> {
        let path = std::env::args()
            .nth(1)
            .context("usage: unibus-hid-socket <config.json>")?;
        let config: Config =
            serde_json::from_slice(&std::fs::read(&path).with_context(|| format!("read {path}"))?)?;
        ensure!(
            (1..=MAX_DESKTOP_LEASE_MS).contains(&config.lease_ms),
            "invalid lease_ms"
        );
        ensure!(
            !config.authorized_targets.is_empty(),
            "authorized_targets is required"
        );
        ensure!(
            config.socket_path.is_absolute(),
            "socket_path must be absolute"
        );
        if config.socket_path.exists() {
            std::fs::remove_file(&config.socket_path)?;
        }
        if let Some(parent) = config.socket_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let socket = UnixDatagram::bind(&config.socket_path)
            .with_context(|| format!("bind {}", config.socket_path.display()))?;
        std::fs::set_permissions(&config.socket_path, Permissions::from_mode(0o600))?;
        let _guard = SocketGuard(config.socket_path.clone());
        let client = Client::connect(config.bus.clone())?;
        let mut bytes = [0_u8; 8192];
        loop {
            let count = socket.recv(&mut bytes)?;
            let report: LocalReport = serde_json::from_slice(&bytes[..count])
                .context("decode local desktop HID report")?;
            ensure!(
                config.authorized_targets.contains(&report.target_id),
                "unauthorized target_id"
            );
            let now = now_ms()?;
            let state = DesktopInputState {
                schema_version: INPUT_SCHEMA_VERSION,
                session_id: config.session_id.clone(),
                controller_id: config.controller_id.clone(),
                target_id: report.target_id,
                sequence: report.sequence,
                sampled_at_unix_ms: now,
                valid_until_unix_ms: now + config.lease_ms,
                keys: report.keys,
                pointer_x: report.pointer_x,
                pointer_y: report.pointer_y,
                pointer_buttons: report.pointer_buttons,
                scroll_x: report.scroll_x,
                scroll_y: report.scroll_y,
            };
            state.validate().map_err(anyhow::Error::msg)?;
            let envelope = client.envelope(
                Some(&config.destination),
                Message::new(DESKTOP_STATE_CONTRACT, serde_json::to_value(state)?),
            )?;
            client.send(&envelope, &[])?;
        }
    }

    fn now_ms() -> Result<u64> {
        Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis() as u64)
    }
}
