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
type Subscribers = Arc<Mutex<Vec<SyncSender<Arc<Vec<u8>>>>>>;
struct Backend(Child);
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
    if !allowed_request(&request) {
        stream.write_all(
            b"HTTP/1.1 400 Bad Request\r\nConnection: close\r\nContent-Length: 0\r\n\r\n",
        )?;
        return Ok(());
    }
    let (tx, rx) = mpsc::sync_channel(32); // <= 32 * 1316 = 42 KiB per viewer.
    {
        let mut clients = clients.lock().unwrap();
        if clients.len() >= 4 {
            stream.write_all(b"HTTP/1.1 503 Service Unavailable\r\nConnection: close\r\nContent-Length: 0\r\n\r\n")?;
            return Ok(());
        }
        clients.push(tx);
    }
    stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: video/mp2t\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n")?;
    loop {
        match rx.recv_timeout(Duration::from_secs(1)) {
            Ok(chunk) => stream.write_all(&chunk)?,
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
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
    let mut receiver = Backend(
        Command::new("uxplay")
            .args(config.uxplay_args()?)
            .stdin(Stdio::null())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .spawn()?,
    );
    let clients: Subscribers = Arc::new(Mutex::new(Vec::new()));
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
            let bytes = Arc::new(bytes);
            // Disconnect lagging viewers; never drop arbitrary TS packets inside a stream.
            fanout
                .lock()
                .unwrap()
                .retain(|client| client.try_send(bytes.clone()).is_ok());
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
        if let Some(status) = receiver.0.try_wait()? {
            return Err(format!("UxPlay exited: {status}").into());
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
                if workers.fetch_add(1, Ordering::SeqCst) >= 4 {
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
    fn http_streams_mux_bytes_and_rejects_other_paths() {
        for (path, accepted) in [("/live.ts", true), ("/control", false)] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let address = listener.local_addr().unwrap();
            let clients: Subscribers = Arc::new(Mutex::new(Vec::new()));
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
                clients.lock().unwrap()[0]
                    .send(Arc::new(vec![0x47; 1316]))
                    .unwrap();
                let mut bytes = [0; 1316];
                viewer.read_exact(&mut bytes).unwrap();
                assert_eq!(bytes, [0x47; 1316]);
                clients.lock().unwrap().clear();
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
