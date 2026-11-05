//! Bounded chat metadata contracts for Unibus.
//!
//! A chat adapter (IRC first) owns the upstream session. This crate owns only
//! the small, versioned documents that may cross the bus:
//!
//! - [`Account`] describes a chat identity and the channels it observes.
//! - [`Message`] is metadata only; message text never crosses the bus.
//! - [`Delta`] is one bounded change window for an account.
//! - [`FetchRequest`] / [`FetchResponse`] move bounded text directly between an
//!   authorized caller and the owning adapter.
//!
//! Credentials are named with a `geode://...` reference and resolved only by
//! the local owner process. No token, password, or bridge appservice secret is
//! part of any document here.

use serde::{Deserialize, Serialize};

pub const ACCOUNT_KIND: &str = "chat.account.v1";
pub const MESSAGE_KIND: &str = "chat.message.v1";
pub const DELTA_KIND: &str = "chat.delta.v1";
pub const FETCH_KIND: &str = "chat.fetch.v1";
pub const FETCH_REPLY_KIND: &str = "chat.fetch.reply.v1";

pub const SCHEMA_VERSION: u32 = 1;
pub const MAX_ACCOUNT_ID: usize = 128;
pub const MAX_CHANNEL: usize = 200;
pub const MAX_GENERATION: usize = 128;
pub const MAX_HOST: usize = 253;
pub const MAX_NICK: usize = 64;
pub const MAX_CREDENTIAL_REF: usize = 256;
pub const MAX_MESSAGE_ID: usize = 256;
pub const MAX_FLAGS: usize = 16;
pub const MAX_DELTA_MESSAGES: usize = 512;
pub const MAX_FETCH_BYTES: u64 = 4 * 1024 * 1024;

/// The upstream transport an adapter owns. IRC is the first implementation;
/// Beeper/Matrix and other transports reuse the same documents.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Transport {
    Irc,
    Beeper,
    Matrix,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Capabilities {
    /// The adapter can observe new messages without polling.
    pub push: bool,
    /// The adapter can fetch bounded history from upstream.
    pub history: bool,
    /// The adapter can send messages upstream. Read-only adapters keep false.
    pub send: bool,
    /// The transport exposes a server-assigned message identity.
    pub server_ids: bool,
}

impl Transport {
    pub const fn maximum_capabilities(self) -> Capabilities {
        match self {
            // IRCv3 `message-tags` gives `msgid`/`server-time` on supporting
            // networks; a plain IRC server may only provide local identity.
            Self::Irc => Capabilities {
                push: true,
                history: false,
                send: true,
                server_ids: true,
            },
            Self::Beeper | Self::Matrix => Capabilities {
                push: true,
                history: true,
                send: true,
                server_ids: true,
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Account {
    pub schema_version: u32,
    pub account_id: String,
    pub transport: Transport,
    /// Human-facing network name (`libera`, `fleet`, `beeper`). Not a secret.
    pub network: String,
    pub host: String,
    pub port: u16,
    pub tls: bool,
    pub nick: String,
    /// A `geode://...` reference for server/SASL credentials. Empty means the
    /// transport needs no secret (for example an open local IRC test server).
    #[serde(default)]
    pub credential_ref: String,
    /// Channels the adapter is allowed to observe. Never server-wide discovery.
    pub channels: Vec<String>,
    pub reconnect_interval_ms: u64,
    pub capabilities: Capabilities,
}

impl Account {
    pub fn validate(&self) -> Result<(), String> {
        if self.schema_version != SCHEMA_VERSION {
            return Err("unsupported account schema".into());
        }
        id(&self.account_id, "account_id", MAX_ACCOUNT_ID)?;
        identifier(&self.network, "network", 64)?;
        host(&self.host)?;
        if self.port == 0 {
            return Err("port is required".into());
        }
        nick(&self.nick)?;
        if !self.credential_ref.is_empty() {
            credential_ref(&self.credential_ref)?;
        }
        if !(1_000..=3_600_000).contains(&self.reconnect_interval_ms) {
            return Err("reconnect_interval_ms is outside the bounded range".into());
        }
        if self.channels.is_empty() || self.channels.len() > 64 {
            return Err("channels must contain between one and 64 entries".into());
        }
        let mut seen = std::collections::BTreeSet::new();
        for channel in &self.channels {
            channel_name(channel)?;
            if !seen.insert(channel.to_ascii_lowercase()) {
                return Err("duplicate channel".into());
            }
        }
        self.capabilities.validate(self.transport)?;
        Ok(())
    }

    pub fn has_channel(&self, channel: &str) -> bool {
        self.channels
            .iter()
            .any(|candidate| candidate.eq_ignore_ascii_case(channel))
    }
}

impl Capabilities {
    pub fn validate(&self, transport: Transport) -> Result<(), String> {
        let maximum = transport.maximum_capabilities();
        for (name, claimed, allowed) in [
            ("push", self.push, maximum.push),
            ("history", self.history, maximum.history),
            ("send", self.send, maximum.send),
            ("server_ids", self.server_ids, maximum.server_ids),
        ] {
            if claimed && !allowed {
                return Err(format!("{transport:?} cannot claim {name} capability"));
            }
        }
        Ok(())
    }
}

/// Whether a message identity is authoritative upstream or adapter-local.
///
/// IRC without `message-tags` has no stable server ID. Such a message is still
/// useful, but consumers must not treat it as deduplicable across reconnects.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Identity {
    Server,
    Local,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageKind {
    Message,
    Notice,
    Action,
    Join,
    Part,
    Quit,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Message {
    pub schema_version: u32,
    pub account_id: String,
    pub channel: String,
    /// Server `msgid` when available; otherwise `local:<generation>:<sequence>`.
    pub message_id: String,
    pub identity: Identity,
    /// Nick only. The full IRC prefix can expose user/host metadata and is not
    /// part of this document.
    pub sender: String,
    pub sent_at_unix_ms: u64,
    pub kind: MessageKind,
    /// UTF-8 length of the retained text. Zero when the adapter did not retain
    /// a body (for example an evicted message).
    pub body_bytes: u64,
    #[serde(default)]
    pub reply_to: Option<String>,
    #[serde(default)]
    pub flags: Vec<String>,
}

impl Message {
    pub fn validate(&self) -> Result<(), String> {
        if self.schema_version != SCHEMA_VERSION {
            return Err("unsupported message schema".into());
        }
        id(&self.account_id, "account_id", MAX_ACCOUNT_ID)?;
        channel_name(&self.channel)?;
        message_id(&self.message_id)?;
        if self.sender.is_empty() {
            return Err("sender is empty".into());
        }
        nick(&self.sender)?;
        if self.sent_at_unix_ms == 0 {
            return Err("sent_at_unix_ms is required".into());
        }
        if self.body_bytes > MAX_FETCH_BYTES {
            return Err("body_bytes is outside its bounds".into());
        }
        if let Some(reply_to) = &self.reply_to {
            message_id(reply_to)?;
        }
        if self.flags.len() > MAX_FLAGS {
            return Err("too many flags".into());
        }
        let mut seen = std::collections::BTreeSet::new();
        for flag in &self.flags {
            identifier(flag, "flag", 64)?;
            if !seen.insert(flag) {
                return Err("duplicate flag".into());
            }
        }
        Ok(())
    }
}

/// One bounded window of chat change. `cursor` is the highest adapter sequence
/// included; consumers advance only when the whole delta validates.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Delta {
    pub schema_version: u32,
    pub account_id: String,
    /// Connection/session epoch. A reconnect starts a new generation, which
    /// tells consumers local message identities are not comparable.
    pub generation: String,
    pub cursor: u64,
    #[serde(default)]
    pub upserts: Vec<Message>,
    #[serde(default)]
    pub expunged: Vec<String>,
    pub observed_at_unix_ms: u64,
    pub valid_until_unix_ms: u64,
}

impl Delta {
    pub fn validate(&self) -> Result<(), String> {
        if self.schema_version != SCHEMA_VERSION {
            return Err("unsupported delta schema".into());
        }
        id(&self.account_id, "account_id", MAX_ACCOUNT_ID)?;
        id(&self.generation, "generation", MAX_GENERATION)?;
        if self.upserts.len() > MAX_DELTA_MESSAGES || self.expunged.len() > MAX_DELTA_MESSAGES {
            return Err("delta exceeds its message bound".into());
        }
        let lease = self
            .valid_until_unix_ms
            .checked_sub(self.observed_at_unix_ms)
            .ok_or("delta lease is inverted")?;
        if self.observed_at_unix_ms == 0 || lease == 0 || lease > 300_000 {
            return Err("delta lease is outside its bounds".into());
        }
        let mut upserts = std::collections::BTreeSet::new();
        for message in &self.upserts {
            message.validate()?;
            if message.account_id != self.account_id {
                return Err("delta contains a message from another account".into());
            }
            if !upserts.insert(message.message_id.clone()) {
                return Err("duplicate delta message".into());
            }
        }
        let mut expunged = std::collections::BTreeSet::new();
        for message_id in &self.expunged {
            message_id_value(message_id)?;
            if !expunged.insert(message_id.clone()) {
                return Err("duplicate expunged message".into());
            }
        }
        if upserts.intersection(&expunged).next().is_some() {
            return Err("a message cannot be both upserted and expunged".into());
        }
        Ok(())
    }
}

/// An authorized request for retained message text. The adapter replies only to
/// the caller that already holds `chat.fetch`; the text is never broadcast.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FetchRequest {
    pub schema_version: u32,
    pub request_id: String,
    pub account_id: String,
    pub channel: String,
    pub message_id: String,
    pub max_bytes: u64,
}

impl FetchRequest {
    pub fn validate(&self) -> Result<(), String> {
        if self.schema_version != SCHEMA_VERSION {
            return Err("unsupported fetch schema".into());
        }
        id(&self.request_id, "request_id", MAX_ACCOUNT_ID)?;
        id(&self.account_id, "account_id", MAX_ACCOUNT_ID)?;
        channel_name(&self.channel)?;
        message_id(&self.message_id)?;
        if self.max_bytes == 0 || self.max_bytes > MAX_FETCH_BYTES {
            return Err("max_bytes is outside its bounds".into());
        }
        Ok(())
    }
}

/// The bounded reply metadata. Text bytes travel in the Unibus binary payload,
/// never inside this JSON document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FetchResponse {
    pub schema_version: u32,
    pub request_id: String,
    pub account_id: String,
    pub channel: String,
    pub message_id: String,
    pub content_type: String,
    pub size_bytes: u64,
    pub truncated: bool,
}

impl FetchResponse {
    pub fn validate(&self, payload_len: usize) -> Result<(), String> {
        if self.schema_version != SCHEMA_VERSION {
            return Err("unsupported fetch reply schema".into());
        }
        id(&self.request_id, "request_id", MAX_ACCOUNT_ID)?;
        id(&self.account_id, "account_id", MAX_ACCOUNT_ID)?;
        channel_name(&self.channel)?;
        message_id(&self.message_id)?;
        if self.content_type.is_empty()
            || self.content_type.len() > 64
            || !self
                .content_type
                .bytes()
                .all(|byte| byte.is_ascii_graphic() || byte == b' ')
        {
            return Err("invalid content_type".into());
        }
        if self.size_bytes != payload_len as u64 {
            return Err("size_bytes does not match the payload".into());
        }
        if self.size_bytes > MAX_FETCH_BYTES {
            return Err("fetch reply exceeds its bound".into());
        }
        Ok(())
    }
}

fn id(value: &str, field: &str, max: usize) -> Result<(), String> {
    if value.is_empty()
        || value.len() > max
        || value
            .bytes()
            .any(|byte| !(byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.')))
    {
        return Err(format!("invalid {field}"));
    }
    Ok(())
}

fn identifier(value: &str, field: &str, max: usize) -> Result<(), String> {
    if value.is_empty()
        || value.len() > max
        || value
            .bytes()
            .any(|byte| !(byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.')))
    {
        return Err(format!("invalid {field}"));
    }
    Ok(())
}

fn host(value: &str) -> Result<(), String> {
    if value.is_empty()
        || value.len() > MAX_HOST
        || value
            .chars()
            .any(|c| c.is_control() || c.is_whitespace() || matches!(c, '/' | '@' | '#' | '?'))
    {
        return Err("invalid host".into());
    }
    Ok(())
}

fn nick(value: &str) -> Result<(), String> {
    if value.is_empty()
        || value.len() > MAX_NICK
        || value
            .chars()
            .any(|c| c.is_control() || c.is_whitespace() || matches!(c, ',' | ':' | '#' | '&'))
    {
        return Err("invalid nick".into());
    }
    Ok(())
}

fn channel_name(value: &str) -> Result<(), String> {
    if value.len() < 2
        || value.len() > MAX_CHANNEL
        || !matches!(value.as_bytes()[0], b'#' | b'&')
        || value
            .chars()
            .any(|c| c.is_control() || c.is_whitespace() || matches!(c, ',' | ':'))
    {
        return Err("invalid channel".into());
    }
    Ok(())
}

fn credential_ref(value: &str) -> Result<(), String> {
    if value.len() > MAX_CREDENTIAL_REF || value.contains(['@', '?', '#']) {
        return Err("invalid credential reference".into());
    }
    let Some((scheme, location)) = value.split_once("://") else {
        return Err("credential reference needs a provider scheme".into());
    };
    if !matches!(scheme, "geode" | "env") {
        return Err("unsupported credential provider".into());
    }
    if location.is_empty() {
        return Err("credential reference has no location".into());
    }
    Ok(())
}

fn message_id(value: &str) -> Result<(), String> {
    message_id_value(value)
}

fn message_id_value(value: &str) -> Result<(), String> {
    if value.is_empty()
        || value.len() > MAX_MESSAGE_ID
        || value
            .bytes()
            .any(|byte| !(byte.is_ascii_graphic()) || byte == b'/' || byte == b'\\')
    {
        return Err("invalid message_id".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account() -> Account {
        Account {
            schema_version: SCHEMA_VERSION,
            account_id: "irc-fleet".into(),
            transport: Transport::Irc,
            network: "fleet".into(),
            host: "irc.example.net".into(),
            port: 6697,
            tls: true,
            nick: "mycelium".into(),
            credential_ref: "geode://chat/fleet/irc".into(),
            channels: vec!["#agents".into(), "#ops".into()],
            reconnect_interval_ms: 15_000,
            capabilities: Transport::Irc.maximum_capabilities(),
        }
    }

    fn message(id: &str) -> Message {
        Message {
            schema_version: SCHEMA_VERSION,
            account_id: "irc-fleet".into(),
            channel: "#agents".into(),
            message_id: id.into(),
            identity: Identity::Server,
            sender: "avery".into(),
            sent_at_unix_ms: 1_700_000_000_000,
            kind: MessageKind::Message,
            body_bytes: 12,
            reply_to: None,
            flags: Vec::new(),
        }
    }

    #[test]
    fn account_round_trips_and_rejects_unsafe_values() {
        let account = account();
        account.validate().unwrap();
        let encoded = serde_json::to_string(&account).unwrap();
        assert_eq!(serde_json::from_str::<Account>(&encoded).unwrap(), account);

        let mut invalid = account.clone();
        invalid.credential_ref = "geode://user:pass@chat/fleet/irc".into();
        assert!(invalid.validate().is_err());
        invalid = account.clone();
        invalid.channels = vec!["agents".into()];
        assert!(invalid.validate().is_err());
        invalid = account.clone();
        invalid.channels = vec!["#Agents".into(), "#agents".into()];
        assert!(invalid.validate().is_err());
        invalid = account;
        invalid.transport = Transport::Irc;
        invalid.capabilities.history = true;
        assert!(invalid.validate().is_err());
    }

    #[test]
    fn message_is_metadata_only_and_bounded() {
        let message = message("$server-id");
        message.validate().unwrap();
        let json = serde_json::to_string(&message).unwrap();
        assert!(!json.contains("\"body\":"));
        assert!(!json.contains("\"text\":"));

        let mut invalid = message.clone();
        invalid.message_id = "bad id".into();
        assert!(invalid.validate().is_err());
        invalid = message.clone();
        invalid.body_bytes = MAX_FETCH_BYTES + 1;
        assert!(invalid.validate().is_err());
        invalid = message;
        invalid.flags = vec!["a".into(), "a".into()];
        assert!(invalid.validate().is_err());
    }

    #[test]
    fn delta_is_consistent_and_lease_bounded() {
        let delta = Delta {
            schema_version: SCHEMA_VERSION,
            account_id: "irc-fleet".into(),
            generation: "conn-1".into(),
            cursor: 9,
            upserts: vec![message("$1"), message("$2")],
            expunged: vec!["$3".into()],
            observed_at_unix_ms: 1_700_000_000_000,
            valid_until_unix_ms: 1_700_000_030_000,
        };
        delta.validate().unwrap();

        let mut invalid = delta.clone();
        invalid.upserts.push(message("$2"));
        assert!(invalid.validate().is_err());
        invalid = delta.clone();
        invalid.expunged.push("$2".into());
        assert!(invalid.validate().is_err());
        invalid = delta;
        invalid.valid_until_unix_ms = invalid.observed_at_unix_ms;
        assert!(invalid.validate().is_err());
    }

    #[test]
    fn fetch_response_size_must_match_the_binary_payload() {
        let response = FetchResponse {
            schema_version: SCHEMA_VERSION,
            request_id: "r-1".into(),
            account_id: "irc-fleet".into(),
            channel: "#agents".into(),
            message_id: "$1".into(),
            content_type: "text/plain; charset=utf-8".into(),
            size_bytes: 5,
            truncated: false,
        };
        response.validate(5).unwrap();
        assert!(response.validate(4).is_err());
    }
}
