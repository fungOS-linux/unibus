//! Typed L2 contracts for remote input and multiplayer session discovery.
//!
//! Gamepad reports are absolute state, not edge-triggered key events. This
//! makes a dropped or duplicated stream packet harmless and lets the receiver
//! release every control when a short lease expires.

use serde::{Deserialize, Serialize};
use std::fmt;

pub const GAMEPAD_STATE_CONTRACT: &str = "input.gamepad_state";
pub const DESKTOP_STATE_CONTRACT: &str = "input.desktop_state.v1";
pub const GAME_SESSION_CONTRACT: &str = "game.session.v1";
pub const INPUT_SCHEMA_VERSION: u32 = 1;
pub const MAX_PLAYER_SLOTS: u8 = 4;
pub const MAX_GAMEPAD_LEASE_MS: u64 = 250;
pub const MAX_DESKTOP_LEASE_MS: u64 = 500;
pub const MAX_DESKTOP_KEYS: usize = 32;
pub const OBSERVATION_PROTOCOL_VERSION: u32 = 1;
pub const MAX_OBSERVATION_RANGES: usize = 64;
pub const MAX_OBSERVATION_PAYLOAD_BYTES: usize = 60 * 1024;

pub mod button {
    pub const SOUTH: u32 = 1 << 0;
    pub const EAST: u32 = 1 << 1;
    pub const WEST: u32 = 1 << 2;
    pub const NORTH: u32 = 1 << 3;
    pub const LEFT_SHOULDER: u32 = 1 << 4;
    pub const RIGHT_SHOULDER: u32 = 1 << 5;
    pub const SELECT: u32 = 1 << 6;
    pub const START: u32 = 1 << 7;
    pub const MODE: u32 = 1 << 8;
    pub const LEFT_THUMB: u32 = 1 << 9;
    pub const RIGHT_THUMB: u32 = 1 << 10;
    pub const DPAD_UP: u32 = 1 << 11;
    pub const DPAD_DOWN: u32 = 1 << 12;
    pub const DPAD_LEFT: u32 = 1 << 13;
    pub const DPAD_RIGHT: u32 = 1 << 14;
    pub const ALL: u32 = (1 << 15) - 1;
}

/// A standard libretro memory region or the core's opaque serialized state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObservationRegion {
    SerializedState,
    SaveRam,
    SystemRam,
    VideoRam,
}

impl ObservationRegion {
    pub fn libretro_memory_id(self) -> Option<u32> {
        match self {
            Self::SerializedState => None,
            Self::SaveRam => Some(0),
            Self::SystemRam => Some(2),
            Self::VideoRam => Some(3),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObservationRange {
    pub offset: u64,
    pub length: u32,
}

/// Local request sent to a libretro session observation socket. Ranges are
/// concatenated in request order, letting adapters read only source-of-truth
/// bytes instead of copying an emulator's entire address space.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObservationRequest {
    pub protocol_version: u32,
    pub region: ObservationRegion,
    #[serde(default)]
    pub ranges: Vec<ObservationRange>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObservationResponse {
    pub protocol_version: u32,
    pub session_id: String,
    pub core_id: String,
    pub content_id: String,
    pub region: ObservationRegion,
    pub region_size: u64,
    pub ranges: Vec<ObservationRange>,
    pub observed_at_unix_ms: u64,
    pub payload_length: u32,
}

impl ObservationRequest {
    pub fn validate(&self) -> Result<(), ContractError> {
        if self.protocol_version != OBSERVATION_PROTOCOL_VERSION {
            return Err(ContractError("unsupported observation protocol".into()));
        }
        if self.ranges.len() > MAX_OBSERVATION_RANGES {
            return Err(ContractError("too many observation ranges".into()));
        }
        let total = self.ranges.iter().try_fold(0_usize, |total, range| {
            if range.length == 0 {
                return Err(ContractError("observation range is empty".into()));
            }
            total
                .checked_add(range.length as usize)
                .ok_or_else(|| ContractError("observation payload length overflow".into()))
        })?;
        if total > MAX_OBSERVATION_PAYLOAD_BYTES {
            return Err(ContractError("observation payload exceeds limit".into()));
        }
        Ok(())
    }
}

impl ObservationResponse {
    pub fn validate(&self, payload_len: usize) -> Result<(), ContractError> {
        if self.protocol_version != OBSERVATION_PROTOCOL_VERSION
            || !slug(&self.session_id)
            || self.core_id.len() != 64
            || self.content_id.len() != 64
            || !self.core_id.bytes().all(|byte| byte.is_ascii_hexdigit())
            || !self.content_id.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(ContractError("invalid observation identity".into()));
        }
        if self.payload_length as usize != payload_len
            || payload_len > MAX_OBSERVATION_PAYLOAD_BYTES
        {
            return Err(ContractError("invalid observation payload length".into()));
        }
        for range in &self.ranges {
            let end = range
                .offset
                .checked_add(u64::from(range.length))
                .ok_or_else(|| ContractError("observation range overflow".into()))?;
            if range.length == 0 || end > self.region_size {
                return Err(ContractError("observation range exceeds region".into()));
            }
        }
        let expected = self
            .ranges
            .iter()
            .map(|range| range.length as usize)
            .sum::<usize>();
        if !self.ranges.is_empty() && expected != payload_len {
            return Err(ContractError(
                "observation ranges do not match payload".into(),
            ));
        }
        Ok(())
    }
}

/// `[u32 BE header length][JSON header][binary payload]`.
pub fn encode_observation(
    response: &ObservationResponse,
    payload: &[u8],
) -> Result<Vec<u8>, ContractError> {
    response.validate(payload.len())?;
    let header = serde_json::to_vec(response)
        .map_err(|error| ContractError(format!("encode observation header: {error}")))?;
    let header_len: u32 = header
        .len()
        .try_into()
        .map_err(|_| ContractError("observation header exceeds u32".into()))?;
    let mut packet = Vec::with_capacity(4 + header.len() + payload.len());
    packet.extend_from_slice(&header_len.to_be_bytes());
    packet.extend_from_slice(&header);
    packet.extend_from_slice(payload);
    Ok(packet)
}

pub fn decode_observation(packet: &[u8]) -> Result<(ObservationResponse, &[u8]), ContractError> {
    let length_bytes: [u8; 4] = packet
        .get(..4)
        .ok_or_else(|| ContractError("truncated observation packet".into()))?
        .try_into()
        .unwrap();
    let header_len = u32::from_be_bytes(length_bytes) as usize;
    let header_end = 4_usize
        .checked_add(header_len)
        .ok_or_else(|| ContractError("observation header length overflow".into()))?;
    let header = packet
        .get(4..header_end)
        .ok_or_else(|| ContractError("truncated observation header".into()))?;
    let payload = packet
        .get(header_end..)
        .ok_or_else(|| ContractError("truncated observation payload".into()))?;
    let response: ObservationResponse = serde_json::from_slice(header)
        .map_err(|error| ContractError(format!("decode observation header: {error}")))?;
    response.validate(payload.len())?;
    Ok((response, payload))
}

/// Server end of a session's local observation socket.
///
/// The path is published by rename and reclaimed only while it still names this
/// socket. A session restart therefore never exposes a missing path to clients
/// that address the socket by name, and an exiting session can never unlink the
/// socket its successor just published.
#[cfg(unix)]
pub struct ObservationSocket {
    socket: std::os::unix::net::UnixDatagram,
    path: std::path::PathBuf,
    identity: (u64, u64),
}

#[cfg(unix)]
impl ObservationSocket {
    /// Bind a private name and rename it over `path`, replacing whatever
    /// session previously owned it.
    pub fn publish(path: &std::path::Path) -> anyhow::Result<Self> {
        use anyhow::{Context, ensure};
        use std::os::unix::fs::FileTypeExt;

        ensure!(path.is_absolute(), "observation socket path must be absolute");
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create observation socket dir {}", parent.display()))?;
        }
        if let Ok(metadata) = path.symlink_metadata() {
            ensure!(
                metadata.file_type().is_socket(),
                "refusing to replace non-socket observation path {}",
                path.display()
            );
        }

        let staging = staging_path(path);
        let _ = std::fs::remove_file(&staging);
        let socket = std::os::unix::net::UnixDatagram::bind(&staging)
            .with_context(|| format!("bind observation socket {}", staging.display()))?;
        socket
            .set_nonblocking(true)
            .context("set observation socket non-blocking")?;
        if let Err(error) = std::fs::rename(&staging, path) {
            let _ = std::fs::remove_file(&staging);
            return Err(error)
                .with_context(|| format!("publish observation socket {}", path.display()));
        }

        let identity = socket_identity(path)?;
        Ok(Self {
            socket,
            path: path.to_owned(),
            identity,
        })
    }

    pub fn socket(&self) -> &std::os::unix::net::UnixDatagram {
        &self.socket
    }

    pub fn path(&self) -> &std::path::Path {
        &self.path
    }
}

#[cfg(unix)]
impl Drop for ObservationSocket {
    fn drop(&mut self) {
        if socket_identity(&self.path).is_ok_and(|identity| identity == self.identity) {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

#[cfg(unix)]
fn staging_path(path: &std::path::Path) -> std::path::PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".{}.staging", std::process::id()));
    path.with_file_name(name)
}

/// `(device, inode)` of the socket currently published at `path`.
#[cfg(unix)]
fn socket_identity(path: &std::path::Path) -> anyhow::Result<(u64, u64)> {
    use anyhow::{Context, ensure};
    use std::os::unix::fs::{FileTypeExt, MetadataExt};

    let metadata = path
        .symlink_metadata()
        .with_context(|| format!("stat observation socket {}", path.display()))?;
    ensure!(
        metadata.file_type().is_socket(),
        "observation path {} is not a socket",
        path.display()
    );
    Ok((metadata.dev(), metadata.ino()))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct GamepadAxes {
    pub left_x: i16,
    pub left_y: i16,
    pub right_x: i16,
    pub right_y: i16,
    pub left_trigger: u16,
    pub right_trigger: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GamepadState {
    pub schema_version: u32,
    pub session_id: String,
    pub controller_id: String,
    pub player_slot: u8,
    pub sequence: u64,
    pub sampled_at_unix_ms: u64,
    pub valid_until_unix_ms: u64,
    pub buttons: u32,
    pub axes: GamepadAxes,
}

/// Absolute keyboard and pointer state for controlling a remote desktop.
///
/// `keys` contains USB HID keyboard usage IDs. Pointer coordinates span the
/// full target output (`0` is the left/top edge and `u16::MAX` the right/bottom
/// edge). Scroll is the only relative value and applies once per `sequence`.
/// A receiver must release all keys and buttons when the lease expires.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DesktopInputState {
    pub schema_version: u32,
    pub session_id: String,
    pub controller_id: String,
    pub target_id: String,
    pub sequence: u64,
    pub sampled_at_unix_ms: u64,
    pub valid_until_unix_ms: u64,
    #[serde(default)]
    pub keys: Vec<u16>,
    pub pointer_x: u16,
    pub pointer_y: u16,
    pub pointer_buttons: u8,
    #[serde(default)]
    pub scroll_x: i16,
    #[serde(default)]
    pub scroll_y: i16,
}

pub mod pointer_button {
    pub const LEFT: u8 = 1 << 0;
    pub const RIGHT: u8 = 1 << 1;
    pub const MIDDLE: u8 = 1 << 2;
    pub const BACK: u8 = 1 << 3;
    pub const FORWARD: u8 = 1 << 4;
    pub const ALL: u8 = LEFT | RIGHT | MIDDLE | BACK | FORWARD;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GameSessionRole {
    Host,
    Client,
    Spectator,
}

/// Control-plane discovery for a RetroArch-compatible session. Gameplay video
/// and netplay bytes stay on negotiated media/data planes, never in this body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GameSession {
    pub schema_version: u32,
    pub session_id: String,
    pub host_node_id: String,
    pub core_id: String,
    pub content_id: String,
    pub role: GameSessionRole,
    pub player_slots: u8,
    pub netplay_endpoint: Option<String>,
    pub announced_at_unix_ms: u64,
    pub valid_until_unix_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContractError(pub String);

impl fmt::Display for ContractError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for ContractError {}

impl GamepadState {
    pub fn validate(&self) -> Result<(), ContractError> {
        if self.schema_version != INPUT_SCHEMA_VERSION
            || !slug(&self.session_id)
            || !slug(&self.controller_id)
            || !(1..=MAX_PLAYER_SLOTS).contains(&self.player_slot)
        {
            return Err(ContractError(
                "invalid gamepad identity or player slot".into(),
            ));
        }
        if self.buttons & !button::ALL != 0 {
            return Err(ContractError(
                "gamepad report contains unknown buttons".into(),
            ));
        }
        let lease = self
            .valid_until_unix_ms
            .checked_sub(self.sampled_at_unix_ms)
            .ok_or_else(|| ContractError("gamepad report expires before it was sampled".into()))?;
        if lease == 0 || lease > MAX_GAMEPAD_LEASE_MS {
            return Err(ContractError(format!(
                "gamepad lease must be 1..={MAX_GAMEPAD_LEASE_MS} ms"
            )));
        }
        Ok(())
    }
}

impl DesktopInputState {
    pub fn validate(&self) -> Result<(), ContractError> {
        if self.schema_version != INPUT_SCHEMA_VERSION
            || !slug(&self.session_id)
            || !slug(&self.controller_id)
            || !slug(&self.target_id)
        {
            return Err(ContractError("invalid desktop input identity".into()));
        }
        if self.keys.len() > MAX_DESKTOP_KEYS
            || self.keys.windows(2).any(|pair| pair[0] >= pair[1])
            || self.keys.contains(&0)
        {
            return Err(ContractError(format!(
                "desktop keys must be unique, sorted, nonzero HID usages (at most {MAX_DESKTOP_KEYS})"
            )));
        }
        if self.pointer_buttons & !pointer_button::ALL != 0 {
            return Err(ContractError(
                "desktop report contains unknown pointer buttons".into(),
            ));
        }
        let lease = self
            .valid_until_unix_ms
            .checked_sub(self.sampled_at_unix_ms)
            .ok_or_else(|| ContractError("desktop report expires before it was sampled".into()))?;
        if lease == 0 || lease > MAX_DESKTOP_LEASE_MS {
            return Err(ContractError(format!(
                "desktop input lease must be 1..={MAX_DESKTOP_LEASE_MS} ms"
            )));
        }
        Ok(())
    }
}

impl GameSession {
    pub fn validate(&self) -> Result<(), ContractError> {
        if self.schema_version != INPUT_SCHEMA_VERSION
            || !slug(&self.session_id)
            || !slug(&self.host_node_id)
            || !slug(&self.core_id)
            || !content_id(&self.content_id)
            || !(1..=MAX_PLAYER_SLOTS).contains(&self.player_slots)
        {
            return Err(ContractError("invalid game session identity".into()));
        }
        if let Some(endpoint) = &self.netplay_endpoint {
            if endpoint.len() > 512
                || !(endpoint.starts_with("tcp://") || endpoint.starts_with("quic://"))
            {
                return Err(ContractError("unsupported netplay endpoint".into()));
            }
        }
        if self.valid_until_unix_ms <= self.announced_at_unix_ms
            || self.valid_until_unix_ms - self.announced_at_unix_ms > 30_000
        {
            return Err(ContractError(
                "game session announcement must expire within 30s".into(),
            ));
        }
        Ok(())
    }
}

fn slug(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

fn content_id(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report() -> GamepadState {
        GamepadState {
            schema_version: INPUT_SCHEMA_VERSION,
            session_id: "mario-night".into(),
            controller_id: "avery-phone".into(),
            player_slot: 1,
            sequence: 42,
            sampled_at_unix_ms: 1_000,
            valid_until_unix_ms: 1_100,
            buttons: button::SOUTH | button::DPAD_RIGHT,
            axes: GamepadAxes::default(),
        }
    }

    #[test]
    fn absolute_report_round_trips() {
        let report = report();
        report.validate().unwrap();
        let wire = serde_json::to_string(&report).unwrap();
        assert_eq!(serde_json::from_str::<GamepadState>(&wire).unwrap(), report);
    }

    #[test]
    fn report_rejects_unknown_buttons_and_long_leases() {
        let mut report = report();
        report.buttons = 1 << 31;
        assert!(report.validate().is_err());
        report.buttons = 0;
        report.valid_until_unix_ms = report.sampled_at_unix_ms + MAX_GAMEPAD_LEASE_MS + 1;
        assert!(report.validate().is_err());
    }

    fn desktop_report() -> DesktopInputState {
        DesktopInputState {
            schema_version: INPUT_SCHEMA_VERSION,
            session_id: "living-room-control".into(),
            controller_id: "ajm-macbook".into(),
            target_id: "home-pi-tv".into(),
            sequence: 7,
            sampled_at_unix_ms: 1_000,
            valid_until_unix_ms: 1_250,
            keys: vec![0x04, 0xE3],
            pointer_x: 32_767,
            pointer_y: 12_345,
            pointer_buttons: pointer_button::LEFT,
            scroll_x: 0,
            scroll_y: -1,
        }
    }

    #[test]
    fn desktop_report_round_trips() {
        let report = desktop_report();
        report.validate().unwrap();
        let wire = serde_json::to_string(&report).unwrap();
        assert_eq!(
            serde_json::from_str::<DesktopInputState>(&wire).unwrap(),
            report
        );
    }

    #[test]
    fn desktop_report_rejects_ambiguous_or_unbounded_state() {
        let mut report = desktop_report();
        report.keys = vec![0xE3, 0x04];
        assert!(report.validate().is_err());
        report.keys = vec![0x04];
        report.pointer_buttons = 1 << 7;
        assert!(report.validate().is_err());
        report.pointer_buttons = 0;
        report.valid_until_unix_ms = report.sampled_at_unix_ms + MAX_DESKTOP_LEASE_MS + 1;
        assert!(report.validate().is_err());
    }

    #[test]
    fn session_is_content_addressed_and_bounded() {
        let session = GameSession {
            schema_version: INPUT_SCHEMA_VERSION,
            session_id: "mario-night".into(),
            host_node_id: "home-pi".into(),
            core_id: "snes9x".into(),
            content_id: "a".repeat(64),
            role: GameSessionRole::Host,
            player_slots: 2,
            netplay_endpoint: Some("tcp://home-pi:55435".into()),
            announced_at_unix_ms: 1_000,
            valid_until_unix_ms: 16_000,
        };
        session.validate().unwrap();
    }

    #[test]
    fn ranged_observation_round_trips_with_identity() {
        let payload = [1, 2, 3, 4, 5];
        let response = ObservationResponse {
            protocol_version: OBSERVATION_PROTOCOL_VERSION,
            session_id: "tetris-run".into(),
            core_id: "a".repeat(64),
            content_id: "b".repeat(64),
            region: ObservationRegion::SystemRam,
            region_size: 8192,
            ranges: vec![
                ObservationRange {
                    offset: 10,
                    length: 2,
                },
                ObservationRange {
                    offset: 100,
                    length: 3,
                },
            ],
            observed_at_unix_ms: 123,
            payload_length: payload.len() as u32,
        };
        let packet = encode_observation(&response, &payload).unwrap();
        let (decoded, decoded_payload) = decode_observation(&packet).unwrap();
        assert_eq!(decoded, response);
        assert_eq!(decoded_payload, payload);
    }

    #[cfg(unix)]
    fn send(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<usize> {
        std::os::unix::net::UnixDatagram::unbound()?.send_to(bytes, path)
    }

    #[cfg(unix)]
    fn recv(socket: &ObservationSocket) -> std::io::Result<Vec<u8>> {
        let mut buffer = [0_u8; 64];
        let (len, _) = socket.socket().recv_from(&mut buffer)?;
        Ok(buffer[..len].to_vec())
    }

    #[cfg(unix)]
    #[test]
    fn published_socket_receives_by_name_and_is_reclaimed_on_drop() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("observation.sock");

        use std::os::unix::fs::FileTypeExt;

        let socket = ObservationSocket::publish(&path).unwrap();
        assert!(path.symlink_metadata().unwrap().file_type().is_socket());
        send(&path, b"ping").unwrap();
        assert_eq!(recv(&socket).unwrap(), b"ping");

        drop(socket);
        assert!(!path.exists(), "owner must reclaim its own socket path");
    }

    /// The restart race: a session that exits after its successor published the
    /// same path must not unlink the successor's socket, or the surviving
    /// emulator becomes permanently unreachable to every agent.
    #[cfg(unix)]
    #[test]
    fn exiting_session_does_not_reclaim_a_successors_socket() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("observation.sock");

        let outgoing = ObservationSocket::publish(&path).unwrap();
        let incoming = ObservationSocket::publish(&path).unwrap();

        drop(outgoing);
        assert!(path.exists(), "successor's socket path was unlinked");

        send(&path, b"live").unwrap();
        assert_eq!(recv(&incoming).unwrap(), b"live");
    }

    /// Publishing by rename leaves no window in which the path is absent, so a
    /// client addressing the socket by name never observes ENOENT mid-restart.
    #[cfg(unix)]
    #[test]
    fn publish_replaces_a_stale_socket_without_unpublishing_the_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("observation.sock");

        let stale = ObservationSocket::publish(&path).unwrap();
        let stale_identity = stale.identity;
        std::mem::forget(stale); // Model a session killed without running Drop.

        let fresh = ObservationSocket::publish(&path).unwrap();
        assert!(path.exists());
        assert_ne!(fresh.identity, stale_identity);
        send(&path, b"fresh").unwrap();
        assert_eq!(recv(&fresh).unwrap(), b"fresh");
    }

    #[cfg(unix)]
    #[test]
    fn publish_refuses_a_non_socket_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("not-a-socket");
        std::fs::write(&path, b"precious").unwrap();

        assert!(ObservationSocket::publish(&path).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"precious");
    }

    #[test]
    fn observation_rejects_oversize_and_out_of_bounds_ranges() {
        let request = ObservationRequest {
            protocol_version: OBSERVATION_PROTOCOL_VERSION,
            region: ObservationRegion::SystemRam,
            ranges: vec![ObservationRange {
                offset: 0,
                length: MAX_OBSERVATION_PAYLOAD_BYTES as u32 + 1,
            }],
        };
        assert!(request.validate().is_err());

        let response = ObservationResponse {
            protocol_version: OBSERVATION_PROTOCOL_VERSION,
            session_id: "tetris-run".into(),
            core_id: "a".repeat(64),
            content_id: "b".repeat(64),
            region: ObservationRegion::SystemRam,
            region_size: 8,
            ranges: vec![ObservationRange {
                offset: 7,
                length: 2,
            }],
            observed_at_unix_ms: 123,
            payload_length: 2,
        };
        assert!(response.validate(2).is_err());
    }
}
