//! IMAP → Unibus mail bridge.
//!
//! One IMAP session, one account, one Unibus producer. Publishes
//! `email.account.v1` once and bounded `email.delta.v1` windows as it polls.
//! Bodies are fetched only for an authorized `email.fetch.v1` caller.
//!
//! Run: `unibus-mail-imap /path/to/bridge.json`

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, Result, bail, ensure};
use serde::Deserialize;
use unibus::Message;
use unibus::client::{Client, ClientConfig};
use unibus_mail::{
    ACCOUNT_KIND, Account, Capabilities, DELTA_KIND, Delta, FETCH_KIND, FETCH_REPLY_KIND,
    FetchRequest, FetchResponse, Protocol, SCHEMA_VERSION,
};
use unibus_mail_imap::{ImapSession, MailAuth};

const BUS_POLL: Duration = Duration::from_millis(100);
const DELTA_LEASE_MS: u64 = 60_000;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct BridgeConfig {
    bus: ClientConfig,
    account: Account,
    #[serde(default)]
    publish_metadata: bool,
    #[serde(default = "default_batch")]
    batch_size: usize,
    #[serde(default = "default_initial_max")]
    initial_max: u64,
    /// When present, `account.credential_ref` names the OAuth refresh token and
    /// the adapter obtains a short-lived access token before every connect.
    #[serde(default)]
    oauth: Option<OAuthConfig>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct OAuthConfig {
    client_id: String,
    client_secret_ref: String,
    #[serde(default = "default_token_endpoint")]
    token_endpoint: String,
}

fn default_token_endpoint() -> String {
    "https://oauth2.googleapis.com/token".into()
}

fn default_batch() -> usize {
    64
}

fn default_initial_max() -> u64 {
    64
}

#[tokio::main]
async fn main() -> Result<()> {
    let path = std::env::args()
        .nth(1)
        .context("usage: unibus-mail-imap <bridge.json>")?;
    let bytes = std::fs::read(&path).with_context(|| format!("read config {path}"))?;
    let mut config: BridgeConfig =
        serde_json::from_slice(&bytes).with_context(|| format!("parse config {path}"))?;
    config.account.validate().map_err(anyhow::Error::msg)?;
    ensure!(
        config.account.protocol == Protocol::Imap,
        "this adapter only accepts IMAP accounts"
    );
    config.account.capabilities = Capabilities {
        push: false,
        ..config.account.capabilities
    };
    ensure!(config.batch_size > 0, "batch_size must be positive");
    ensure!(config.initial_max > 0, "initial_max must be positive");
    ensure!(
        config
            .bus
            .capabilities
            .iter()
            .any(|capability| capability == "mail.bridge"),
        "bus capabilities must include mail.bridge"
    );

    let client = Arc::new(Client::connect(config.bus.clone())?);
    publish(&client, ACCOUNT_KIND, &config.account)?;
    eprintln!(
        "unibus-mail-imap: {} polling {} folder(s), producer {}",
        config.account.account_id,
        folders(&config.account).len(),
        client.config().producer_id
    );

    loop {
        if let Err(error) = run(&client, &config).await {
            eprintln!("unibus-mail-imap: session ended: {error:#}");
        }
        if !client.is_healthy() {
            bail!("Unibus hub connection is unhealthy; exiting for the service manager");
        }
        tokio::time::sleep(Duration::from_millis(config.account.poll_interval_ms)).await;
    }
}

async fn run(client: &Client, config: &BridgeConfig) -> Result<()> {
    let mut session = connect_session(config).await?;
    let mut last_uid: BTreeMap<String, u64> = BTreeMap::new();
    let mut generation: BTreeMap<String, u64> = BTreeMap::new();

    let mut next_poll = tokio::time::Instant::now();
    for folder in folders(&config.account) {
        let state = session.select(&folder).await?;
        generation.insert(folder.clone(), state.uidvalidity);
        let from = state.uidnext.saturating_sub(config.initial_max).max(1);
        poll_folder(
            client,
            config,
            &mut session,
            &folder,
            from,
            &mut last_uid,
            &generation,
        )
        .await?;
    }

    loop {
        tokio::time::sleep(BUS_POLL).await;
        drain_bus(client, config, &mut session)?;
        if !client.is_healthy() {
            return Ok(());
        }
        // Polling cadence is owned by the account contract. The loop wakes more
        // often only to serve fetch requests.
        if tokio::time::Instant::now() < next_poll {
            continue;
        }
        next_poll =
            tokio::time::Instant::now() + Duration::from_millis(config.account.poll_interval_ms);
        for folder in folders(&config.account) {
            let state = session.select(&folder).await?;
            if generation.get(&folder).copied() != Some(state.uidvalidity) {
                // UIDVALIDITY changed: the old cursor is meaningless.
                generation.insert(folder.clone(), state.uidvalidity);
                last_uid.insert(folder.clone(), 0);
                continue;
            }
            let from = last_uid.get(&folder).copied().unwrap_or(0) + 1;
            poll_folder(
                client,
                config,
                &mut session,
                &folder,
                from,
                &mut last_uid,
                &generation,
            )
            .await?;
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn poll_folder(
    client: &Client,
    config: &BridgeConfig,
    session: &mut ImapSession,
    folder: &str,
    from_uid: u64,
    last_uid: &mut BTreeMap<String, u64>,
    generation: &BTreeMap<String, u64>,
) -> Result<()> {
    let messages = session
        .fetch_metadata(folder, from_uid, config.batch_size)
        .await?;
    if messages.is_empty() {
        return Ok(());
    }
    let observed = now_ms();
    let generation = generation
        .get(folder)
        .copied()
        .context("folder was not selected")?
        .to_string();
    let delta = Delta {
        schema_version: SCHEMA_VERSION,
        account_id: config.account.account_id.clone(),
        mailbox: folder.to_owned(),
        generation,
        cursor: messages
            .iter()
            .map(|message| message.uid)
            .max()
            .unwrap_or(0),
        valid_until_unix_ms: observed.saturating_add(DELTA_LEASE_MS),
        upserts: messages,
        expunged: Vec::new(),
        observed_at_unix_ms: observed,
    };
    delta.validate().map_err(anyhow::Error::msg)?;
    let highest = delta.cursor;
    publish(client, DELTA_KIND, &delta)?;
    last_uid.insert(folder.to_owned(), highest);
    Ok(())
}

fn drain_bus(client: &Client, config: &BridgeConfig, session: &mut ImapSession) -> Result<()> {
    while let Some((envelope, _)) = client.try_read()? {
        if envelope.message.kind != FETCH_KIND {
            continue;
        }
        let value = serde_json::Value::Object(envelope.message.body.clone());
        let request: FetchRequest = match serde_json::from_value(value) {
            Ok(request) => request,
            Err(error) => {
                eprintln!("unibus-mail-imap: invalid fetch request: {error}");
                continue;
            }
        };
        if request.account_id != config.account.account_id {
            continue;
        }
        if let Err(error) = request.validate() {
            eprintln!("unibus-mail-imap: rejected fetch request: {error}");
            continue;
        }
        let reply = match futures_executor_block_on(session.fetch_body(
            &request.mailbox,
            request.uid,
            request.max_bytes,
        )) {
            Ok(body) => (body, true),
            Err(error) => {
                eprintln!(
                    "unibus-mail-imap: fetch {} UID {} failed: {error:#}",
                    request.mailbox, request.uid
                );
                (
                    unibus_mail_imap::FetchedBody {
                        bytes: Vec::new(),
                        truncated: false,
                    },
                    false,
                )
            }
        };
        let (body, retained) = reply;
        let response = FetchResponse {
            schema_version: SCHEMA_VERSION,
            request_id: request.request_id,
            account_id: request.account_id,
            mailbox: request.mailbox,
            uid: request.uid,
            content_type: "message/rfc822".into(),
            size_bytes: body.bytes.len() as u64,
            truncated: body.truncated,
            retained,
        };
        response
            .validate(body.bytes.len())
            .map_err(anyhow::Error::msg)?;
        reply_envelope(
            client,
            &envelope.producer_id,
            FETCH_REPLY_KIND,
            &response,
            &body.bytes,
        )?;
    }
    Ok(())
}

async fn connect_session(config: &BridgeConfig) -> Result<ImapSession> {
    let auth = if let Some(oauth) = &config.oauth {
        ensure!(
            !config.account.credential_ref.is_empty(),
            "Gmail OAuth needs account.credential_ref to name the refresh token"
        );
        let refresh = unibus_credentials::resolve(&config.account.credential_ref)?
            .context("refresh token reference resolved to nothing")?;
        let client_secret = unibus_credentials::resolve(&oauth.client_secret_ref)?
            .context("client secret reference resolved to nothing")?;
        let access = refresh_access_token(oauth, &refresh, &client_secret).await?;
        MailAuth::XOAuth2(access)
    } else {
        MailAuth::Password(
            unibus_credentials::resolve(&config.account.credential_ref)?.unwrap_or_default(),
        )
    };
    ImapSession::connect_with_auth(config.account.clone(), auth, config.publish_metadata).await
}

async fn refresh_access_token(
    oauth: &OAuthConfig,
    refresh_token: &str,
    client_secret: &str,
) -> Result<String> {
    let response = reqwest::Client::new()
        .post(&oauth.token_endpoint)
        .form(&[
            ("client_id", oauth.client_id.as_str()),
            ("client_secret", client_secret),
            ("refresh_token", refresh_token),
            ("grant_type", "refresh_token"),
        ])
        .send()
        .await
        .context("reach OAuth token endpoint")?;
    let status = response.status();
    let body: serde_json::Value = response.json().await.unwrap_or_default();
    if !status.is_success() {
        let error = body
            .get("error")
            .and_then(|value| value.as_str())
            .unwrap_or("unknown");
        bail!("OAuth token refresh failed: HTTP {status} ({error})");
    }
    body.get("access_token")
        .and_then(|value| value.as_str())
        .map(str::to_owned)
        .context("OAuth token response has no access_token")
}

fn folders(account: &Account) -> Vec<String> {
    if account.folders.is_empty() {
        vec!["INBOX".into()]
    } else {
        account.folders.clone()
    }
}

fn publish<T: serde::Serialize>(client: &Client, kind: &str, body: &T) -> Result<()> {
    let envelope = client.envelope(None, Message::new(kind, serde_json::to_value(body)?))?;
    client.send(&envelope, &[])?;
    Ok(())
}

fn reply_envelope<T: serde::Serialize>(
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

/// The IMAP fetch is async but the Unibus client is synchronous. This tiny
/// bridge keeps the fetch call inside a current-thread runtime rather than
/// blocking the daemon's runtime.
fn futures_executor_block_on<F: std::future::Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build fetch runtime")
        .block_on(future)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}
