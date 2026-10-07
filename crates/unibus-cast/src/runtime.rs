//! Fixed mirror backend plans. Operators choose endpoints, never pipelines.
use serde::Deserialize;
use std::net::{Ipv4Addr, SocketAddrV4};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MirrorConfig {
    pub listen: SocketAddrV4,
    pub allowed_clients: Vec<Ipv4Addr>,
    pub receiver_name: String,
    pub receiver_device_id: String,
    pub receiver_port: u16,
    pub video_rtp_port: u16,
    pub audio_rtp_port: u16,
    #[serde(default = "receiver_enabled_default")]
    pub receiver_enabled: bool,
    #[serde(default = "video_bind_default")]
    pub video_bind: Ipv4Addr,
    #[serde(default)]
    pub normalize_video: bool,
}
fn receiver_enabled_default() -> bool {
    true
}
fn video_bind_default() -> Ipv4Addr {
    Ipv4Addr::LOCALHOST
}

impl MirrorConfig {
    pub fn validate(&self) -> Result<(), String> {
        if !self.video_bind.is_private() && !self.video_bind.is_loopback() {
            return Err("RTP bind must be a specific private or loopback IPv4 address".into());
        }
        if !self.listen.ip().is_private() || self.listen.port() < 1024 {
            return Err("HTTP listener must be a specific private LAN IPv4 endpoint".into());
        }
        if self.allowed_clients.is_empty()
            || self.allowed_clients.len() > 16
            || self.allowed_clients.iter().any(|ip| !ip.is_private())
        {
            return Err("require 1..16 explicit private LAN client IPs".into());
        }
        if self.receiver_name.is_empty()
            || self.receiver_name.len() > 64
            || self.receiver_name.chars().any(char::is_control)
        {
            return Err("receiver name invalid".into());
        }
        let octets: Vec<_> = self.receiver_device_id.split(':').collect();
        if octets.len() != 6
            || octets
                .iter()
                .any(|v| v.len() != 2 || u8::from_str_radix(v, 16).is_err())
        {
            return Err("receiver device ID must be a stable MAC-format identity".into());
        }
        if !(1024..=65533).contains(&self.receiver_port)
            || self.video_rtp_port < 1024
            || self.audio_rtp_port < 1024
        {
            return Err("unprivileged distinct ports required".into());
        }
        let ports = [
            self.receiver_port,
            self.receiver_port + 1,
            self.receiver_port + 2,
            self.video_rtp_port,
            self.audio_rtp_port,
            self.listen.port(),
        ];
        for (i, port) in ports.iter().enumerate() {
            if ports[..i].contains(port) {
                return Err("ports overlap".into());
            }
        }
        Ok(())
    }

    pub fn uxplay_args(&self) -> Result<Vec<String>, String> {
        self.validate()?;
        // Ignore implicit user config; PIN-pair once and retain registered
        // devices in UxPlay's owner-private registration file.
        Ok(vec![
            "-rc".into(),
            "/dev/null".into(),
            "-n".into(),
            self.receiver_name.clone(),
            "-m".into(),
            self.receiver_device_id.clone(),
            "-key".into(),
            "-p".into(),
            self.receiver_port.to_string(),
            "-pin".into(),
            "-reg".into(),
            "-s".into(),
            "1920x1080".into(),
            "-fps".into(),
            "30".into(),
            "-vrtp".into(),
            format!(
                "config-interval=1 ! udpsink host=127.0.0.1 port={}",
                self.video_rtp_port
            ),
            "-artp".into(),
            format!(
                "pt=96 ! udpsink host=127.0.0.1 port={}",
                self.audio_rtp_port
            ),
        ])
    }

    pub fn gstreamer_args(&self) -> Result<Vec<String>, String> {
        self.validate()?;
        // H264-only first slice. No raw video round-trip, software video encoder,
        // or unrestricted host-provided GStreamer graph.
        let mut args: Vec<String> = vec!["-q".into(), "mpegtsmux".into(), "name=mux".into(),
            // Stock Android MediaPlayer uses byte-based HTTP buffering; null
            // packets bound startup even for an almost-static desktop.
            "bitrate=8000000".into(), "alignment=7".into(), "!".into(), "fdsink".into(), "fd=1".into(), "sync=false".into(),
            "udpsrc".into(), "address=127.0.0.1".into(), "buffer-size=4194304".into(), format!("port={}",self.video_rtp_port),
            "caps=application/x-rtp,media=video,clock-rate=90000,encoding-name=H264,payload=96".into(),
            "!".into(), "rtpjitterbuffer".into(), "latency=100".into(), "drop-on-latency=false".into(),
            "!".into(), "rtph264depay".into(), "wait-for-keyframe=true".into(),
            "!".into(), "h264parse".into(), "config-interval=-1".into(),
            "!".into(), "video/x-h264,stream-format=byte-stream,alignment=au".into(),
            "!".into(), "queue".into(), "max-size-time=1000000000".into(), "!".into(), "mux.".into(),
            "udpsrc".into(), "address=127.0.0.1".into(), format!("port={}",self.audio_rtp_port),
            "caps=application/x-rtp,media=audio,clock-rate=44100,encoding-name=L16,channels=2,payload=96".into(),
            "!".into(), "rtpjitterbuffer".into(), "latency=100".into(), "drop-on-latency=true".into(),
            "!".into(), "rtpL16depay".into(), "!".into(), "audioconvert".into(),
            "!".into(), "aud.".into(),
            // A silent live input prevents a muted/silent sender from starving
            // the mux. Real incoming audio is mixed in when its pad activates.
            "audiotestsrc".into(), "is-live=true".into(), "wave=silence".into(),
            "!".into(), "audio/x-raw,rate=44100,channels=2".into(), "!".into(), "aud.".into(),
            "audiomixer".into(), "name=aud".into(), "force-live=true".into(), "ignore-inactive-pads=true".into(),
            "!".into(), "audioconvert".into(), "!".into(), "voaacenc".into(), "bitrate=128000".into(),
            "!".into(), "aacparse".into(), "!".into(), "queue".into(),
            "max-size-time=1000000000".into(), "!".into(), "mux.".into()];
        for arg in &mut args {
            if arg == "address=127.0.0.1" {
                *arg = format!("address={}", self.video_bind);
            }
        }
        if self.normalize_video {
            let index = args
                .iter()
                .position(|arg| arg == "h264parse")
                .ok_or("video parser missing")?;
            // GPU decode/reencode normalizes non-IDR Mac I-slices into a real
            // IDR cadence with repeated codec headers. No software fallback.
            args.splice(
                index..index,
                [
                    "h264parse",
                    "!",
                    "nvh264dec",
                    "!",
                    "video/x-raw(memory:CUDAMemory),format=NV12",
                    "!",
                    "nvh264enc",
                    "gop-size=30",
                    "bitrate=6000",
                    "bframes=0",
                    "rc-lookahead=0",
                    "zerolatency=true",
                    "repeat-sequence-header=true",
                    "!",
                ]
                .map(String::from),
            );
        }
        Ok(args)
    }
}

pub fn allowed_request(bytes: &[u8]) -> bool {
    if bytes.len() > 4096 || !bytes.ends_with(b"\r\n\r\n") {
        return false;
    }
    matches!(
        bytes.split(|b| *b == b'\n').next(),
        Some(b"GET /live.ts HTTP/1.1\r" | b"GET /live.ts HTTP/1.0\r")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    fn config() -> MirrorConfig {
        MirrorConfig {
            listen: "192.168.2.74:8099".parse().unwrap(),
            allowed_clients: vec!["192.168.2.97".parse().unwrap()],
            receiver_name: "fungOS Mirror".into(),
            receiver_device_id: "02:46:50:4c:00:01".into(),
            receiver_port: 35100,
            video_rtp_port: 35200,
            audio_rtp_port: 35201,
            receiver_enabled: true,
            video_bind: Ipv4Addr::LOCALHOST,
            normalize_video: false,
        }
    }
    #[test]
    fn gpu_mux_only_plan_has_explicit_private_bind_and_no_software_fallback() {
        let mut c = config();
        c.receiver_enabled = false;
        c.normalize_video = true;
        c.video_bind = "192.168.2.67".parse().unwrap();
        let graph = c.gstreamer_args().unwrap().join(" ");
        assert!(graph.contains("nvh264dec") && graph.contains("nvh264enc"));
        assert!(graph.contains("repeat-sequence-header=true") && graph.contains("gop-size=30"));
        assert!(graph.contains("address=192.168.2.67"));
        assert!(!graph.contains("avdec") && !graph.contains("x264enc"));
        c.video_bind = Ipv4Addr::UNSPECIFIED;
        assert!(c.validate().is_err());
    }
    #[test]
    fn fixed_plans_and_private_admission() {
        let c = config();
        assert!(c.validate().is_ok());
        assert!(c.uxplay_args().unwrap().contains(&"-pin".into()));
        assert!(c.uxplay_args().unwrap().contains(&"-reg".into()));
        assert!(!c.uxplay_args().unwrap().contains(&"-pw".into()));
        let plan = c.gstreamer_args().unwrap().join(" ");
        assert!(plan.contains("rtph264depay") && plan.contains("voaacenc"));
        assert!(plan.contains("wave=silence") && plan.contains("ignore-inactive-pads=true"));
        assert!(!plan.contains("decodebin") && !plan.contains("x264enc"));
        assert!(allowed_request(
            b"GET /live.ts HTTP/1.1\r\nHost: local\r\n\r\n"
        ));
        assert!(!allowed_request(b"POST /live.ts HTTP/1.1\r\n\r\n"));
        assert!(!allowed_request(b"GET /other HTTP/1.1\r\n\r\n"));
        let mut c = config();
        c.listen = "0.0.0.0:8099".parse().unwrap();
        assert!(c.validate().is_err());
        c = config();
        c.audio_rtp_port = c.video_rtp_port;
        assert!(c.validate().is_err());
    }
}
