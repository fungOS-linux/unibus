//! QEMU integration boundary for Unibus.
//!
//! Video and input share guest identity but not authority: RFB is view-only,
//! while the input path accepts only the bounded `input.desktop_state.v1`
//! contract and translates it to QMP `input-send-event`. Arbitrary QMP is
//! intentionally not exposed on the bus.

use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    io::{BufRead, BufReader, Write},
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use unibus::{
    Message,
    client::{Client, ClientConfig},
};
use unibus_input::{DESKTOP_STATE_CONTRACT, DesktopInputState, pointer_button};
use unibus_media::{
    ENDPOINT_ANNOUNCEMENT_V2_KIND, MEDIA_SCHEMA_VERSION, QemuRfbH264UdpRuntime,
    STREAM_DESCRIPTOR_V2_KIND, TransportRole, VIDEO_STREAM_CONTRACT, VideoCodec, VideoStream,
    VideoTransport,
};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    bus: ClientConfig,
    destination: String,
    gst_launch: PathBuf,
    qmp_socket: PathBuf,
    vnc_host: String,
    vnc_port: u16,
    target_id: String,
    stream_id: String,
    display_name: String,
    media_targets: Vec<String>,
    advertised_endpoint: String,
    width: u32,
    height: u32,
    fps: u32,
    #[serde(default = "default_bitrate_kbps")]
    bitrate_kbps: u32,
    #[serde(default = "default_ttl_ms")]
    ttl_ms: u64,
}

const fn default_bitrate_kbps() -> u32 {
    8_000
}
const fn default_ttl_ms() -> u64 {
    30_000
}

impl Config {
    fn read(path: &Path) -> Result<Self> {
        let config: Self = serde_json::from_slice(
            &std::fs::read(path).with_context(|| format!("read {}", path.display()))?,
        )
        .with_context(|| format!("parse {}", path.display()))?;
        ensure!(config.gst_launch.is_file(), "gst-launch not found");
        ensure!(config.qmp_socket.exists(), "QMP socket not found");
        ensure!(!config.vnc_host.is_empty(), "VNC host is required");
        ensure!(!config.target_id.is_empty(), "target_id is required");
        ensure!(
            !config.media_targets.is_empty(),
            "media_targets is required"
        );
        ensure!((16..=7680).contains(&config.width), "invalid width");
        ensure!((16..=4320).contains(&config.height), "invalid height");
        ensure!((1..=60).contains(&config.fps), "fps must be 1..=60");
        ensure!(
            (250..=200_000).contains(&config.bitrate_kbps),
            "invalid bitrate_kbps"
        );
        ensure!((5_000..=300_000).contains(&config.ttl_ms), "invalid ttl_ms");
        for target in &config.media_targets {
            parse_host_port(target)?;
        }
        Ok(config)
    }
}

fn main() -> Result<()> {
    let path = std::env::args()
        .nth(1)
        .context("usage: unibus-qemu <config.json>")?;
    let config = Config::read(Path::new(&path))?;
    let mut qmp = Qmp::connect(&config.qmp_socket)?;
    qmp.verify_vnc(config.vnc_port)?;
    let bus = Client::connect(config.bus.clone())?;
    let mut pipeline = spawn_pipeline(&config)?;
    let mut input = InputState::default();
    let mut next_announcement = Instant::now();

    loop {
        if Instant::now() >= next_announcement {
            announce(&bus, &config)?;
            next_announcement = Instant::now() + Duration::from_millis(config.ttl_ms / 2);
        }
        if let Some(status) = pipeline.try_wait()? {
            bail!("QEMU display pipeline exited {status}");
        }
        if let Some((envelope, payload)) = bus.try_read()? {
            if envelope.message.kind == DESKTOP_STATE_CONTRACT {
                ensure!(payload.is_empty(), "desktop input carried a binary payload");
                let state: DesktopInputState =
                    serde_json::from_value(Value::Object(envelope.message.body))
                        .context("decode input.desktop_state.v1")?;
                state.validate().map_err(anyhow::Error::msg)?;
                if state.target_id == config.target_id && state.valid_until_unix_ms > now_ms()? {
                    input.apply(&mut qmp, &state)?;
                }
            }
        }
        input.release_if_expired(&mut qmp)?;
    }
}

fn parse_host_port(value: &str) -> Result<(&str, u16)> {
    let (host, port) = value
        .rsplit_once(':')
        .context("media target must be host:port")?;
    ensure!(!host.is_empty(), "media target host is empty");
    Ok((host, port.parse().context("invalid media target port")?))
}

fn pipeline_args(config: &Config) -> Vec<String> {
    QemuRfbH264UdpRuntime {
        rfb_host: config.vnc_host.clone(),
        rfb_port: config.vnc_port,
        udp_targets: config.media_targets.clone(),
        width: config.width,
        height: config.height,
        fps: config.fps,
        bitrate_kbps: config.bitrate_kbps,
    }
    .gstreamer_args()
    .expect("validated QEMU media runtime")
}

fn spawn_pipeline(config: &Config) -> Result<Child> {
    Command::new(&config.gst_launch)
        .args(pipeline_args(config))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .context("start QEMU display pipeline")
}

fn announce(bus: &Client, config: &Config) -> Result<()> {
    let now = now_ms()?;
    let stream = VideoStream {
        schema_version: MEDIA_SCHEMA_VERSION,
        stream_id: config.stream_id.clone(),
        display_name: config.display_name.clone(),
        zone: config.destination.clone(),
        transport: VideoTransport::UdpMpegTs,
        transport_role: TransportRole::Listen,
        endpoint: config.advertised_endpoint.clone(),
        codec: VideoCodec::H264,
        width: config.width,
        height: config.height,
        frame_rate_millihz: config.fps * 1_000,
        announced_at_unix_ms: now,
        valid_until_unix_ms: now + config.ttl_ms,
    };
    stream.validate().map_err(anyhow::Error::msg)?;
    let legacy = bus.envelope(
        Some(&config.destination),
        Message::new(VIDEO_STREAM_CONTRACT, serde_json::to_value(&stream)?),
    )?;
    bus.send(&legacy, &[])
        .context("announce legacy QEMU display")?;
    let projection = stream
        .project_v2(
            &format!("qemu/{}/display", config.target_id),
            &format!("{}:{}", config.target_id, now),
            "unibus-qemu",
        )
        .map_err(anyhow::Error::msg)?;
    for (kind, body) in [
        (
            STREAM_DESCRIPTOR_V2_KIND,
            serde_json::to_value(projection.descriptor)?,
        ),
        (
            ENDPOINT_ANNOUNCEMENT_V2_KIND,
            serde_json::to_value(projection.endpoints)?,
        ),
    ] {
        let envelope = bus.envelope(Some(&config.destination), Message::new(kind, body))?;
        bus.send(&envelope, &[])
            .with_context(|| format!("announce QEMU {kind}"))?;
    }
    Ok(())
}

fn now_ms() -> Result<u64> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis() as u64)
}

struct Qmp {
    writer: UnixStream,
    reader: BufReader<UnixStream>,
    next_id: u64,
}

impl Qmp {
    fn connect(path: &Path) -> Result<Self> {
        let writer = UnixStream::connect(path)
            .with_context(|| format!("connect QMP at {}", path.display()))?;
        writer.set_read_timeout(Some(Duration::from_secs(2)))?;
        writer.set_write_timeout(Some(Duration::from_secs(2)))?;
        let reader = BufReader::new(writer.try_clone()?);
        let mut this = Self {
            writer,
            reader,
            next_id: 1,
        };
        ensure!(
            this.read_value()?.get("QMP").is_some(),
            "invalid QMP greeting"
        );
        this.execute("qmp_capabilities", None)?;
        Ok(this)
    }

    fn execute(&mut self, command: &str, arguments: Option<Value>) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        let mut request = json!({"execute": command, "id": id});
        if let Some(arguments) = arguments {
            request["arguments"] = arguments;
        }
        serde_json::to_writer(&mut self.writer, &request)?;
        self.writer.write_all(b"\n")?;
        self.writer.flush()?;
        loop {
            let response = self.read_value()?;
            if response.get("id").and_then(Value::as_u64) == Some(id) {
                if let Some(error) = response.get("error") {
                    bail!("QMP {command} failed: {error}");
                }
                return Ok(response);
            }
        }
    }

    fn read_value(&mut self) -> Result<Value> {
        let mut line = String::new();
        ensure!(
            self.reader.read_line(&mut line)? != 0,
            "QMP closed the socket"
        );
        serde_json::from_str(&line).context("decode QMP response")
    }

    fn verify_vnc(&mut self, expected_port: u16) -> Result<()> {
        let response = self.execute("query-vnc", None)?;
        let result = response
            .get("return")
            .context("QMP query-vnc has no return")?;
        ensure!(
            result.get("enabled").and_then(Value::as_bool) == Some(true),
            "QEMU VNC is disabled"
        );
        if let Some(service) = result.get("service").and_then(Value::as_str) {
            ensure!(
                service.parse::<u16>()? == expected_port,
                "QMP VNC port mismatch"
            );
        }
        Ok(())
    }

    fn send_events(&mut self, events: Vec<Value>) -> Result<()> {
        if !events.is_empty() {
            self.execute("input-send-event", Some(json!({"events": events})))?;
        }
        Ok(())
    }
}

#[derive(Default)]
struct InputState {
    keys: BTreeSet<u16>,
    buttons: u8,
    last_sequence: Option<u64>,
    expires_at: Option<Instant>,
}

impl InputState {
    fn apply(&mut self, qmp: &mut Qmp, state: &DesktopInputState) -> Result<()> {
        if self
            .last_sequence
            .is_some_and(|sequence| state.sequence <= sequence)
        {
            return Ok(());
        }
        let next = state.keys.iter().copied().collect::<BTreeSet<_>>();
        let mut events = key_diff_events(&self.keys, &next)?;
        events.extend(button_diff_events(self.buttons, state.pointer_buttons));
        events.push(json!({"type":"abs","data":{"axis":"x","value": i64::from(state.pointer_x) * 0x7fff / 0xffff}}));
        events.push(json!({"type":"abs","data":{"axis":"y","value": i64::from(state.pointer_y) * 0x7fff / 0xffff}}));
        if state.scroll_x != 0 {
            events.push(json!({"type":"rel","data":{"axis":"x","value":state.scroll_x}}));
        }
        if state.scroll_y != 0 {
            events.push(json!({"type":"rel","data":{"axis":"y","value":state.scroll_y}}));
        }
        qmp.send_events(events)?;
        self.keys = next;
        self.buttons = state.pointer_buttons;
        self.last_sequence = Some(state.sequence);
        let remaining = state.valid_until_unix_ms.saturating_sub(now_ms()?);
        self.expires_at = Some(Instant::now() + Duration::from_millis(remaining));
        Ok(())
    }

    fn release_if_expired(&mut self, qmp: &mut Qmp) -> Result<()> {
        if self
            .expires_at
            .is_some_and(|deadline| deadline <= Instant::now())
        {
            let mut events = key_diff_events(&self.keys, &BTreeSet::new())?;
            events.extend(button_diff_events(self.buttons, 0));
            qmp.send_events(events)?;
            self.keys.clear();
            self.buttons = 0;
            self.expires_at = None;
        }
        Ok(())
    }
}

fn key_diff_events(before: &BTreeSet<u16>, after: &BTreeSet<u16>) -> Result<Vec<Value>> {
    let mut events = Vec::new();
    for usage in before.difference(after) {
        events.push(key_event(*usage, false)?);
    }
    for usage in after.difference(before) {
        events.push(key_event(*usage, true)?);
    }
    Ok(events)
}

fn key_event(usage: u16, down: bool) -> Result<Value> {
    let qcode =
        hid_usage_to_qcode(usage).with_context(|| format!("unsupported HID usage {usage:#06x}"))?;
    Ok(json!({"type":"key","data":{"down":down,"key":{"type":"qcode","data":qcode}}}))
}

fn button_diff_events(before: u8, after: u8) -> Vec<Value> {
    [
        (pointer_button::LEFT, "left"),
        (pointer_button::RIGHT, "right"),
        (pointer_button::MIDDLE, "middle"),
    ]
    .into_iter()
    .filter_map(|(mask, button)| {
        ((before & mask) != (after & mask))
            .then(|| json!({"type":"btn","data":{"down":after & mask != 0,"button":button}}))
    })
    .collect()
}

fn hid_usage_to_qcode(usage: u16) -> Option<&'static str> {
    Some(match usage {
        0x04..=0x1d => {
            (const {
                [
                    "a", "b", "c", "d", "e", "f", "g", "h", "i", "j", "k", "l", "m", "n", "o", "p",
                    "q", "r", "s", "t", "u", "v", "w", "x", "y", "z",
                ]
            })[(usage - 0x04) as usize]
        }
        0x1e..=0x26 => {
            (const { ["1", "2", "3", "4", "5", "6", "7", "8", "9"] })[(usage - 0x1e) as usize]
        }
        0x27 => "0",
        0x28 => "ret",
        0x29 => "esc",
        0x2a => "backspace",
        0x2b => "tab",
        0x2c => "spc",
        0x2d => "minus",
        0x2e => "equal",
        0x2f => "bracket_left",
        0x30 => "bracket_right",
        0x31 => "backslash",
        0x33 => "semicolon",
        0x34 => "apostrophe",
        0x35 => "grave_accent",
        0x36 => "comma",
        0x37 => "dot",
        0x38 => "slash",
        0x39 => "caps_lock",
        0x3a..=0x45 => {
            (const {
                [
                    "f1", "f2", "f3", "f4", "f5", "f6", "f7", "f8", "f9", "f10", "f11", "f12",
                ]
            })[(usage - 0x3a) as usize]
        }
        0x49 => "insert",
        0x4a => "home",
        0x4b => "pgup",
        0x4c => "delete",
        0x4d => "end",
        0x4e => "pgdn",
        0x4f => "right",
        0x50 => "left",
        0x51 => "down",
        0x52 => "up",
        0xe0 => "ctrl",
        0xe1 => "shift",
        0xe2 => "alt",
        0xe3 => "meta_l",
        0xe4 => "ctrl_r",
        0xe5 => "shift_r",
        0xe6 => "alt_r",
        0xe7 => "meta_r",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_encoder_fans_out_to_every_media_target() {
        let config = Config {
            bus: serde_json::from_value(json!({"installation_id":"home","producer_id":"qemu","hub_endpoint":"127.0.0.1:1","token_env":"TOKEN","zones":["compute"]})).unwrap(),
            destination: "living-room".into(), gst_launch: "/bin/true".into(), qmp_socket: "/tmp/qmp".into(),
            vnc_host: "127.0.0.1".into(), vnc_port: 5901, target_id: "vm".into(), stream_id: "vm".into(),
            display_name: "VM".into(), media_targets: vec!["192.0.2.1:55300".into(), "192.0.2.2:55300".into()],
            advertised_endpoint: "udp://0.0.0.0:55300".into(), width: 1280, height: 720, fps: 30,
            bitrate_kbps: 8000, ttl_ms: 30000,
        };
        let args = pipeline_args(&config);
        assert_eq!(
            args.iter()
                .filter(|arg| arg.as_str() == "nvh264enc")
                .count(),
            1
        );
        assert!(args.contains(&"clients=192.0.2.1:55300,192.0.2.2:55300".into()));
    }

    #[test]
    fn key_diff_releases_before_pressing() {
        let before = BTreeSet::from([0x04]);
        let after = BTreeSet::from([0x05]);
        let events = key_diff_events(&before, &after).unwrap();
        assert_eq!(events[0]["data"]["down"], false);
        assert_eq!(events[0]["data"]["key"]["data"], "a");
        assert_eq!(events[1]["data"]["down"], true);
        assert_eq!(events[1]["data"]["key"]["data"], "b");
    }
}
