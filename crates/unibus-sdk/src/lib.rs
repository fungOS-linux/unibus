//! Bounded, credential-safe control-plane client for embedded hosts.
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{self, Receiver, SyncSender},
    },
    thread::{self, JoinHandle},
};
use unibus::{
    Message,
    client::{Client, ClientConfig},
};

pub const MAX_BODY_BYTES: usize = 64 * 1024;
pub const QUEUE_CAPACITY: usize = 128;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Publish {
    pub destination: Option<String>,
    pub kind: String,
    pub body: Value,
}

/// Deliberately not an Envelope: credentials cannot cross the host boundary.
#[derive(Debug, Serialize)]
pub struct Event {
    pub producer_id: String,
    pub request_id: String,
    pub issued_at_unix_ms: u64,
    pub kind: String,
    pub body: Value,
}

#[derive(Serialize)]
pub struct Status {
    pub connected: bool,
    pub dropped_events: u64,
}

pub struct Session {
    outbound: SyncSender<(String, Publish)>,
    inbound: Receiver<Event>,
    alive: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    dropped: Arc<AtomicU64>,
    worker: Option<JoinHandle<()>>,
}

impl Publish {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            !self.kind.is_empty() && self.kind.len() <= 128,
            "kind must be 1..128 bytes"
        );
        ensure!(self.body.is_object(), "body must be an object");
        ensure!(
            self.body.get("message_type").is_none(),
            "body must not override message_type"
        );
        ensure!(
            serde_json::to_vec(&self.body)?.len() <= MAX_BODY_BYTES,
            "control body exceeds 64 KiB"
        );
        ensure!(
            self.destination
                .as_ref()
                .is_none_or(|s| !s.is_empty() && s.len() <= 128),
            "invalid destination"
        );
        Ok(())
    }
}

impl Session {
    /// Registration happens synchronously. Polling never waits for network IO.
    pub fn connect(mut config: ClientConfig) -> Result<Self> {
        config.read_timeout_ms = 10;
        let client = Client::connect(config)?;
        let (outbound, commands) = mpsc::sync_channel::<(String, Publish)>(QUEUE_CAPACITY);
        let (events, inbound) = mpsc::sync_channel(QUEUE_CAPACITY);
        let alive = Arc::new(AtomicBool::new(true));
        let stop = Arc::new(AtomicBool::new(false));
        let dropped = Arc::new(AtomicU64::new(0));
        let (a, s, d) = (alive.clone(), stop.clone(), dropped.clone());
        let worker = thread::spawn(move || {
            while !s.load(Ordering::Acquire) {
                for _ in 0..16 {
                    let Ok((request_id, command)) = commands.try_recv() else {
                        break;
                    };
                    let sent = client
                        .envelope(
                            command.destination.as_deref(),
                            Message::new(&command.kind, command.body),
                        )
                        .and_then(|mut envelope| {
                            envelope.request_id = request_id;
                            client.send(&envelope, &[])
                        });
                    if sent.is_err() {
                        a.store(false, Ordering::Release);
                        return;
                    }
                }
                match client.try_read() {
                    Ok(Some((envelope, payload))) => {
                        // Bulk content belongs to media transports, not this SDK.
                        if !payload.is_empty() {
                            d.fetch_add(1, Ordering::Relaxed);
                            continue;
                        }
                        let event = Event {
                            producer_id: envelope.producer_id,
                            request_id: envelope.request_id,
                            issued_at_unix_ms: envelope.issued_at_unix_ms,
                            kind: envelope.message.kind,
                            body: Value::Object(envelope.message.body),
                        };
                        if serde_json::to_vec(&event).map_or(true, |v| v.len() > MAX_BODY_BYTES) {
                            d.fetch_add(1, Ordering::Relaxed);
                            continue;
                        }
                        if events.try_send(event).is_err() {
                            d.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    Ok(None) => {}
                    Err(_) => break,
                }
            }
            a.store(false, Ordering::Release);
        });
        Ok(Self {
            outbound,
            inbound,
            alive,
            stop,
            dropped,
            worker: Some(worker),
        })
    }
    /// Success means queued, not sent or authorized. Observe router replies.
    pub fn publish(&self, command: Publish) -> Result<String> {
        command.validate()?;
        ensure!(
            self.status().connected,
            "session disconnected; reconnect explicitly"
        );
        let request_id = unibus::envelope::next_request_id();
        self.outbound
            .try_send((request_id.clone(), command))
            .map_err(|_| anyhow::anyhow!("outbound queue full or disconnected"))?;
        Ok(request_id)
    }
    pub fn poll(&self) -> Option<Event> {
        self.inbound.try_recv().ok()
    }
    pub fn status(&self) -> Status {
        Status {
            connected: self.alive.load(Ordering::Acquire),
            dropped_events: self.dropped.load(Ordering::Relaxed),
        }
    }
}
impl Drop for Session {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::Write,
        net::TcpListener,
        time::{Duration, Instant},
    };
    use unibus::{Envelope, framing, kinds};
    #[test]
    fn outbound_backpressure_is_explicit_and_ids_are_unique() {
        let (outbound, commands) = mpsc::sync_channel(QUEUE_CAPACITY);
        let (_events, inbound) = mpsc::sync_channel(QUEUE_CAPACITY);
        let session = Session {
            outbound,
            inbound,
            alive: Arc::new(AtomicBool::new(true)),
            stop: Arc::new(AtomicBool::new(false)),
            dropped: Arc::new(AtomicU64::new(0)),
            worker: None,
        };
        let command = || Publish {
            destination: None,
            kind: "test.event.v1".into(),
            body: serde_json::json!({}),
        };
        let mut ids = std::collections::HashSet::new();
        for _ in 0..QUEUE_CAPACITY {
            assert!(ids.insert(session.publish(command()).unwrap()));
        }
        assert!(
            session
                .publish(command())
                .unwrap_err()
                .to_string()
                .contains("queue full")
        );
        let (id, _) = commands.try_recv().unwrap();
        assert!(ids.contains(&id));
        assert!(session.publish(command()).is_ok());
    }
    #[test]
    fn real_framing_publish_poll_overflow_and_disconnect() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut token = tempfile::NamedTempFile::new().unwrap();
        token.write_all(b"secret-transport-token").unwrap();
        let config: ClientConfig = serde_json::from_value(serde_json::json!({
            "installation_id":"test", "producer_id":"sdk", "hub_endpoint":listener.local_addr().unwrap().to_string(),
            "token_file":token.path(), "zones":["test"], "schema_version":2
        })).unwrap();
        let router = thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let (registration, _) = framing::read_frame(&mut socket).unwrap();
            let reply = Envelope::new(
                "test",
                "router",
                Some("sdk"),
                "secret-transport-token",
                Message::new(
                    kinds::V1_REPLY,
                    serde_json::json!({"request_id":registration.request_id,"success":true}),
                ),
            );
            framing::write_frame(&mut socket, &reply, &[]).unwrap();
            let received_id = loop {
                let (command, _) = framing::read_frame(&mut socket).unwrap();
                if command.message.kind == "test.command.v1" {
                    break command.request_id;
                }
            };
            for i in 0..300 {
                let event = Envelope::new(
                    "test",
                    "source",
                    Some("sdk"),
                    "secret-transport-token",
                    Message::new("test.event.v1", serde_json::json!({"index":i})),
                );
                framing::write_frame(&mut socket, &event, &[]).unwrap();
            }
            received_id
        });
        let session = Session::connect(config).unwrap();
        let request_id = session
            .publish(Publish {
                destination: Some("source".into()),
                kind: "test.command.v1".into(),
                body: serde_json::json!({}),
            })
            .unwrap();
        assert_eq!(router.join().unwrap(), request_id);
        let deadline = Instant::now() + Duration::from_secs(5);
        while session.status().connected && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        assert!(!session.status().connected);
        assert_eq!(session.status().dropped_events, 300 - QUEUE_CAPACITY as u64);
        let mut count = 0;
        while let Some(event) = session.poll() {
            assert!(
                !serde_json::to_string(&event)
                    .unwrap()
                    .contains("secret-transport-token")
            );
            count += 1;
        }
        assert_eq!(count, QUEUE_CAPACITY);
        assert!(
            session
                .publish(Publish {
                    destination: None,
                    kind: "test".into(),
                    body: serde_json::json!({})
                })
                .is_err()
        );
    }
    #[test]
    fn reject_ambiguous_or_bulk_commands() {
        for body in [
            serde_json::json!({"message_type":"override"}),
            Value::Null,
            serde_json::json!({"x":"x".repeat(MAX_BODY_BYTES)}),
        ] {
            assert!(
                Publish {
                    destination: None,
                    kind: "test.event.v1".into(),
                    body
                }
                .validate()
                .is_err()
            );
        }
    }
    #[test]
    fn projection_has_no_credentials() {
        let event = Event {
            producer_id: "source".into(),
            request_id: "1".into(),
            issued_at_unix_ms: 1,
            kind: "test".into(),
            body: serde_json::json!({}),
        };
        assert!(serde_json::to_value(event).unwrap().get("token").is_none());
    }
}
