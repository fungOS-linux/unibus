//! IRC → Unibus chat bridge.
//!
//! One IRC session, one account, one Unibus producer. The bridge publishes
//! `chat.account.v1` once and one bounded `chat.delta.v1` per observed message.
//! Message text stays in a bounded local ring and is served only through an
//! authorized `chat.fetch.v1` request.
//!
//! Run: `unibus-irc /path/to/bridge.json`

use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, Result, bail, ensure};
use serde::Deserialize;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;
use unibus::Message;
use unibus::client::{Client, ClientConfig};
use unibus_chat::{
    ACCOUNT_KIND, Account, DELTA_KIND, Delta, FETCH_KIND, FETCH_REPLY_KIND, FetchRequest,
    FetchResponse, SCHEMA_VERSION,
};
use unibus_irc::{BodyRing, IrcEvent, IrcSession};

const BUS_POLL: Duration = Duration::from_millis(100);
const DELTA_LEASE_MS: u64 = 60_000;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct BridgeConfig {
    bus: ClientConfig,
    account: Account,
    #[serde(default = "default_ring_messages")]
    ring_messages: usize,
    #[serde(default = "default_ring_bytes")]
    ring_bytes: usize,
}

fn default_ring_messages() -> usize {
    unibus_irc::DEFAULT_RING_MESSAGES
}

fn default_ring_bytes() -> usize {
    unibus_irc::DEFAULT_RING_BYTES
}

#[tokio::main]
async fn main() -> Result<()> {
    let path = std::env::args()
        .nth(1)
        .context("usage: unibus-irc <bridge.json>")?;
    let bytes = std::fs::read(&path).with_context(|| format!("read config {path}"))?;
    let config: BridgeConfig =
        serde_json::from_slice(&bytes).with_context(|| format!("parse config {path}"))?;
    config.account.validate().map_err(anyhow::Error::msg)?;
    ensure!(
        config.ring_messages > 0 && config.ring_bytes > 0,
        "ring bounds must be positive"
    );
    ensure!(
        config
            .bus
            .capabilities
            .iter()
            .any(|capability| capability == "chat.bridge"),
        "bus capabilities must include chat.bridge"
    );

    let client = Arc::new(Client::connect(config.bus.clone())?);
    publish(&client, ACCOUNT_KIND, &config.account)?;
    eprintln!(
        "unibus-irc: {} on {} ({} channels), producer {}",
        config.account.account_id,
        config.account.network,
        config.account.channels.len(),
        client.config().producer_id
    );

    loop {
        if let Err(error) = run(&client, &config).await {
            eprintln!("unibus-irc: session ended: {error:#}");
        }
        if !client.is_healthy() {
            bail!("Unibus hub connection is unhealthy; exiting for the service manager");
        }
        tokio::time::sleep(Duration::from_millis(config.account.reconnect_interval_ms)).await;
    }
}

async fn run(client: &Client, config: &BridgeConfig) -> Result<()> {
    let stream = connect_stream(&config.account).await?;
    let secret = resolve_secret(&config.account.credential_ref)?;
    let generation = now_ms().to_string();
    let mut session = IrcSession::connect(
        stream,
        config.account.clone(),
        secret.as_deref(),
        generation.clone(),
    )
    .await?;
    let mut ring = BodyRing::new(config.ring_messages, config.ring_bytes)?;
    let mut cursor: u64 = 0;
    eprintln!("unibus-irc: registered as {}", config.account.nick);

    loop {
        tokio::select! {
            event = session.next_event() => {
                let Some(event) = event? else {
                    return Ok(());
                };
                match event {
                    IrcEvent::Message(incoming) => {
                        ring.insert(
                            incoming.message_id.clone(),
                            incoming.channel.clone(),
                            incoming.body.clone(),
                        );
                        cursor = cursor.wrapping_add(1);
                        let observed = now_ms();
                        let delta = Delta {
                            schema_version: SCHEMA_VERSION,
                            account_id: config.account.account_id.clone(),
                            generation: generation.clone(),
                            cursor,
                            upserts: vec![incoming.metadata(&config.account.account_id)],
                            expunged: Vec::new(),
                            observed_at_unix_ms: observed,
                            valid_until_unix_ms: observed.saturating_add(DELTA_LEASE_MS),
                        };
                        delta.validate().map_err(anyhow::Error::msg)?;
                        publish(client, DELTA_KIND, &delta)?;
                    }
                    IrcEvent::Joined(channel) => eprintln!("unibus-irc: joined {channel}"),
                    IrcEvent::Parted(channel) => eprintln!("unibus-irc: parted {channel}"),
                    IrcEvent::Ready { nick } => eprintln!("unibus-irc: ready as {nick}"),
                }
            }
            _ = tokio::time::sleep(BUS_POLL) => {}
        }
        drain_bus(client, config, &ring)?;
    }
}

fn drain_bus(client: &Client, config: &BridgeConfig, ring: &BodyRing) -> Result<()> {
    while let Some((envelope, _)) = client.try_read()? {
        if envelope.message.kind != FETCH_KIND {
            continue;
        }
        let value = serde_json::Value::Object(envelope.message.body.clone());
        let request: FetchRequest = match serde_json::from_value(value) {
            Ok(request) => request,
            Err(error) => {
                eprintln!("unibus-irc: invalid fetch request: {error}");
                continue;
            }
        };
        if request.account_id != config.account.account_id {
            continue;
        }
        if let Err(error) = request.validate() {
            eprintln!("unibus-irc: rejected fetch request: {error}");
            continue;
        }
        let (response, payload) = match ring.fetch(&request) {
            Some((content_type, bytes, truncated)) => (
                FetchResponse {
                    schema_version: SCHEMA_VERSION,
                    request_id: request.request_id,
                    account_id: request.account_id,
                    channel: request.channel,
                    message_id: request.message_id,
                    content_type,
                    size_bytes: bytes.len() as u64,
                    truncated,
                    retained: true,
                },
                bytes,
            ),
            None => (
                FetchResponse {
                    schema_version: SCHEMA_VERSION,
                    request_id: request.request_id,
                    account_id: request.account_id,
                    channel: request.channel,
                    message_id: request.message_id,
                    content_type: "text/plain; charset=utf-8".into(),
                    size_bytes: 0,
                    truncated: false,
                    retained: false,
                },
                Vec::new(),
            ),
        };
        response
            .validate(payload.len())
            .map_err(anyhow::Error::msg)?;
        reply(
            client,
            &envelope.producer_id,
            FETCH_REPLY_KIND,
            &response,
            &payload,
        )?;
    }
    Ok(())
}

fn resolve_secret(reference: &str) -> Result<Option<String>> {
    if reference.is_empty() {
        return Ok(None);
    }
    let Some(name) = reference.strip_prefix("env://") else {
        bail!(
            "unibus-irc supports env:// credentials only; {reference} needs a host geode resolver"
        );
    };
    let value = std::env::var(name)
        .with_context(|| format!("read credential environment variable {name}"))?;
    ensure!(!value.is_empty(), "IRC credential is empty");
    Ok(Some(value))
}

fn publish<T: serde::Serialize>(client: &Client, kind: &str, body: &T) -> Result<()> {
    let envelope = client.envelope(None, Message::new(kind, serde_json::to_value(body)?))?;
    client.send(&envelope, &[])?;
    Ok(())
}

fn reply<T: serde::Serialize>(
    client: &Client,
    destination: &str,
    kind: &str,
    body: &T,
    payload: &[u8],
) -> Result<()> {
    let envelope = client.envelope(
        Some(destination),
        Message::new(kind, serde_json::to_value(body)?),
    )?;
    client.send(&envelope, payload)?;
    Ok(())
}

enum IrcStream {
    Plain(TcpStream),
    Tls(Box<tokio_rustls::client::TlsStream<TcpStream>>),
}

impl AsyncRead for IrcStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            Self::Plain(stream) => Pin::new(stream).poll_read(cx, buf),
            Self::Tls(stream) => Pin::new(stream).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for IrcStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        match self.get_mut() {
            Self::Plain(stream) => Pin::new(stream).poll_write(cx, buf),
            Self::Tls(stream) => Pin::new(stream).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            Self::Plain(stream) => Pin::new(stream).poll_flush(cx),
            Self::Tls(stream) => Pin::new(stream).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            Self::Plain(stream) => Pin::new(stream).poll_shutdown(cx),
            Self::Tls(stream) => Pin::new(stream).poll_shutdown(cx),
        }
    }
}

async fn connect_stream(account: &Account) -> Result<IrcStream> {
    let stream = TcpStream::connect((account.host.as_str(), account.port))
        .await
        .with_context(|| format!("connect IRC {}:{}", account.host, account.port))?;
    if !account.tls {
        return Ok(IrcStream::Plain(stream));
    }
    let _ = rustls::crypto::ring::default_provider().install_default();
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let config = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let connector = tokio_rustls::TlsConnector::from(Arc::new(config));
    let server_name = rustls::pki_types::ServerName::try_from(account.host.clone())
        .with_context(|| format!("invalid IRC TLS server name {}", account.host))?;
    let stream = connector
        .connect(server_name, stream)
        .await
        .with_context(|| format!("IRC TLS handshake with {}", account.host))?;
    Ok(IrcStream::Tls(Box::new(stream)))
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}
