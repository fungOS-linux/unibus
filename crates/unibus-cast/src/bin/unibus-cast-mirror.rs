//! Single paired AirPlay mirror receiver to a bounded private HTTP MPEG-TS feed.
use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    process::{Child, Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
        mpsc::{self, SyncSender},
    },
    thread,
    time::{Duration, Instant},
};
use unibus_cast::runtime::{MirrorConfig, allowed_request};
use unibus_cast::status::{MirrorPhase, MirrorStatus};
type Subscribers = Arc<Mutex<Fanout>>;
#[derive(Default)]
struct Fanout {
    clients: Vec<(SyncSender<Arc<Vec<u8>>>, bool, u64)>,
    next_client_id: u64,
    logged_tables: bool,
    last_ra_log: Option<Instant>,
    last_state_log: Option<Instant>,
    pat: Option<Vec<u8>>,
    pmt: Option<Vec<u8>>,
    pmt_pid: Option<u16>,
    video_pid: Option<u16>,
    cache: Vec<u8>,
    bootstrap_ready: bool,
    nal_tail: Vec<u8>,
    seen_sps: bool,
    seen_pps: bool,
    logged_nals: u32,
    last_video: Option<Instant>,
    normalize_video: bool,
    process_live: bool,
}
impl Fanout {
    fn expire_video(&mut self, now: Instant) {
        if self
            .last_video
            .is_none_or(|last| now.saturating_duration_since(last) >= Duration::from_secs(5))
        {
            self.cache.clear();
            self.bootstrap_ready = false;
            self.seen_sps = false;
            self.seen_pps = false;
            self.nal_tail.clear();
        }
    }
    fn status(&mut self, now: Instant) -> MirrorStatus {
        self.expire_video(now);
        let video_age_ms = self.last_video.map(|last| {
            now.saturating_duration_since(last)
                .as_millis()
                .min(u128::from(u64::MAX)) as u64
        });
        let source_live = video_age_ms.is_some_and(|age| age < 5000);
        MirrorStatus {
            schema_version: 1,
            phase: if !source_live {
                MirrorPhase::WaitingSource
            } else if !self.bootstrap_ready {
                MirrorPhase::WaitingBootstrap
            } else {
                MirrorPhase::Streaming
            },
            process_live: self.process_live,
            source_live,
            video_age_ms,
            active_viewers: self
                .clients
                .iter()
                .filter(|(_, started, _)| *started)
                .count(),
            waiting_viewers: self
                .clients
                .iter()
                .filter(|(_, started, _)| !started)
                .count(),
            bootstrap_ready: self.bootstrap_ready,
            cache_bytes: self.cache.len(),
            normalize_video: self.normalize_video,
        }
    }
    fn ingest(&mut self, bytes: &[u8]) {
        self.ingest_at(bytes, Instant::now());
    }
    fn ingest_at(&mut self, bytes: &[u8], now: Instant) {
        self.expire_video(now);
        for packet in bytes.chunks_exact(188) {
            if packet[0] != 0x47 {
                self.cache.clear();
                self.bootstrap_ready = false;
                continue;
            }
            let pid = (u16::from(packet[1] & 0x1f) << 8) | u16::from(packet[2]);
            let payload_start = packet[1] & 0x40 != 0;
            let adaptation = (packet[3] >> 4) & 3;
            let offset = if adaptation & 2 != 0 {
                5 + usize::from(packet[4])
            } else {
                4
            };
            if Some(pid) == self.video_pid && adaptation & 1 != 0 && offset < 188 {
                self.last_video = Some(now);
            }
            if payload_start && adaptation & 1 != 0 && offset < 188 {
                let section = offset + 1 + usize::from(packet[offset]);
                if section + 12 <= 188 {
                    let end = section
                        + 3
                        + ((usize::from(packet[section + 1] & 15) << 8)
                            | usize::from(packet[section + 2]));
                    if end <= 188 && end >= section + 12 {
                        if pid == 0 && packet[section] == 0 {
                            for program in packet[section + 8..end - 4].chunks_exact(4) {
                                if program[0] != 0 || program[1] != 0 {
                                    self.pmt_pid = Some(
                                        (u16::from(program[2] & 31) << 8) | u16::from(program[3]),
                                    );
                                    self.pat = Some(packet.to_vec());
                                    break;
                                }
                            }
                        } else if Some(pid) == self.pmt_pid && packet[section] == 2 {
                            let info = (usize::from(packet[section + 10] & 15) << 8)
                                | usize::from(packet[section + 11]);
                            let mut stream = section + 12 + info;
                            while stream + 5 <= end - 4 {
                                if packet[stream] == 0x1b {
                                    self.video_pid = Some(
                                        (u16::from(packet[stream + 1] & 31) << 8)
                                            | u16::from(packet[stream + 2]),
                                    );
                                    self.pmt = Some(packet.to_vec());
                                    break;
                                }
                                stream += 5
                                    + ((usize::from(packet[stream + 3] & 15) << 8)
                                        | usize::from(packet[stream + 4]));
                            }
                        }
                    }
                }
            }
            if !self.logged_tables && self.pat.is_some() && self.pmt.is_some() {
                eprintln!(
                    "mirror tables ready: PMT PID={:?} H264 PID={:?}",
                    self.pmt_pid, self.video_pid
                );
                self.logged_tables = true;
            }
            let random_access = Some(pid) == self.video_pid
                && payload_start
                && adaptation & 2 != 0
                && packet[4] > 0
                && packet[5] & 0x40 != 0;
            // Some real Mac streams omit TS random_access_indicator. Keep a
            // candidate from a video PES boundary and gate it on actual NALs.
            let video_start = Some(pid) == self.video_pid && payload_start;
            if random_access || (video_start && !self.bootstrap_ready && !self.seen_sps) {
                self.cache.clear();
                self.bootstrap_ready = false;
                self.seen_sps = false;
                self.seen_pps = false;
                self.nal_tail.clear();
                if let (Some(pat), Some(pmt)) = (&self.pat, &self.pmt) {
                    self.cache.extend_from_slice(pat);
                    self.cache.extend_from_slice(pmt);
                    if self
                        .last_ra_log
                        .is_none_or(|last| last.elapsed() >= Duration::from_secs(5))
                    {
                        eprintln!("mirror video random access: bootstrap initialized PID={pid}");
                        self.last_ra_log = Some(Instant::now());
                    }
                }
            }
            if Some(pid) == self.video_pid && adaptation & 1 != 0 && offset < 188 {
                let mut elementary = offset;
                if payload_start {
                    if offset + 9 <= 188 && packet[offset..offset + 3] == [0, 0, 1] {
                        elementary = offset + 9 + usize::from(packet[offset + 8]);
                    } else {
                        elementary = 188;
                    }
                }
                if elementary < 188 {
                    let mut scan = std::mem::take(&mut self.nal_tail);
                    scan.extend_from_slice(&packet[elementary..]);
                    for i in 0..scan.len().saturating_sub(3) {
                        if scan[i..i + 3] == [0, 0, 1] {
                            let nal = scan[i + 3] & 31;
                            if self.logged_nals & (1 << nal) == 0 {
                                eprintln!("mirror H264 observed NAL type={nal}");
                                self.logged_nals |= 1 << nal;
                            }
                            match nal {
                                7 => self.seen_sps = true,
                                8 if self.seen_sps => self.seen_pps = true,
                                5 if self.seen_sps && self.seen_pps && !self.cache.is_empty() => {
                                    if !self.bootstrap_ready {
                                        eprintln!("mirror AnnexB SPS/PPS/IDR bootstrap ready");
                                    }
                                    self.bootstrap_ready = true;
                                }
                                _ => {}
                            }
                        }
                    }
                    self.nal_tail = scan[scan.len().saturating_sub(3)..].to_vec();
                }
            }
            // Null padding feeds Android's byte cache, not the replay budget.
            if !self.cache.is_empty() && pid != 0x1fff {
                if self.cache.len() + 188 > 2 * 1024 * 1024 {
                    self.cache.clear();
                    self.bootstrap_ready = false;
                    self.seen_sps = false;
                    self.seen_pps = false;
                } else {
                    self.cache.extend_from_slice(packet);
                }
            }
        }
        let live = Arc::new(bytes.to_vec());
        let bootstrap = (self.bootstrap_ready
            && !self.cache.is_empty()
            && self.clients.iter().any(|(_, started, _)| !started))
        .then(|| Arc::new(self.cache.clone()));
        self.clients.retain_mut(|(client, started, _)| {
            if *started {
                client.try_send(live.clone()).is_ok()
            } else if let Some(bootstrap) = &bootstrap {
                *started = true;
                client.try_send(bootstrap.clone()).is_ok()
            } else {
                true
            }
        });
        if self
            .last_state_log
            .is_none_or(|last| last.elapsed() >= Duration::from_secs(10))
        {
            eprintln!(
                "mirror mux: H264 PID={:?} bootstrap_bytes={} bootstrap_ready={} viewers={} waiting={}",
                self.video_pid,
                self.cache.len(),
                self.bootstrap_ready,
                self.clients.len(),
                self.clients
                    .iter()
                    .filter(|(_, started, _)| !started)
                    .count()
            );
            self.last_state_log = Some(Instant::now());
        }
    }
}
struct Backend(Child);
struct Subscription {
    clients: Subscribers,
    id: u64,
}
impl Drop for Subscription {
    fn drop(&mut self) {
        self.clients
            .lock()
            .unwrap()
            .clients
            .retain(|(_, _, id)| *id != self.id);
    }
}
impl Drop for Backend {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn subscriber(mut stream: TcpStream, clients: Subscribers) -> std::io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
    let mut request = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(2);
    // Bounded header parsing; no body, persistent requests or control mutations.
    while request.len() < 4096 && !request.ends_with(b"\r\n\r\n") {
        if Instant::now() >= deadline {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "header deadline",
            ));
        }
        let mut byte = [0];
        stream.read_exact(&mut byte)?;
        request.push(byte[0]);
    }
    let status_request = request.len() <= 4096
        && request.ends_with(b"\r\n\r\n")
        && matches!(
            request.split(|byte| *byte == b'\n').next(),
            Some(b"GET /status HTTP/1.1\r" | b"GET /status HTTP/1.0\r")
        );
    if status_request {
        let status = clients.lock().unwrap().status(Instant::now());
        let bytes = serde_json::to_vec(&status)?;
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nCache-Control: no-store\r\nConnection: close\r\nContent-Length: {}\r\n\r\n",
            bytes.len()
        )?;
        stream.write_all(&bytes)?;
        return Ok(());
    }
    if !allowed_request(&request) {
        stream.write_all(
            b"HTTP/1.1 400 Bad Request\r\nConnection: close\r\nContent-Length: 0\r\n\r\n",
        )?;
        return Ok(());
    }
    // One <=2MiB startup GOP, then <=512 * 1316 live bytes (~0.67s at 8Mbps).
    let (tx, rx) = mpsc::sync_channel(512);
    let subscription_clients = clients.clone();
    let _subscription = {
        let mut clients = clients.lock().unwrap();
        clients.expire_video(Instant::now());
        if clients.clients.len() >= 4 {
            stream.write_all(b"HTTP/1.1 503 Service Unavailable\r\nConnection: close\r\nContent-Length: 0\r\n\r\n")?;
            return Ok(());
        }
        let started = clients.bootstrap_ready && !clients.cache.is_empty();
        if started {
            let _ = tx.try_send(Arc::new(clients.cache.clone()));
        }
        let id = clients.next_client_id;
        clients.next_client_id = clients.next_client_id.wrapping_add(1);
        clients.clients.push((tx, started, id));
        Subscription {
            clients: subscription_clients,
            id,
        }
    };
    stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: video/mp2t\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n")?;
    loop {
        match rx.recv_timeout(Duration::from_secs(1)) {
            Ok(chunk) => stream.write_all(&chunk)?,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                stream.set_read_timeout(Some(Duration::from_millis(50)))?;
                match stream.peek(&mut [0_u8]) {
                    Ok(0) => break,
                    Ok(_) => break, // No pipelined requests or request bodies.
                    Err(error)
                        if matches!(
                            error.kind(),
                            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                        ) =>
                    {
                        continue;
                    }
                    Err(error) => return Err(error),
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args()
        .nth(1)
        .ok_or("usage: unibus-cast-mirror CONFIG.json")?;
    let mut bytes = Vec::new();
    std::fs::File::open(path)?
        .take(16_385)
        .read_to_end(&mut bytes)?;
    if bytes.len() > 16_384 {
        return Err("config exceeds 16 KiB".into());
    }
    let config: MirrorConfig = serde_json::from_slice(&bytes)?;
    config.validate()?;
    let listener = TcpListener::bind(config.listen)?;
    listener.set_nonblocking(true)?;
    let mut gst = Backend(
        Command::new("gst-launch-1.0")
            .args(config.gstreamer_args()?)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()?,
    );
    let mut output = gst.0.stdout.take().ok_or("backend stdout missing")?;
    let mut receiver = if config.receiver_enabled {
        Some(Backend(
            Command::new("uxplay")
                .args(config.uxplay_args()?)
                .stdin(Stdio::null())
                .stdout(Stdio::inherit())
                .stderr(Stdio::inherit())
                .spawn()?,
        ))
    } else {
        None
    };
    let clients: Subscribers = Arc::new(Mutex::new(Fanout {
        normalize_video: config.normalize_video,
        process_live: true,
        ..Fanout::default()
    }));
    let workers = Arc::new(AtomicUsize::new(0));
    let fanout = clients.clone();
    let (failed_tx, failed_rx) = mpsc::channel();
    thread::spawn(move || {
        loop {
            let mut bytes = vec![0; 1316];
            if let Err(error) = output.read_exact(&mut bytes) {
                let _ = failed_tx.send(error);
                break;
            }
            // Disconnect lagging viewers; never drop arbitrary TS packets inside a stream.
            fanout.lock().unwrap().ingest(&bytes);
        }
    });
    eprintln!(
        "AirPlay mirror HTTP ready: http://{}/live.ts; PIN displayed by UxPlay; sync unverified",
        config.listen
    );
    loop {
        if let Some(status) = gst.0.try_wait()? {
            return Err(format!("GStreamer exited: {status}").into());
        }
        if let Some(receiver) = &mut receiver {
            if let Some(status) = receiver.0.try_wait()? {
                return Err(format!("UxPlay exited: {status}").into());
            }
        }
        if let Ok(error) = failed_rx.try_recv() {
            return Err(format!("media pipe failed: {error}").into());
        }
        match listener.accept() {
            Ok((stream, peer)) => {
                if !config
                    .allowed_clients
                    .iter()
                    .any(|ip| peer.ip() == std::net::IpAddr::V4(*ip))
                {
                    drop(stream);
                    continue;
                }
                let clients = clients.clone();
                // Bound simultaneous pending header parsers as well as live subscribers.
                // Reserve a fifth bounded worker for status while four viewers
                // are live. A fifth stream still receives 503 in subscriber.
                if workers.fetch_add(1, Ordering::SeqCst) >= 5 {
                    workers.fetch_sub(1, Ordering::SeqCst);
                    drop(stream);
                    continue;
                }
                let workers = workers.clone();
                thread::spawn(move || {
                    if let Err(error) = subscriber(stream, clients) {
                        eprintln!("viewer disconnected: {error}");
                    }
                    workers.fetch_sub(1, Ordering::SeqCst);
                });
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(20))
            }
            Err(error) => return Err(error.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn stale_source_clears_bootstrap_without_disconnecting_viewers() {
        let now = Instant::now();
        let (tx, _rx) = mpsc::sync_channel(32);
        let mut state = Fanout {
            last_video: Some(now),
            bootstrap_ready: true,
            cache: vec![0x47; 188],
            process_live: true,
            normalize_video: true,
            clients: vec![(tx, true, 0)],
            ..Fanout::default()
        };
        let fresh = state.status(now + Duration::from_millis(4999));
        assert!(fresh.source_live && fresh.bootstrap_ready && fresh.process_live);
        assert_eq!(fresh.phase, MirrorPhase::Streaming);
        let stale = state.status(now + Duration::from_secs(5));
        assert!(!stale.source_live && !stale.bootstrap_ready);
        assert_eq!(stale.phase, MirrorPhase::WaitingSource);
        assert_eq!(stale.video_age_ms, Some(5000));
        assert_eq!(stale.cache_bytes, 0);
        assert_eq!(stale.active_viewers, 1);
        assert!(stale.process_live && stale.normalize_video);
        let mut audio = vec![0xff; 188];
        audio[..4].copy_from_slice(&[0x47, 0, 66, 0x10]);
        state.ingest_at(&audio, now + Duration::from_millis(5500));
        assert_eq!(
            state.status(now + Duration::from_millis(5500)).cache_bytes,
            0
        );
        assert!(!state.status(now + Duration::from_millis(5500)).source_live);
        let mut video = vec![0xff; 188];
        video[..4].copy_from_slice(&[0x47, 0, 65, 0x10]);
        state.video_pid = Some(65);
        state.ingest_at(&video, now + Duration::from_secs(6));
        let resumed = state.status(now + Duration::from_secs(6));
        assert!(resumed.source_live && !resumed.bootstrap_ready);
        assert_eq!(resumed.phase, MirrorPhase::WaitingBootstrap);
    }
    #[test]
    fn status_http_is_typed_and_does_not_register_a_viewer() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let clients: Subscribers = Arc::new(Mutex::new(Fanout::default()));
        let server_state = clients.clone();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || subscriber(listener.accept().unwrap().0, server_state));
        let mut viewer = TcpStream::connect(address).unwrap();
        viewer
            .write_all(b"GET /status HTTP/1.1\r\nHost: test\r\n\r\n")
            .unwrap();
        let mut bytes = Vec::new();
        viewer.read_to_end(&mut bytes).unwrap();
        let offset = bytes
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .unwrap()
            + 4;
        let status: MirrorStatus = serde_json::from_slice(&bytes[offset..]).unwrap();
        assert_eq!(status.phase, MirrorPhase::WaitingSource);
        assert_eq!(status.active_viewers, 0);
        assert!(!status.source_live);
        assert!(clients.lock().unwrap().clients.is_empty());
        server.join().unwrap().unwrap();
    }
    #[test]
    fn idle_viewer_eof_releases_subscription_without_media() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let clients: Subscribers = Arc::new(Mutex::new(Fanout::default()));
        let state = clients.clone();
        let mut viewer = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (done_tx, done_rx) = mpsc::channel();
        thread::spawn(move || {
            let result = subscriber(listener.accept().unwrap().0, state);
            done_tx.send(result).unwrap();
        });
        viewer
            .write_all(b"GET /live.ts HTTP/1.1\r\nHost: test\r\n\r\n")
            .unwrap();
        let mut header = Vec::new();
        while !header.ends_with(b"\r\n\r\n") {
            let mut byte = [0];
            viewer.read_exact(&mut byte).unwrap();
            header.push(byte[0]);
        }
        assert_eq!(clients.lock().unwrap().clients.len(), 1);
        drop(viewer);
        done_rx
            .recv_timeout(Duration::from_secs(3))
            .unwrap()
            .unwrap();
        assert!(clients.lock().unwrap().clients.is_empty());
    }
    #[test]
    fn join_waits_for_video_random_access_and_caches_tables_and_gop() {
        let mut fanout = Fanout::default();
        let (tx, rx) = mpsc::sync_channel(32);
        fanout.clients.push((tx, false, 0));
        let mut audio = vec![0xff; 188];
        audio[0] = 0x47;
        audio[1] = 1;
        audio[2] = 2;
        audio[3] = 0x10;
        fanout.ingest(&audio);
        assert!(rx.try_recv().is_err());
        let mut pat = vec![0xff; 188];
        pat[..5].copy_from_slice(&[0x47, 0x40, 0, 0x10, 0]);
        pat[5..21].copy_from_slice(&[0, 0xb0, 13, 0, 1, 0xc1, 0, 0, 0, 1, 0xe1, 0, 0, 0, 0, 0]);
        let mut pmt = vec![0xff; 188];
        pmt[..5].copy_from_slice(&[0x47, 0x41, 0, 0x10, 0]);
        pmt[5..26].copy_from_slice(&[
            2, 0xb0, 18, 0, 1, 0xc1, 0, 0, 0xe1, 1, 0xf0, 0, 0x1b, 0xe1, 1, 0xf0, 0, 0, 0, 0, 0,
        ]);
        fanout.ingest(&pat);
        fanout.ingest(&pmt);
        assert!(rx.try_recv().is_err());
        let mut video = vec![0xff; 188];
        video[..6].copy_from_slice(&[0x47, 0x41, 1, 0x30, 1, 0x40]);
        video[6..15].copy_from_slice(&[0, 0, 1, 0xe0, 0, 0, 0x80, 0, 0]);
        video[15..27].copy_from_slice(&[0, 0, 1, 0x67, 0, 0, 1, 0x68, 0, 0, 1, 0x65]);
        fanout.ingest(&video);
        let bootstrap = rx.try_recv().unwrap();
        assert_eq!(&bootstrap[..188], &pat);
        assert_eq!(&bootstrap[188..376], &pmt);
        assert_eq!(&bootstrap[376..], &video);
        fanout.ingest(&audio);
        assert_eq!(&*rx.try_recv().unwrap(), &audio);
        assert_eq!(fanout.cache.len(), 752);
        for _ in 0..12_000 {
            fanout.ingest(&audio);
        }
        assert!(fanout.cache.len() <= 2 * 1024 * 1024);
        assert!(fanout.cache.is_empty());
    }
    #[test]
    fn annexb_bootstrap_without_ra_and_split_start_code() {
        let mut fanout = Fanout {
            video_pid: Some(65),
            pat: Some(vec![0x47; 188]),
            pmt: Some(vec![0x47; 188]),
            ..Fanout::default()
        };
        let (tx, rx) = mpsc::sync_channel(32);
        fanout.clients.push((tx, false, 0));
        let mut first = vec![0xff; 188];
        first[..4].copy_from_slice(&[0x47, 0x40, 65, 0x10]);
        first[4..13].copy_from_slice(&[0, 0, 1, 0xe0, 0, 0, 0x80, 0, 0]);
        first[13..21].copy_from_slice(&[0, 0, 1, 0x67, 0, 0, 1, 0x68]);
        first[185..].copy_from_slice(&[0, 0, 1]);
        fanout.ingest(&first);
        assert!(rx.try_recv().is_err());
        let mut next = vec![0xff; 188];
        next[..5].copy_from_slice(&[0x47, 0, 65, 0x11, 0x65]);
        fanout.ingest(&next);
        let startup = rx.try_recv().unwrap();
        assert_eq!(startup.len(), 188 * 4);
        assert!(fanout.bootstrap_ready);
    }
    #[test]
    fn http_streams_mux_bytes_and_rejects_other_paths() {
        for (path, accepted) in [("/live.ts", true), ("/control", false)] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let address = listener.local_addr().unwrap();
            let clients: Subscribers = Arc::new(Mutex::new(Fanout::default()));
            let server_clients = clients.clone();
            let server =
                thread::spawn(move || subscriber(listener.accept().unwrap().0, server_clients));
            let mut viewer = TcpStream::connect(address).unwrap();
            viewer
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            write!(viewer, "GET {path} HTTP/1.1\r\nHost: test\r\n\r\n").unwrap();
            let mut header = Vec::new();
            while !header.ends_with(b"\r\n\r\n") {
                let mut byte = [0];
                viewer.read_exact(&mut byte).unwrap();
                header.push(byte[0]);
            }
            if accepted {
                assert!(header.starts_with(b"HTTP/1.1 200"));
                clients.lock().unwrap().clients[0]
                    .0
                    .send(Arc::new(vec![0x47; 1316]))
                    .unwrap();
                let mut bytes = [0; 1316];
                viewer.read_exact(&mut bytes).unwrap();
                assert_eq!(bytes, [0x47; 1316]);
                clients.lock().unwrap().clients.clear();
            } else {
                assert!(header.starts_with(b"HTTP/1.1 400"));
            }
            server.join().unwrap().unwrap();
        }
    }
    #[test]
    fn bounded_fanout_disconnects_slow_consumer_instead_of_dropping_packets() {
        let (tx, _rx) = mpsc::sync_channel(1);
        let mut viewers = vec![tx];
        let chunk = Arc::new(vec![0x47; 1316]);
        viewers.retain(|viewer| viewer.try_send(chunk.clone()).is_ok());
        assert_eq!(viewers.len(), 1);
        viewers.retain(|viewer| viewer.try_send(chunk.clone()).is_ok());
        assert!(viewers.is_empty());
    }
}
