//! Headless libretro session host.
//!
//! The core ABI is the only unsafe boundary. Video leaves as a hardware-
//! encoded MPEG-TS stream; controls arrive as expiring absolute Unibus state.

use anyhow::{Context, Result, bail, ensure};
use libloading::Library;
use serde::Deserialize;
use sha2::{Digest, Sha256};
#[cfg(unix)]
use std::os::unix::{fs::FileTypeExt, net::UnixDatagram};
use std::{
    ffi::{c_char, c_void},
    fs::File,
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, Command, Stdio},
    ptr,
    sync::{Arc, Mutex, OnceLock},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use unibus::Message;
use unibus::client::{Client, ClientConfig};
#[cfg(unix)]
use unibus_input::ObservationSocket;
use unibus_input::{
    GAMEPAD_STATE_CONTRACT, GamepadState, OBSERVATION_PROTOCOL_VERSION, ObservationRange,
    ObservationRegion, ObservationRequest, ObservationResponse, button, encode_observation,
};
use unibus_media::{
    MEDIA_SCHEMA_VERSION, TransportRole, VIDEO_STREAM_CONTRACT, VideoCodec, VideoStream,
    VideoTransport,
};

/// Latest-value presence for the game/session registry. Consumers can keep
/// the newest message per session_id and consider entries with
/// valid_until_unix_ms in the past inactive.
const GAME_SESSION_CONTRACT: &str = "retro.game.session.v1";

const RETRO_DEVICE_JOYPAD: u32 = 1;
const RETRO_DEVICE_ANALOG: u32 = 5;
const RETRO_ENVIRONMENT_GET_CAN_DUPE: u32 = 3;
const RETRO_ENVIRONMENT_GET_SYSTEM_DIRECTORY: u32 = 9;
const RETRO_ENVIRONMENT_SET_PIXEL_FORMAT: u32 = 10;
const RETRO_ENVIRONMENT_SET_SUPPORT_NO_GAME: u32 = 18;
const RETRO_ENVIRONMENT_GET_SAVE_DIRECTORY: u32 = 31;
const PIXEL_0RGB1555: u32 = 0;
const PIXEL_XRGB8888: u32 = 1;
const PIXEL_RGB565: u32 = 2;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Encoder {
    Software,
    AppleVideoToolbox,
    NvidiaNvenc,
    Vaapi,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    #[serde(default)]
    bus: Option<ClientConfig>,
    session_id: String,
    /// Human-readable title shown by game/session dashboards.
    #[serde(default)]
    game_name: Option<String>,
    core_path: PathBuf,
    #[serde(default)]
    content_path: Option<PathBuf>,
    system_dir: PathBuf,
    save_dir: PathBuf,
    ffmpeg_path: PathBuf,
    /// Encoder destination, commonly a local SSH tunnel endpoint.
    stream_endpoint: String,
    /// Endpoint advertised to media edges (from their network namespace).
    advertised_endpoint: String,
    /// Media transport used by FFmpeg and advertised to consumers. UDP keeps
    /// a headless session alive when no Canvas receiver is attached.
    #[serde(default)]
    video_transport: Option<VideoTransport>,
    #[serde(default)]
    advertised_transport_role: TransportRole,
    encoder: Encoder,
    #[serde(default = "default_player_slots")]
    player_slots: u8,
    /// Standalone mode bypasses the router and accepts the same bounded local
    /// reports directly. Exactly one of `bus` or this path is required.
    #[cfg(unix)]
    #[serde(default)]
    local_controller_socket_path: Option<PathBuf>,
    /// Spawn ffplay against the configured UDP MPEG-TS endpoint.
    #[serde(default)]
    local_video_player: bool,
    #[serde(default)]
    ffplay_path: Option<PathBuf>,
    /// Optional local request/reply socket exposing the core's standard
    /// serialized state. A client sends an empty datagram and receives one
    /// complete save-state snapshot. Gameplay video remains on the media plane.
    #[cfg(unix)]
    #[serde(default)]
    observation_socket_path: Option<PathBuf>,
}

fn default_player_slots() -> u8 {
    1
}

#[repr(C)]
#[derive(Clone, Copy)]
struct RetroGameInfo {
    path: *const c_char,
    data: *const c_void,
    size: usize,
    meta: *const c_char,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct RetroSystemInfo {
    library_name: *const c_char,
    library_version: *const c_char,
    valid_extensions: *const c_char,
    need_fullpath: bool,
    block_extract: bool,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct RetroGameGeometry {
    base_width: u32,
    base_height: u32,
    max_width: u32,
    max_height: u32,
    aspect_ratio: f32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct RetroSystemTiming {
    fps: f64,
    sample_rate: f64,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct RetroSystemAvInfo {
    geometry: RetroGameGeometry,
    timing: RetroSystemTiming,
}

#[derive(Clone, Copy)]
struct Pad {
    buttons: u32,
    /// Rising edges remain visible for one emulator poll even when a complete
    /// press/release pair arrives while video output is blocking the run loop.
    pressed: u32,
    axes: unibus_input::GamepadAxes,
    valid_until_ms: u64,
}

impl Default for Pad {
    fn default() -> Self {
        Self {
            buttons: 0,
            pressed: 0,
            axes: Default::default(),
            valid_until_ms: 0,
        }
    }
}

struct Runtime {
    pixel_format: u32,
    width: u32,
    height: u32,
    encoder: Option<ChildStdin>,
    encoder_error: Option<String>,
    logged_frame: bool,
    system_dir: std::ffi::CString,
    save_dir: std::ffi::CString,
}

#[cfg(unix)]
struct LocalInputSocketGuard(PathBuf);

#[cfg(unix)]
impl Drop for LocalInputSocketGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

static RUNTIME: OnceLock<Arc<Mutex<Runtime>>> = OnceLock::new();
static PADS: OnceLock<Arc<Mutex<Vec<Pad>>>> = OnceLock::new();

type SetEnvironment = unsafe extern "C" fn(unsafe extern "C" fn(u32, *mut c_void) -> bool);
type SetVideo = unsafe extern "C" fn(unsafe extern "C" fn(*const c_void, u32, u32, usize));
type SetAudio = unsafe extern "C" fn(unsafe extern "C" fn(i16, i16));
type SetAudioBatch = unsafe extern "C" fn(unsafe extern "C" fn(*const i16, usize) -> usize);
type SetInputPoll = unsafe extern "C" fn(unsafe extern "C" fn());
type SetInputState = unsafe extern "C" fn(unsafe extern "C" fn(u32, u32, u32, u32) -> i16);
type SerializeSize = unsafe extern "C" fn() -> usize;
type Serialize = unsafe extern "C" fn(*mut c_void, usize) -> bool;
type GetMemoryData = unsafe extern "C" fn(u32) -> *mut c_void;
type GetMemorySize = unsafe extern "C" fn(u32) -> usize;

#[cfg(unix)]
struct ObservationSource<'a> {
    session_id: &'a str,
    core_id: &'a str,
    content_id: &'a str,
    serialize_size: SerializeSize,
    serialize: Serialize,
    get_memory_data: GetMemoryData,
    get_memory_size: GetMemorySize,
}

fn main() -> Result<()> {
    let path = std::env::args()
        .nth(1)
        .context("usage: unibus-libretro-session <config.json>")?;
    let config: Config =
        serde_json::from_slice(&std::fs::read(&path).with_context(|| format!("read {path}"))?)?;
    validate_config(&config)?;
    std::fs::create_dir_all(&config.system_dir)?;
    std::fs::create_dir_all(&config.save_dir)?;

    let core_bytes = std::fs::read(&config.core_path)
        .with_context(|| format!("read core {}", config.core_path.display()))?;
    let core_id = hex_digest(&core_bytes);
    let content_id = match &config.content_path {
        Some(path) => digest_file(path)?,
        None => "0".repeat(64),
    };
    eprintln!(
        "libretro-session: session={} core_sha256={} content_sha256={}",
        config.session_id, core_id, content_id
    );

    let runtime = Arc::new(Mutex::new(Runtime {
        pixel_format: PIXEL_0RGB1555,
        width: 0,
        height: 0,
        encoder: None,
        encoder_error: None,
        logged_frame: false,
        system_dir: path_cstring(&config.system_dir)?,
        save_dir: path_cstring(&config.save_dir)?,
    }));
    RUNTIME
        .set(runtime.clone())
        .map_err(|_| anyhow::anyhow!("runtime already initialized"))?;
    let pads = Arc::new(Mutex::new(vec![
        Pad::default();
        usize::from(config.player_slots)
    ]));
    PADS.set(pads.clone())
        .map_err(|_| anyhow::anyhow!("pads already initialized"))?;
    // SAFETY: the library stays alive until after retro_deinit, every loaded
    // symbol uses the published libretro ABI, and callbacks access only the
    // process-lifetime RUNTIME singleton.
    unsafe {
        let library = Library::new(&config.core_path)
            .with_context(|| format!("load libretro core {}", config.core_path.display()))?;
        let set_environment = *library.get::<SetEnvironment>(b"retro_set_environment\0")?;
        let set_video = *library.get::<SetVideo>(b"retro_set_video_refresh\0")?;
        let set_audio = *library.get::<SetAudio>(b"retro_set_audio_sample\0")?;
        let set_audio_batch = *library.get::<SetAudioBatch>(b"retro_set_audio_sample_batch\0")?;
        let set_input_poll = *library.get::<SetInputPoll>(b"retro_set_input_poll\0")?;
        let set_input_state = *library.get::<SetInputState>(b"retro_set_input_state\0")?;
        let init = *library.get::<unsafe extern "C" fn()>(b"retro_init\0")?;
        let get_system_info = *library
            .get::<unsafe extern "C" fn(*mut RetroSystemInfo)>(b"retro_get_system_info\0")?;
        let _deinit = *library.get::<unsafe extern "C" fn()>(b"retro_deinit\0")?;
        let load_game = *library
            .get::<unsafe extern "C" fn(*const RetroGameInfo) -> bool>(b"retro_load_game\0")?;
        let _unload_game = *library.get::<unsafe extern "C" fn()>(b"retro_unload_game\0")?;
        let get_av = *library
            .get::<unsafe extern "C" fn(*mut RetroSystemAvInfo)>(b"retro_get_system_av_info\0")?;
        let run = *library.get::<unsafe extern "C" fn()>(b"retro_run\0")?;
        let serialize_size = *library.get::<SerializeSize>(b"retro_serialize_size\0")?;
        let serialize = *library.get::<Serialize>(b"retro_serialize\0")?;
        let get_memory_data = *library.get::<GetMemoryData>(b"retro_get_memory_data\0")?;
        let get_memory_size = *library.get::<GetMemorySize>(b"retro_get_memory_size\0")?;

        set_environment(environment);
        set_video(video_refresh);
        set_audio(audio_sample);
        set_audio_batch(audio_sample_batch);
        set_input_poll(input_poll);
        set_input_state(input_state);
        let mut system_info = std::mem::zeroed::<RetroSystemInfo>();
        get_system_info(&mut system_info);
        init();

        let content_path = config
            .content_path
            .as_ref()
            .map(|path| std::ffi::CString::new(path.as_os_str().as_encoded_bytes()))
            .transpose()?;
        let content_bytes = if system_info.need_fullpath {
            None
        } else {
            config
                .content_path
                .as_ref()
                .map(|path| {
                    std::fs::read(path).with_context(|| format!("read content {}", path.display()))
                })
                .transpose()?
        };
        let info = content_path.as_ref().map(|path| {
            let (data, size) = content_bytes.as_ref().map_or((ptr::null(), 0), |bytes| {
                (bytes.as_ptr().cast::<c_void>(), bytes.len())
            });
            RetroGameInfo {
                path: path.as_ptr(),
                data,
                size,
                meta: ptr::null(),
            }
        });
        let info_ptr = info.as_ref().map_or(ptr::null(), |value| value as *const _);
        ensure!(load_game(info_ptr), "libretro core rejected content");
        let mut av = std::mem::zeroed::<RetroSystemAvInfo>();
        get_av(&mut av);
        validate_av(av)?;
        let (display_width, display_height) = display_dimensions(av)?;
        eprintln!(
            "libretro-session: framebuffer={}x{} display={}x{} aspect={:.4} fps={:.3} sample_rate={:.0}",
            av.geometry.base_width,
            av.geometry.base_height,
            display_width,
            display_height,
            av.geometry.aspect_ratio,
            av.timing.fps,
            av.timing.sample_rate
        );
        {
            let mut state = runtime.lock().unwrap();
            state.width = av.geometry.base_width;
            state.height = av.geometry.base_height;
        }
        if let Some(bus) = config.bus.clone() {
            let zone = bus.zones[0].clone();
            spawn_input(
                bus,
                config.session_id.clone(),
                config.game_name.clone(),
                pads.clone(),
                VideoStream {
                    schema_version: MEDIA_SCHEMA_VERSION,
                    stream_id: config.session_id.clone(),
                    display_name: "Libretro".into(),
                    zone,
                    transport: config
                        .video_transport
                        .clone()
                        .unwrap_or(VideoTransport::TcpMpegTs),
                    transport_role: config.advertised_transport_role,
                    endpoint: config.advertised_endpoint.clone(),
                    codec: VideoCodec::H264,
                    width: display_width,
                    height: display_height,
                    frame_rate_millihz: (av.timing.fps * 1000.0).round() as u32,
                    announced_at_unix_ms: 0,
                    valid_until_unix_ms: 1,
                },
            );
        } else {
            #[cfg(unix)]
            spawn_local_input(
                config
                    .local_controller_socket_path
                    .clone()
                    .context("standalone session lacks local_controller_socket_path")?,
                pads.clone(),
            )?;
        }
        if config.local_video_player {
            start_local_player(&config)?;
        }
        // The media edge must bind its TCP receiver before FFmpeg connects.
        // Announcements are latest-value state, so a short bounded rendezvous
        // is cheaper and more deterministic than retrying a dead encoder.
        thread::sleep(Duration::from_millis(750));
        {
            let mut state = runtime.lock().unwrap();
            let (child, stdin) = start_encoder(&config, &av)?;
            state.encoder = Some(stdin);
            std::mem::forget(child); // FFmpeg owns session lifetime; pipe EOF stops it.
        }

        #[cfg(unix)]
        let observation = config
            .observation_socket_path
            .as_deref()
            .map(ObservationSocket::publish)
            .transpose()?;
        #[cfg(unix)]
        let observation_source = ObservationSource {
            session_id: &config.session_id,
            core_id: &core_id,
            content_id: &content_id,
            serialize_size,
            serialize,
            get_memory_data,
            get_memory_size,
        };

        let frame = Duration::from_secs_f64(1.0 / av.timing.fps);
        let mut deadline = Instant::now();
        loop {
            run();
            #[cfg(unix)]
            if let Some(observation) = &observation {
                serve_observation(observation, &observation_source)?;
            }
            if let Some(error) = runtime.lock().unwrap().encoder_error.take() {
                bail!("video encoder failed: {error}");
            }
            deadline += frame;
            if let Some(delay) = deadline.checked_duration_since(Instant::now()) {
                thread::sleep(delay);
            } else {
                deadline = Instant::now();
            }
        }
    }
    #[allow(unreachable_code)]
    Ok(())
}

fn validate_config(config: &Config) -> Result<()> {
    ensure!(
        !config.session_id.is_empty() && config.session_id.len() <= 64,
        "invalid session_id"
    );
    ensure!(
        (1..=4).contains(&config.player_slots),
        "player_slots must be 1..=4"
    );
    ensure!(
        config.stream_endpoint.starts_with("tcp://")
            || config.stream_endpoint.starts_with("udp://"),
        "stream_endpoint must be tcp:// or udp://"
    );
    ensure!(
        config.advertised_endpoint.starts_with("tcp://")
            || config.advertised_endpoint.starts_with("udp://"),
        "advertised_endpoint must be tcp:// or udp://"
    );
    ensure!(config.core_path.is_file(), "core_path is not a file");
    #[cfg(unix)]
    ensure!(
        config.bus.is_some() ^ config.local_controller_socket_path.is_some(),
        "configure exactly one of bus or local_controller_socket_path"
    );
    if config.local_video_player {
        ensure!(
            config.stream_endpoint.starts_with("udp://127.0.0.1:"),
            "local_video_player requires a loopback UDP stream_endpoint"
        );
        ensure!(
            config
                .ffplay_path
                .as_ref()
                .is_some_and(|path| path.is_file()),
            "local_video_player requires an existing ffplay_path"
        );
    }
    if let Some(path) = &config.content_path {
        ensure!(path.is_file(), "content_path is not a file");
    }
    #[cfg(unix)]
    if let Some(path) = &config.observation_socket_path {
        ensure!(
            path.is_absolute(),
            "observation_socket_path must be absolute"
        );
        if let Ok(metadata) = path.symlink_metadata() {
            ensure!(
                metadata.file_type().is_socket(),
                "refusing to replace non-socket observation path {}",
                path.display()
            );
        }
    }
    Ok(())
}

#[cfg(unix)]
fn serve_observation(
    observation: &ObservationSocket,
    source: &ObservationSource<'_>,
) -> Result<()> {
    let mut request_bytes = [0_u8; 8192];
    loop {
        let (request_len, peer) = match observation.socket().recv_from(&mut request_bytes) {
            Ok(value) => value,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => return Ok(()),
            Err(error) => return Err(error).context("receive observation request"),
        };
        // A junk request or a reply address that vanished is the client's
        // failure, not the session's. Propagating it killed the emulator, whose
        // restart then re-broke every other agent attached to it — one agent
        // restarting must not take the session down with it.
        if let Err(error) =
            answer_observation(observation, source, &request_bytes[..request_len], &peer)
        {
            eprintln!("libretro-session: observation request failed: {error:#}");
        }
    }
}

#[cfg(unix)]
fn answer_observation(
    observation: &ObservationSocket,
    source: &ObservationSource<'_>,
    request_bytes: &[u8],
    peer: &std::os::unix::net::SocketAddr,
) -> Result<()> {
    {
        if request_bytes.is_empty() {
            let state = serialized_state(source)?;
            observation
                .socket()
                .send_to_addr(&state, peer)
                .context("send legacy observation snapshot")?;
            return Ok(());
        }
        let request: ObservationRequest =
            serde_json::from_slice(request_bytes).context("decode observation request")?;
        request.validate().map_err(anyhow::Error::msg)?;
        let (region_size, region) = snapshot_region(source, request.region)?;
        let ranges = if request.ranges.is_empty() {
            vec![ObservationRange {
                offset: 0,
                length: region_size
                    .try_into()
                    .context("observation region exceeds u32")?,
            }]
        } else {
            request.ranges
        };
        let payload_len = ranges
            .iter()
            .try_fold(0_usize, |total, range| {
                total.checked_add(range.length as usize)
            })
            .context("observation payload length overflow")?;
        ensure!(
            payload_len <= unibus_input::MAX_OBSERVATION_PAYLOAD_BYTES,
            "observation payload exceeds limit"
        );
        let mut payload = Vec::with_capacity(payload_len);
        for range in &ranges {
            let start: usize = range
                .offset
                .try_into()
                .context("range offset exceeds usize")?;
            let end = start
                .checked_add(range.length as usize)
                .context("observation range overflow")?;
            let bytes = region
                .get(start..end)
                .context("observation range exceeds region")?;
            payload.extend_from_slice(bytes);
        }
        let response = ObservationResponse {
            protocol_version: OBSERVATION_PROTOCOL_VERSION,
            session_id: source.session_id.into(),
            core_id: source.core_id.into(),
            content_id: source.content_id.into(),
            region: request.region,
            region_size: region_size as u64,
            ranges,
            observed_at_unix_ms: now_ms(),
            payload_length: payload.len() as u32,
        };
        let packet = encode_observation(&response, &payload).map_err(anyhow::Error::msg)?;
        observation
            .socket()
            .send_to_addr(&packet, peer)
            .with_context(|| format!("send {:?} observation snapshot", request.region))?;
        Ok(())
    }
}

#[cfg(unix)]
fn serialized_state(source: &ObservationSource<'_>) -> Result<Vec<u8>> {
    let size = unsafe { (source.serialize_size)() };
    ensure!(
        (1..=unibus_input::MAX_OBSERVATION_PAYLOAD_BYTES).contains(&size),
        "serialized core state size {size} exceeds local observation limit"
    );
    let mut state = vec![0_u8; size];
    ensure!(
        unsafe { (source.serialize)(state.as_mut_ptr().cast(), state.len()) },
        "libretro core refused to serialize state"
    );
    Ok(state)
}

#[cfg(unix)]
fn snapshot_region(
    source: &ObservationSource<'_>,
    region: ObservationRegion,
) -> Result<(usize, Vec<u8>)> {
    if region == ObservationRegion::SerializedState {
        let state = serialized_state(source)?;
        return Ok((state.len(), state));
    }
    let id = region.libretro_memory_id().expect("memory region has id");
    let size = unsafe { (source.get_memory_size)(id) };
    ensure!(size > 0, "core does not expose {region:?}");
    let data = unsafe { (source.get_memory_data)(id) };
    ensure!(!data.is_null(), "core returned null data for {region:?}");
    let bytes = unsafe { std::slice::from_raw_parts(data.cast::<u8>(), size) };
    Ok((size, bytes.to_vec()))
}

fn validate_av(av: RetroSystemAvInfo) -> Result<()> {
    ensure!(
        av.geometry.base_width > 0 && av.geometry.base_height > 0,
        "core returned invalid geometry"
    );
    ensure!(
        av.geometry.base_width <= 4096 && av.geometry.base_height <= 4096,
        "core geometry exceeds 4096x4096"
    );
    ensure!(
        av.timing.fps.is_finite() && (1.0..=240.0).contains(&av.timing.fps),
        "core returned invalid fps"
    );
    Ok(())
}

fn display_dimensions(av: RetroSystemAvInfo) -> Result<(u32, u32)> {
    let aspect = av.geometry.aspect_ratio;
    if !aspect.is_finite() || aspect <= 0.0 {
        return Ok((av.geometry.base_width, av.geometry.base_height));
    }
    ensure!(
        (0.2..=5.0).contains(&aspect),
        "core returned invalid display aspect ratio"
    );
    let height = av.geometry.base_height;
    let width = ((f64::from(height) * f64::from(aspect)).round() as u32).clamp(16, 4096);
    Ok((width, height))
}

fn start_encoder(config: &Config, av: &RetroSystemAvInfo) -> Result<(Child, ChildStdin)> {
    let size = format!("{}x{}", av.geometry.base_width, av.geometry.base_height);
    let (display_width, display_height) = display_dimensions(*av)?;
    let scale = format!("scale={display_width}:{display_height}:flags=neighbor");
    let rate = format!("{:.6}", av.timing.fps);
    let mut command = Command::new(&config.ffmpeg_path);
    command.args([
        "-hide_banner",
        "-loglevel",
        "warning",
        "-f",
        "rawvideo",
        "-pixel_format",
        "rgba",
        "-video_size",
        &size,
        "-framerate",
        &rate,
        "-i",
        "pipe:0",
        "-an",
    ]);
    match config.encoder {
        Encoder::Software => {
            command.args([
                "-c:v",
                "libx264",
                "-preset",
                "ultrafast",
                "-tune",
                "zerolatency",
                "-b:v",
                "3M",
            ]);
        }
        Encoder::AppleVideoToolbox => {
            // The Pi V4L2 stateless decoder is deliberately the compatibility
            // floor for game streams. VideoToolbox otherwise defaults to High
            // profile, which software decoders accept but the Pi pipeline can
            // stall on without ever producing a frame.
            let filter = format!("{scale},format=nv12");
            command.args(["-vf", &filter]);
            command.args([
                "-c:v",
                "h264_videotoolbox",
                "-profile:v",
                "baseline",
                "-level:v",
                "3.1",
                "-realtime",
                "1",
                "-b:v",
                "5M",
            ]);
        }
        Encoder::NvidiaNvenc => {
            command.args(["-vf", &scale]);
            command.args([
                "-c:v",
                "h264_nvenc",
                "-preset",
                "p1",
                "-tune",
                "ull",
                "-b:v",
                "5M",
            ]);
        }
        Encoder::Vaapi => {
            let filter = format!("{scale},format=nv12,hwupload");
            command.args([
                "-vaapi_device",
                "/dev/dri/renderD128",
                "-vf",
                &filter,
                "-c:v",
                "h264_vaapi",
                "-b:v",
                "5M",
            ]);
        }
    }
    let mut child = command
        .args([
            "-g",
            "30",
            "-bf",
            "0",
            "-mpegts_flags",
            "+resend_headers",
            "-muxdelay",
            "0",
            "-muxpreload",
            "0",
            "-flush_packets",
            "1",
            "-f",
            "mpegts",
            &config.stream_endpoint,
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .with_context(|| format!("spawn {}", config.ffmpeg_path.display()))?;
    let stdin = child.stdin.take().context("FFmpeg stdin unavailable")?;
    Ok((child, stdin))
}

fn spawn_input(
    config: ClientConfig,
    session_id: String,
    game_name: Option<String>,
    pads: Arc<Mutex<Vec<Pad>>>,
    mut stream: VideoStream,
) {
    thread::Builder::new()
        .name("unibus-game-input".into())
        .spawn(move || {
            let result = (|| -> Result<()> {
                let client = Arc::new(Client::connect(config)?);
                let announcer = client.clone();
                let registry_session = session_id.clone();
                let registry_game = game_name.unwrap_or_else(|| session_id.clone());
                thread::Builder::new()
                    .name("unibus-game-video-announce".into())
                    .spawn(move || {
                        loop {
                            let result = (|| -> Result<()> {
                                let now = now_ms();
                                stream.announced_at_unix_ms = now;
                                stream.valid_until_unix_ms = now + 15_000;
                                stream.validate().map_err(anyhow::Error::msg)?;
                                let body = serde_json::to_value(&stream)?
                                    .as_object()
                                    .cloned()
                                    .context("video stream did not encode as an object")?;
                                let envelope = announcer.envelope(
                                    None,
                                    Message::new(
                                        VIDEO_STREAM_CONTRACT,
                                        serde_json::Value::Object(body),
                                    ),
                                )?;
                                announcer.send(&envelope, &[])?;
                                let registry = serde_json::json!({
                                    "session_id": &registry_session,
                                    "game": &registry_game,
                                    "state": "running",
                                    "stream_endpoint": &stream.endpoint,
                                    "transport_role": format!("{:?}", stream.transport_role).to_lowercase(),
                                    "codec": "h264",
                                    "announced_at_unix_ms": now,
                                    "valid_until_unix_ms": now + 15_000,
                                });
                                let registry_envelope = announcer.envelope(
                                    None,
                                    Message::new(GAME_SESSION_CONTRACT, registry),
                                )?;
                                announcer.send(&registry_envelope, &[])
                            })();
                            if let Err(error) = result {
                                eprintln!(
                                    "libretro-session: video announcement stopped: {error:#}"
                                );
                                break;
                            }
                            thread::sleep(Duration::from_secs(5));
                        }
                    })?;
                loop {
                    let Some((envelope, payload)) = client.try_read()? else {
                        continue;
                    };
                    if envelope.message.kind != GAMEPAD_STATE_CONTRACT {
                        continue;
                    }
                    ensure!(payload.is_empty(), "gamepad state carried a binary payload");
                    let state: GamepadState =
                        serde_json::from_value(serde_json::Value::Object(envelope.message.body))?;
                    state.validate().map_err(anyhow::Error::msg)?;
                    if state.session_id != session_id {
                        continue;
                    }
                    if state.buttons != 0 {
                        eprintln!(
                            "libretro-session: input player={} sequence={} buttons={:#06x}",
                            state.player_slot, state.sequence, state.buttons
                        );
                    }
                    let index = usize::from(state.player_slot - 1);
                    let mut guard = pads.lock().unwrap();
                    if let Some(pad) = guard.get_mut(index) {
                        pad.pressed |= state.buttons & !pad.buttons;
                        pad.buttons = state.buttons;
                        pad.axes = state.axes;
                        pad.valid_until_ms = state.valid_until_unix_ms;
                    }
                }
            })();
            if let Err(error) = result {
                eprintln!("libretro-session: input stopped: {error:#}");
            }
        })
        .expect("spawn input reader");
}

#[cfg(unix)]
fn spawn_local_input(path: PathBuf, pads: Arc<Mutex<Vec<Pad>>>) -> Result<()> {
    if path.exists() {
        ensure!(
            path.symlink_metadata()?.file_type().is_socket(),
            "refusing to replace non-socket path {}",
            path.display()
        );
        std::fs::remove_file(&path)?;
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let socket = UnixDatagram::bind(&path)
        .with_context(|| format!("bind standalone controller {}", path.display()))?;
    let socket_path = path.clone();
    thread::Builder::new()
        .name("libretro-local-input".into())
        .spawn(move || {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct LocalReport {
                sequence: u64,
                buttons: u32,
                #[serde(default)]
                axes: unibus_input::GamepadAxes,
            }

            let _socket_guard = LocalInputSocketGuard(socket_path);
            let mut bytes = [0_u8; 8192];
            loop {
                let result = (|| -> Result<()> {
                    let count = socket.recv(&mut bytes)?;
                    let report: LocalReport = serde_json::from_slice(&bytes[..count])?;
                    let _ = report.sequence;
                    ensure!(
                        report.buttons & !button::ALL == 0,
                        "unknown gamepad buttons"
                    );
                    let mut guard = pads.lock().unwrap();
                    let pad = &mut guard[0];
                    pad.pressed |= report.buttons & !pad.buttons;
                    pad.buttons = report.buttons;
                    pad.axes = report.axes;
                    pad.valid_until_ms = now_ms() + 250;
                    Ok(())
                })();
                if let Err(error) = result {
                    eprintln!("libretro-session: standalone input error: {error:#}");
                }
            }
        })?;
    Ok(())
}

fn start_local_player(config: &Config) -> Result<()> {
    let ffplay = config
        .ffplay_path
        .as_ref()
        .context("ffplay_path is required")?;
    let child = Command::new(ffplay)
        .args([
            "-loglevel",
            "warning",
            "-fflags",
            "nobuffer",
            "-flags",
            "low_delay",
            "-framedrop",
            "-an",
            "-window_title",
            "Speedy libretro",
            "-i",
            &config.stream_endpoint,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .with_context(|| format!("spawn {}", ffplay.display()))?;
    std::mem::forget(child);
    thread::sleep(Duration::from_millis(250));
    Ok(())
}

unsafe extern "C" fn environment(command: u32, data: *mut c_void) -> bool {
    let runtime = RUNTIME.get().expect("runtime initialized");
    match command {
        RETRO_ENVIRONMENT_GET_CAN_DUPE => {
            unsafe { *(data as *mut bool) = true };
            true
        }
        RETRO_ENVIRONMENT_SET_SUPPORT_NO_GAME => true,
        RETRO_ENVIRONMENT_SET_PIXEL_FORMAT => {
            let value = unsafe { *(data as *const u32) };
            if !matches!(value, PIXEL_0RGB1555 | PIXEL_XRGB8888 | PIXEL_RGB565) {
                return false;
            }
            runtime.lock().unwrap().pixel_format = value;
            true
        }
        RETRO_ENVIRONMENT_GET_SYSTEM_DIRECTORY => {
            unsafe { *(data as *mut *const c_char) = runtime.lock().unwrap().system_dir.as_ptr() };
            true
        }
        RETRO_ENVIRONMENT_GET_SAVE_DIRECTORY => {
            unsafe { *(data as *mut *const c_char) = runtime.lock().unwrap().save_dir.as_ptr() };
            true
        }
        _ => false,
    }
}

unsafe extern "C" fn video_refresh(data: *const c_void, width: u32, height: u32, pitch: usize) {
    if data.is_null() {
        return;
    }
    let mut state = RUNTIME.get().expect("runtime initialized").lock().unwrap();
    if !state.logged_frame {
        eprintln!(
            "libretro-session: first frame={}x{} pitch={} pixel_format={}",
            width, height, pitch, state.pixel_format
        );
        state.logged_frame = true;
    }
    if width != state.width || height != state.height {
        state.encoder_error = Some(format!("dynamic geometry {width}x{height} is unsupported"));
        return;
    }
    let bytes_per_pixel = if state.pixel_format == PIXEL_XRGB8888 {
        4
    } else {
        2
    };
    let input = unsafe { std::slice::from_raw_parts(data.cast::<u8>(), pitch * height as usize) };
    let mut rgba = vec![0_u8; width as usize * height as usize * 4];
    for y in 0..height as usize {
        for x in 0..width as usize {
            let src = y * pitch + x * bytes_per_pixel;
            let dst = (y * width as usize + x) * 4;
            let (r, g, b) = match state.pixel_format {
                PIXEL_XRGB8888 => (input[src + 2], input[src + 1], input[src]),
                PIXEL_RGB565 => {
                    let v = u16::from_ne_bytes([input[src], input[src + 1]]);
                    (
                        (((v >> 11) & 31) * 255 / 31) as u8,
                        (((v >> 5) & 63) * 255 / 63) as u8,
                        ((v & 31) * 255 / 31) as u8,
                    )
                }
                _ => {
                    let v = u16::from_ne_bytes([input[src], input[src + 1]]);
                    (
                        (((v >> 10) & 31) * 255 / 31) as u8,
                        (((v >> 5) & 31) * 255 / 31) as u8,
                        ((v & 31) * 255 / 31) as u8,
                    )
                }
            };
            rgba[dst..dst + 4].copy_from_slice(&[r, g, b, 255]);
        }
    }
    if let Some(encoder) = state.encoder.as_mut() {
        if let Err(error) = encoder.write_all(&rgba) {
            state.encoder_error = Some(error.to_string());
        }
    }
}

unsafe extern "C" fn audio_sample(_left: i16, _right: i16) {}
unsafe extern "C" fn audio_sample_batch(_data: *const i16, frames: usize) -> usize {
    frames
}
unsafe extern "C" fn input_poll() {}

unsafe extern "C" fn input_state(port: u32, device: u32, _index: u32, id: u32) -> i16 {
    let mut pads = PADS.get().expect("pads initialized").lock().unwrap();
    let Some(pad) = pads.get_mut(port as usize) else {
        return 0;
    };
    if pad.valid_until_ms <= now_ms() {
        return 0;
    }
    if device == RETRO_DEVICE_ANALOG {
        return match (_index, id) {
            (0, 0) => pad.axes.left_x,
            (0, 1) => pad.axes.left_y,
            (1, 0) => pad.axes.right_x,
            (1, 1) => pad.axes.right_y,
            _ => 0,
        };
    }
    if device != RETRO_DEVICE_JOYPAD {
        return 0;
    }
    let mask = match id {
        0 => button::SOUTH,
        1 => button::WEST,
        2 => button::SELECT,
        3 => button::START,
        4 => button::DPAD_UP,
        5 => button::DPAD_DOWN,
        6 => button::DPAD_LEFT,
        7 => button::DPAD_RIGHT,
        8 => button::EAST,
        9 => button::NORTH,
        10 => button::LEFT_SHOULDER,
        11 => button::RIGHT_SHOULDER,
        12 => return i16::from(pad.axes.left_trigger > u16::MAX / 2),
        13 => return i16::from(pad.axes.right_trigger > u16::MAX / 2),
        14 => button::LEFT_THUMB,
        15 => button::RIGHT_THUMB,
        _ => return 0,
    };
    let active = (pad.buttons | pad.pressed) & mask != 0;
    pad.pressed &= !mask;
    i16::from(active)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
fn hex_digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn digest_file(path: &Path) -> Result<String> {
    let mut file = File::open(path)?;
    let mut hash = Sha256::new();
    let mut buf = [0_u8; 65536];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hash.update(&buf[..n]);
    }
    Ok(format!("{:x}", hash.finalize()))
}
fn path_cstring(path: &Path) -> Result<std::ffi::CString> {
    Ok(std::ffi::CString::new(path.as_os_str().as_encoded_bytes())?)
}
