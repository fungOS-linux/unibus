//! Bounded IMAP owner adapter for the Unibus mail bridge.
//!
//! The adapter owns the IMAP session and preserves the metadata boundary from
//! [`unibus_mail`]: it publishes identity, size, time, and flags, and fetches
//! content only for an authorized caller. Bodies never enter a delta.
//!
//! Credentials are resolved through [`CredentialResolver`]. The default
//! [`EnvResolver`] accepts `env://NAME`; `geode://` references are rejected here
//! on purpose, because a host process must supply the owner-local geode client.

use std::fmt;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, Result, bail, ensure};
use async_imap::types::{Fetch, Flag, Mailbox};
use async_trait::async_trait;
use futures_util::TryStreamExt;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;
use unibus_mail::{Account, Capabilities, Delta, Message, Metadata, Protocol, SCHEMA_VERSION};

/// Metadata-only fetch query. No body section is requested.
const METADATA_QUERY: &str = "(UID FLAGS INTERNALDATE RFC822.SIZE ENVELOPE)";

/// A bounded body fetch. `PEEK` must not mark the message as seen.
const BODY_QUERY: &str = "(BODY.PEEK[])";

/// Delta lease requested from consumers.
pub const DELTA_LEASE_MS: u64 = 60_000;

/// How the adapter authenticates after connecting.
///
/// `Password` covers `LOGIN` (app passwords, local test servers). `XOAuth2`
/// covers Gmail and other OAuth2 IMAP providers with a short-lived bearer token.
#[derive(Clone, PartialEq, Eq)]
pub enum MailAuth {
    Password(String),
    XOAuth2(String),
}

impl fmt::Debug for MailAuth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Password(_) => f.write_str("MailAuth::Password(***redacted***)"),
            Self::XOAuth2(_) => f.write_str("MailAuth::XOAuth2(***redacted***)"),
        }
    }
}

/// Resolve a credential reference to an authentication secret.
///
/// A host resolver implements this over geode: for Gmail it leases an access
/// token from `geode-oauth` (refreshing the refresh token first when needed). The
/// adapter never caches the value and never logs it.
#[async_trait]
pub trait CredentialResolver: Send + Sync {
    async fn resolve(&self, reference: &str) -> Result<MailAuth>;
}

/// Development resolver. Only `env://NAME` is accepted; `geode://` needs a
/// host-provided resolver and fails closed here.
#[derive(Debug, Default, Clone, Copy)]
pub struct EnvResolver;

#[async_trait]
impl CredentialResolver for EnvResolver {
    async fn resolve(&self, reference: &str) -> Result<MailAuth> {
        let name = reference
            .strip_prefix("env://")
            .context("this resolver only accepts env:// credential references")?;
        ensure!(!name.is_empty(), "env credential reference is empty");
        let value = std::env::var(name)
            .with_context(|| format!("read credential environment variable {name}"))?;
        ensure!(!value.is_empty(), "credential is empty");
        Ok(MailAuth::Password(value))
    }
}

/// A test/host resolver for an already-resolved secret. Not for production use.
#[derive(Clone)]
pub struct StaticResolver(String);

impl StaticResolver {
    pub fn new(secret: impl Into<String>) -> Self {
        Self(secret.into())
    }
}

#[async_trait]
impl CredentialResolver for StaticResolver {
    async fn resolve(&self, _reference: &str) -> Result<MailAuth> {
        Ok(MailAuth::Password(self.0.clone()))
    }
}

/// Test/host resolver that yields an OAuth2 access token.
#[derive(Clone)]
pub struct OAuthTokenResolver(String);

impl OAuthTokenResolver {
    pub fn new(access_token: impl Into<String>) -> Self {
        Self(access_token.into())
    }
}

#[async_trait]
impl CredentialResolver for OAuthTokenResolver {
    async fn resolve(&self, _reference: &str) -> Result<MailAuth> {
        Ok(MailAuth::XOAuth2(self.0.clone()))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MailboxState {
    pub uidvalidity: u64,
    pub exists: u32,
    pub uidnext: u64,
}

/// Metadata projected from one IMAP FETCH, without a body.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FetchedMeta {
    pub uid: u64,
    pub size_bytes: u64,
    pub flags: Vec<String>,
    pub internal_date_unix_ms: u64,
    pub from_domain: Option<String>,
    pub subject: Option<String>,
}

/// Project one fetched message into the bus contract.
pub fn project_message(
    account_id: &str,
    mailbox: &str,
    uidvalidity: u64,
    meta: FetchedMeta,
    include_metadata: bool,
) -> Result<Message> {
    ensure!(uidvalidity > 0, "IMAP UIDVALIDITY is required");
    ensure!(meta.uid > 0, "IMAP UID must be positive");
    ensure!(
        meta.flags.len() <= unibus_mail::MAX_FLAGS,
        "too many IMAP flags"
    );
    let metadata = if include_metadata {
        let from_domain = meta.from_domain.filter(|domain| {
            // Keep only a simple domain; never publish a full address or host.
            !domain.is_empty()
                && domain.len() <= 253
                && domain
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-'))
        });
        let subject = meta.subject.and_then(|subject| {
            let subject: String = subject
                .chars()
                .filter(|c| !c.is_control() || *c == '\t')
                .take(unibus_mail::MAX_SUBJECT)
                .collect();
            (!subject.trim().is_empty()).then_some(subject)
        });
        if from_domain.is_some() || subject.is_some() {
            Some(Metadata {
                from_domain,
                subject,
            })
        } else {
            None
        }
    } else {
        None
    };
    let message = Message {
        schema_version: SCHEMA_VERSION,
        account_id: account_id.to_owned(),
        mailbox: mailbox.to_owned(),
        uidvalidity,
        uid: meta.uid,
        uidl: None,
        size_bytes: meta.size_bytes,
        received_at_unix_ms: meta.internal_date_unix_ms,
        flags: meta.flags,
        metadata,
    };
    let mut message = message;
    if message.validate().is_err() && message.metadata.is_some() {
        // A server-supplied domain/subject that fails the contract is dropped;
        // identity, size, time, and flags are still publishable.
        message.metadata = None;
    }
    message.validate().map_err(anyhow::Error::msg)?;
    Ok(message)
}

/// Build one delta window and validate it before publication.
pub fn build_delta(
    account_id: &str,
    mailbox: &str,
    generation: impl Into<String>,
    observed_at_unix_ms: u64,
    upserts: Vec<Message>,
    expunged: Vec<u64>,
) -> Result<Delta> {
    let cursor = upserts.iter().map(|message| message.uid).max().unwrap_or(0);
    let delta = Delta {
        schema_version: SCHEMA_VERSION,
        account_id: account_id.to_owned(),
        mailbox: mailbox.to_owned(),
        generation: generation.into(),
        cursor,
        upserts,
        expunged,
        observed_at_unix_ms,
        valid_until_unix_ms: observed_at_unix_ms.saturating_add(DELTA_LEASE_MS),
    };
    delta.validate().map_err(anyhow::Error::msg)?;
    Ok(delta)
}

/// IMAP over plain TCP or TLS.
enum ImapStream {
    Plain(TcpStream),
    Tls(Box<tokio_rustls::client::TlsStream<TcpStream>>),
}

impl fmt::Debug for ImapStream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Plain(_) => "ImapStream::Plain",
            Self::Tls(_) => "ImapStream::Tls",
        })
    }
}

impl AsyncRead for ImapStream {
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

impl AsyncWrite for ImapStream {
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

/// One connected, authenticated IMAP session.
pub struct ImapSession {
    session: async_imap::Session<ImapStream>,
    account: Account,
    uidvalidity: u64,
    uidnext: u64,
    publish_metadata: bool,
}

impl ImapSession {
    /// Connect and authenticate. The credential is resolved once and dropped
    /// after login; reconnect resolves it again.
    pub async fn connect(
        account: Account,
        resolver: &dyn CredentialResolver,
        publish_metadata: bool,
    ) -> Result<Self> {
        account.validate().map_err(anyhow::Error::msg)?;
        ensure!(
            account.protocol == Protocol::Imap,
            "unibus-mail-imap only accepts IMAP accounts"
        );
        let auth = if account.credential_ref.is_empty() {
            MailAuth::Password(String::new())
        } else {
            resolver.resolve(&account.credential_ref).await?
        };
        let stream = if account.tls {
            tls_connect(&account.host, account.port).await?
        } else {
            ImapStream::Plain(
                TcpStream::connect((account.host.as_str(), account.port))
                    .await
                    .with_context(|| format!("connect IMAP {}", account.host))?,
            )
        };
        let client = async_imap::Client::new(stream);
        let session = match auth {
            MailAuth::Password(password) => client
                .login(&account.username, &password)
                .await
                .map_err(|(error, _)| anyhow::anyhow!("IMAP login failed: {error}"))?,
            MailAuth::XOAuth2(token) => {
                let authenticator = XOAuth2Authenticator::new(&account.username, token);
                client
                    .authenticate("XOAUTH2", authenticator)
                    .await
                    .map_err(|(error, _)| anyhow::anyhow!("IMAP XOAUTH2 failed: {error}"))?
            }
        };
        Ok(Self {
            session,
            account,
            uidvalidity: 0,
            uidnext: 0,
            publish_metadata,
        })
    }

    pub fn account(&self) -> &Account {
        &self.account
    }

    pub async fn select(&mut self, mailbox: &str) -> Result<MailboxState> {
        let mailbox: Mailbox = self
            .session
            .select(mailbox)
            .await
            .with_context(|| format!("select IMAP mailbox {mailbox}"))?;
        let uidvalidity = mailbox
            .uid_validity
            .context("IMAP server did not report UIDVALIDITY")? as u64;
        ensure!(uidvalidity > 0, "IMAP UIDVALIDITY must be positive");
        self.uidvalidity = uidvalidity;
        self.uidnext = mailbox.uid_next.unwrap_or(0) as u64;
        Ok(MailboxState {
            uidvalidity,
            exists: mailbox.exists,
            uidnext: self.uidnext,
        })
    }

    /// Fetch metadata for UIDs at or after `from_uid`.
    ///
    /// `from_uid` must be the last fully published UID + 1. The `uidnext` guard
    /// prevents the IMAP `N:*` "return the last message even when N is beyond the
    /// mailbox" trap from re-publishing an old message.
    pub async fn fetch_metadata(
        &mut self,
        mailbox: &str,
        from_uid: u64,
        max: usize,
    ) -> Result<Vec<Message>> {
        ensure!(max > 0, "fetch bound must be positive");
        ensure!(self.uidvalidity > 0, "select a mailbox before fetching");
        if from_uid >= self.uidnext && self.uidnext != 0 {
            return Ok(Vec::new());
        }
        let set = format!("{from_uid}:*");
        let mut stream = self
            .session
            .uid_fetch(set, METADATA_QUERY)
            .await
            .with_context(|| format!("fetch IMAP metadata for {mailbox}"))?;
        let mut messages = Vec::new();
        while let Some(fetch) = stream.try_next().await? {
            if messages.len() >= max {
                break;
            }
            if let Some(meta) = fetched_meta(&fetch) {
                messages.push(project_message(
                    &self.account.account_id,
                    mailbox,
                    self.uidvalidity,
                    meta,
                    self.publish_metadata,
                )?);
            }
        }
        Ok(messages)
    }

    /// Fetch a bounded body. The adapter truncates at `max_bytes`; callers that
    /// need exact MIME boundaries must request a part from a content service.
    pub async fn fetch_body(&mut self, mailbox: &str, uid: u64, max_bytes: u64) -> Result<Vec<u8>> {
        ensure!(uid > 0, "UID must be positive");
        ensure!(
            (1..=unibus_mail::MAX_FETCH_BYTES).contains(&max_bytes),
            "max_bytes is outside its bounds"
        );
        ensure!(self.uidvalidity > 0, "select a mailbox before fetching");
        let mut stream = self
            .session
            .uid_fetch(uid.to_string(), BODY_QUERY)
            .await
            .with_context(|| format!("fetch IMAP body for {mailbox} UID {uid}"))?;
        let Some(fetch) = stream.try_next().await? else {
            bail!("IMAP message {uid} was not found");
        };
        let body = fetch.body().context("IMAP response had no body section")?;
        let limit = usize::try_from(max_bytes).unwrap_or(usize::MAX);
        Ok(body[..body.len().min(limit)].to_vec())
    }

    pub async fn logout(mut self) -> Result<()> {
        self.session.logout().await?;
        Ok(())
    }
}

fn fetched_meta(fetch: &Fetch) -> Option<FetchedMeta> {
    let uid = fetch.uid? as u64;
    let flags = fetch
        .flags()
        .map(|flag| match flag {
            Flag::Seen => "\\Seen".to_owned(),
            Flag::Answered => "\\Answered".to_owned(),
            Flag::Flagged => "\\Flagged".to_owned(),
            Flag::Deleted => "\\Deleted".to_owned(),
            Flag::Draft => "\\Draft".to_owned(),
            Flag::Recent => "\\Recent".to_owned(),
            Flag::MayCreate => "\\*".to_owned(),
            Flag::Custom(value) => value.into_owned(),
        })
        .collect();
    let internal_date_unix_ms = fetch
        .internal_date()
        .and_then(|date| u64::try_from(date.timestamp_millis()).ok())
        .unwrap_or_else(now_ms);
    let (from_domain, subject) = match fetch.envelope() {
        Some(envelope) => {
            let from_domain = envelope
                .from
                .as_ref()
                .and_then(|addresses| addresses.first())
                .and_then(|address| address.host.as_ref())
                .map(|host| String::from_utf8_lossy(host).into_owned());
            let subject = envelope
                .subject
                .as_ref()
                .map(|subject| String::from_utf8_lossy(subject).into_owned());
            (from_domain, subject)
        }
        None => (None, None),
    };
    Some(FetchedMeta {
        uid,
        size_bytes: fetch.size.unwrap_or(0) as u64,
        flags,
        internal_date_unix_ms,
        from_domain,
        subject,
    })
}

async fn tls_connect(host: &str, port: u16) -> Result<ImapStream> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let config = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let connector = tokio_rustls::TlsConnector::from(std::sync::Arc::new(config));
    let server_name = rustls::pki_types::ServerName::try_from(host.to_owned())
        .with_context(|| format!("invalid IMAP TLS server name {host}"))?;
    let stream = tokio::time::timeout(Duration::from_secs(15), TcpStream::connect((host, port)))
        .await
        .context("IMAP TLS connect timed out")?
        .with_context(|| format!("connect IMAP TLS {host}:{port}"))?;
    let stream = connector
        .connect(server_name, stream)
        .await
        .with_context(|| format!("TLS handshake with {host}"))?;
    Ok(ImapStream::Tls(Box::new(stream)))
}

/// SASL XOAUTH2 initial response: `user=<login>\x01auth=Bearer <token>\x01\x01`.
struct XOAuth2Authenticator {
    response: Vec<u8>,
}

impl XOAuth2Authenticator {
    fn new(login: &str, token: String) -> Self {
        let mut response = Vec::with_capacity(login.len() + token.len() + 32);
        response.extend_from_slice(b"user=");
        response.extend_from_slice(login.as_bytes());
        response.extend_from_slice(b"\x01auth=Bearer ");
        response.extend_from_slice(token.as_bytes());
        response.extend_from_slice(b"\x01\x01");
        Self { response }
    }
}

impl async_imap::Authenticator for XOAuth2Authenticator {
    type Response = Vec<u8>;

    fn process(&mut self, _challenge: &[u8]) -> Self::Response {
        self.response.clone()
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

/// The maximum capability set an IMAP adapter may advertise. Flags/delete are
/// reported only after `select` proves the mailbox supports them.
pub fn imap_capabilities() -> Capabilities {
    Protocol::Imap.maximum_capabilities()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta() -> FetchedMeta {
        FetchedMeta {
            uid: 7,
            size_bytes: 2048,
            flags: vec!["\\Seen".into()],
            internal_date_unix_ms: 1_700_000_000_000,
            from_domain: Some("example.com".into()),
            subject: Some("hello".into()),
        }
    }

    fn account() -> Account {
        Account {
            schema_version: SCHEMA_VERSION,
            account_id: "mail-work".into(),
            protocol: Protocol::Imap,
            host: "imap.example.com".into(),
            port: 993,
            tls: true,
            username: "avery@example.com".into(),
            credential_ref: "geode://mail/work/imap".into(),
            folders: vec!["INBOX".into()],
            poll_interval_ms: 30_000,
            capabilities: imap_capabilities(),
        }
    }

    #[test]
    fn projection_publishes_metadata_only_when_enabled() {
        let message = project_message("mail-work", "INBOX", 42, meta(), true).unwrap();
        assert_eq!(message.uidvalidity, 42);
        assert_eq!(message.uid, 7);
        assert_eq!(
            message.metadata.as_ref().unwrap().from_domain.as_deref(),
            Some("example.com")
        );
        message.validate().unwrap();

        let message = project_message("mail-work", "INBOX", 42, meta(), false).unwrap();
        assert!(message.metadata.is_none());
        let json = serde_json::to_string(&message).unwrap();
        assert!(!json.contains("hello"));
    }

    #[test]
    fn projection_drops_unsafe_domains_and_empty_subjects() {
        let mut meta = meta();
        meta.from_domain = Some("bad domain!".into());
        meta.subject = Some("\u{0}\u{1}  ".into());
        let message = project_message("mail-work", "INBOX", 42, meta, true).unwrap();
        assert!(message.metadata.is_none());
    }

    #[test]
    fn delta_cursor_is_the_highest_upsert_and_validates() {
        let first = project_message("mail-work", "INBOX", 42, meta(), false).unwrap();
        let mut second_meta = meta();
        second_meta.uid = 9;
        let second = project_message("mail-work", "INBOX", 42, second_meta, false).unwrap();
        let delta = build_delta(
            "mail-work",
            "INBOX",
            "42",
            1_700_000_000_000,
            vec![first, second],
            vec![],
        )
        .unwrap();
        assert_eq!(delta.cursor, 9);
        delta.validate().unwrap();
    }

    #[tokio::test]
    async fn env_resolver_reads_only_env_references() {
        let name = "UNIBUS_MAIL_IMAP_TEST_PASSWORD";
        // SAFETY: test-local variable with a unique name.
        unsafe { std::env::set_var(name, "hunter2") };
        let resolver = EnvResolver;
        assert_eq!(
            resolver.resolve(&format!("env://{name}")).await.unwrap(),
            MailAuth::Password("hunter2".into())
        );
        assert!(resolver.resolve("geode://mail/work/imap").await.is_err());
        unsafe { std::env::remove_var(name) };
    }

    #[test]
    fn account_contract_rejects_insecure_or_unbounded_input() {
        let account = account();
        account.validate().unwrap();
        let mut invalid = account.clone();
        invalid.credential_ref = "geode://user:pass@mail/work/imap".into();
        assert!(invalid.validate().is_err());
        invalid = account;
        invalid.poll_interval_ms = 0;
        assert!(invalid.validate().is_err());
    }
}
