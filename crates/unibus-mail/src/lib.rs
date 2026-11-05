//! Bounded POP3/IMAP metadata contracts for the Unibus mail bridge.
//!
//! The bridge owns the mailbox session. This crate owns only the small,
//! versioned documents that may cross the bus:
//!
//! - [`Account`] describes a mailbox and its non-secret identity.
//! - [`Message`] is a metadata notification, never a body.
//! - [`Delta`] is one bounded change window for a mailbox.
//! - [`FetchRequest`] asks the owning adapter for a specific part.
//!
//! Bodies, attachments, passwords, OAuth tokens, and full upstream headers do
//! not belong in any document here. A credential is named with a
//! `geode://...` reference and resolved only by the local owner process.

use serde::{Deserialize, Serialize};

pub const ACCOUNT_KIND: &str = "email.account.v1";
pub const MESSAGE_KIND: &str = "email.message.v1";
pub const DELTA_KIND: &str = "email.delta.v1";
pub const FETCH_KIND: &str = "email.fetch.v1";
pub const FETCH_REPLY_KIND: &str = "email.fetch.reply.v1";

pub const SCHEMA_VERSION: u32 = 1;
pub const MAX_ACCOUNT_ID: usize = 128;
pub const MAX_MAILBOX: usize = 256;
pub const MAX_GENERATION: usize = 128;
pub const MAX_HOST: usize = 253;
pub const MAX_USERNAME: usize = 320;
pub const MAX_CREDENTIAL_REF: usize = 256;
pub const MAX_SUBJECT: usize = 998;
pub const MAX_FLAGS: usize = 32;
pub const MAX_DELTA_MESSAGES: usize = 512;
pub const MAX_FETCH_BYTES: u64 = 8 * 1024 * 1024;

/// A protocol the bridge can own. POP3 is intentionally the degraded mode:
/// inbox only, poll only, no folders, no flags, no push.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Protocol {
    Imap,
    Pop3,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Capabilities {
    /// The adapter can observe new mail without polling (IMAP IDLE).
    pub push: bool,
    /// More than one mailbox is addressable.
    pub folders: bool,
    /// Per-message flags are authoritative and can be changed.
    pub flags: bool,
    /// The adapter can expunge messages.
    pub delete: bool,
    /// The server exposes stable UIDLs (POP3).
    pub uidl: bool,
}

impl Protocol {
    /// Capabilities a protocol can support at most. A server or configuration
    /// may negotiate less; it may never claim more.
    pub const fn maximum_capabilities(self) -> Capabilities {
        match self {
            Self::Imap => Capabilities {
                push: true,
                folders: true,
                flags: true,
                delete: true,
                uidl: false,
            },
            Self::Pop3 => Capabilities {
                push: false,
                folders: false,
                flags: false,
                delete: true,
                uidl: true,
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Account {
    pub schema_version: u32,
    pub account_id: String,
    pub protocol: Protocol,
    pub host: String,
    pub port: u16,
    pub tls: bool,
    /// The login name. This is not a secret and may be an email address, but
    /// it is still local metadata and is never logged by the bridge.
    pub username: String,
    /// A `geode://...` reference. No password, token, or URI user-info.
    pub credential_ref: String,
    /// Mailboxes the bridge is allowed to observe. Empty means the protocol's
    /// inbox only, never server-wide discovery.
    #[serde(default)]
    pub folders: Vec<String>,
    /// Poll cadence for protocols without push. The adapter bounds this.
    pub poll_interval_ms: u64,
    pub capabilities: Capabilities,
}

impl Account {
    pub fn validate(&self) -> Result<(), String> {
        if self.schema_version != SCHEMA_VERSION {
            return Err("unsupported account schema".into());
        }
        id(&self.account_id, "account_id", MAX_ACCOUNT_ID)?;
        host(&self.host)?;
        if self.port == 0 {
            return Err("port is required".into());
        }
        username(&self.username)?;
        credential_ref(&self.credential_ref)?;
        if !(1_000..=86_400_000).contains(&self.poll_interval_ms) {
            return Err("poll_interval_ms is outside the bounded range".into());
        }
        if self.folders.len() > 64 {
            return Err("too many folders".into());
        }
        let mut seen = std::collections::BTreeSet::new();
        for folder in &self.folders {
            mailbox(folder)?;
            if !seen.insert(folder) {
                return Err("duplicate folder".into());
            }
        }
        self.capabilities.validate(self.protocol)?;
        Ok(())
    }
}

impl Capabilities {
    pub fn validate(&self, protocol: Protocol) -> Result<(), String> {
        let maximum = protocol.maximum_capabilities();
        for (name, claimed, allowed) in [
            ("push", self.push, maximum.push),
            ("folders", self.folders, maximum.folders),
            ("flags", self.flags, maximum.flags),
            ("delete", self.delete, maximum.delete),
            ("uidl", self.uidl, maximum.uidl),
        ] {
            if claimed && !allowed {
                return Err(format!("{protocol:?} cannot claim {name} capability"));
            }
        }
        Ok(())
    }
}

/// Optional presentation metadata. Absent means the bridge was configured not
/// to publish headers at all. Subject is the most sensitive field here; the
/// host should treat it as a notification, not an index.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Metadata {
    #[serde(default)]
    pub from_domain: Option<String>,
    #[serde(default)]
    pub subject: Option<String>,
}

impl Metadata {
    pub fn validate(&self) -> Result<(), String> {
        if self.from_domain.is_none() && self.subject.is_none() {
            return Err("metadata must contain at least one field".into());
        }
        if let Some(domain) = &self.from_domain {
            domain_name(domain)?;
        }
        if let Some(subject) = &self.subject
            && (subject.is_empty()
                || subject.len() > MAX_SUBJECT
                || subject
                    .chars()
                    .any(|c| c == '\0' || (c.is_control() && c != '\t')))
        {
            return Err("subject is empty or outside its bounds".into());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Message {
    pub schema_version: u32,
    pub account_id: String,
    pub mailbox: String,
    /// IMAP UIDVALIDITY. POP3 has no such value and sends zero.
    pub uidvalidity: u64,
    /// IMAP UID or the adapter's stable local sequence for POP3.
    pub uid: u64,
    /// POP3 UIDL when the server exposes one.
    #[serde(default)]
    pub uidl: Option<String>,
    pub size_bytes: u64,
    pub received_at_unix_ms: u64,
    #[serde(default)]
    pub flags: Vec<String>,
    #[serde(default)]
    pub metadata: Option<Metadata>,
}

impl Message {
    pub fn validate(&self) -> Result<(), String> {
        if self.schema_version != SCHEMA_VERSION {
            return Err("unsupported message schema".into());
        }
        id(&self.account_id, "account_id", MAX_ACCOUNT_ID)?;
        mailbox(&self.mailbox)?;
        if self.uid == 0 {
            return Err("uid must be positive".into());
        }
        if let Some(uidl) = &self.uidl {
            uidl_value(uidl)?;
        }
        if self.size_bytes > i64::MAX as u64 {
            return Err("message size is outside its bounds".into());
        }
        if self.flags.len() > MAX_FLAGS {
            return Err("too many flags".into());
        }
        let mut seen = std::collections::BTreeSet::new();
        for flag in &self.flags {
            flag_token(flag)?;
            if !seen.insert(flag) {
                return Err("duplicate flag".into());
            }
        }
        if let Some(metadata) = &self.metadata {
            metadata.validate()?;
        }
        Ok(())
    }
}

/// One bounded window of mailbox change. `cursor` is the highest stable
/// sequence included; consumers advance only when the whole delta validates.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Delta {
    pub schema_version: u32,
    pub account_id: String,
    pub mailbox: String,
    /// Mailbox epoch. IMAP UIDVALIDITY; the adapter's stable epoch for POP3.
    pub generation: String,
    pub cursor: u64,
    #[serde(default)]
    pub upserts: Vec<Message>,
    #[serde(default)]
    pub expunged: Vec<u64>,
    pub observed_at_unix_ms: u64,
    pub valid_until_unix_ms: u64,
}

impl Delta {
    pub fn validate(&self) -> Result<(), String> {
        if self.schema_version != SCHEMA_VERSION {
            return Err("unsupported delta schema".into());
        }
        id(&self.account_id, "account_id", MAX_ACCOUNT_ID)?;
        mailbox(&self.mailbox)?;
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
            if message.account_id != self.account_id || message.mailbox != self.mailbox {
                return Err("delta contains a message from another mailbox".into());
            }
            if message.uid > self.cursor {
                return Err("delta message is ahead of the cursor".into());
            }
            if !upserts.insert(message.uid) {
                return Err("duplicate delta message".into());
            }
        }
        let mut expunged = std::collections::BTreeSet::new();
        for uid in &self.expunged {
            if *uid == 0 || *uid > self.cursor {
                return Err("expunged uid is outside the cursor".into());
            }
            if !expunged.insert(*uid) {
                return Err("duplicate expunged uid".into());
            }
        }
        if upserts.iter().any(|uid| expunged.contains(uid)) {
            return Err("a message cannot be both upserted and expunged".into());
        }
        Ok(())
    }

    /// The newest timestamp in this window, for a consumer that needs a single
    /// "as of" value without reading every message.
    pub fn latest_received_at(&self) -> Option<u64> {
        self.upserts
            .iter()
            .map(|message| message.received_at_unix_ms)
            .max()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FetchPart {
    Headers,
    Text,
    Html,
    Attachments,
}

/// An authorized request for message content. The adapter returns the result
/// only to the caller that already holds `mail.fetch`; it is never broadcast.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FetchRequest {
    pub schema_version: u32,
    pub request_id: String,
    pub account_id: String,
    pub mailbox: String,
    pub uid: u64,
    pub parts: Vec<FetchPart>,
    pub max_bytes: u64,
}

impl FetchRequest {
    pub fn validate(&self) -> Result<(), String> {
        if self.schema_version != SCHEMA_VERSION {
            return Err("unsupported fetch schema".into());
        }
        id(&self.request_id, "request_id", MAX_ACCOUNT_ID)?;
        id(&self.account_id, "account_id", MAX_ACCOUNT_ID)?;
        mailbox(&self.mailbox)?;
        if self.uid == 0 {
            return Err("uid must be positive".into());
        }
        if self.parts.is_empty() || self.parts.len() > 4 {
            return Err("fetch parts must contain between one and four entries".into());
        }
        let mut seen = std::collections::BTreeSet::new();
        for part in &self.parts {
            if !seen.insert(*part as u8) {
                return Err("duplicate fetch part".into());
            }
        }
        if self.max_bytes == 0 || self.max_bytes > MAX_FETCH_BYTES {
            return Err("max_bytes is outside its bounds".into());
        }
        Ok(())
    }
}

/// Reply to an [`FetchRequest`]. The bounded content travels in the Unibus
/// binary payload, never inside this JSON document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FetchResponse {
    pub schema_version: u32,
    pub request_id: String,
    pub account_id: String,
    pub mailbox: String,
    pub uid: u64,
    /// v1 returns the bounded raw message (`message/rfc822`) rather than
    /// parsing MIME parts.
    pub content_type: String,
    pub size_bytes: u64,
    pub truncated: bool,
    #[serde(default = "default_true")]
    pub retained: bool,
}

fn default_true() -> bool {
    true
}

impl FetchResponse {
    pub fn validate(&self, payload_len: usize) -> Result<(), String> {
        if self.schema_version != SCHEMA_VERSION {
            return Err("unsupported fetch reply schema".into());
        }
        id(&self.request_id, "request_id", MAX_ACCOUNT_ID)?;
        id(&self.account_id, "account_id", MAX_ACCOUNT_ID)?;
        mailbox(&self.mailbox)?;
        if self.uid == 0 {
            return Err("uid must be positive".into());
        }
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
        if !self.retained && self.size_bytes != 0 {
            return Err("a not-retained reply cannot carry bytes".into());
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

/// IMAP system flags are written with a leading backslash (`\Seen`); custom
/// flags and keywords are bare tokens. Both must be bounded and printable.
fn flag_token(value: &str) -> Result<(), String> {
    if value == "\\*" {
        return Ok(());
    }
    let name = value.strip_prefix('\\').unwrap_or(value);
    if name.is_empty()
        || name.len() > 64
        || name.bytes().any(|byte| {
            !(byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'$'))
        })
    {
        return Err("invalid flag".into());
    }
    Ok(())
}

fn token(value: &str, field: &str, max: usize) -> Result<(), String> {
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
            .bytes()
            .any(|byte| !(byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.')))
        || value.starts_with('.')
        || value.ends_with('.')
    {
        return Err("invalid host".into());
    }
    Ok(())
}

fn username(value: &str) -> Result<(), String> {
    if value.is_empty()
        || value.len() > MAX_USERNAME
        || value.chars().any(|c| c.is_control() || c == '\0')
    {
        return Err("invalid username".into());
    }
    Ok(())
}

fn mailbox(value: &str) -> Result<(), String> {
    if value.is_empty()
        || value.len() > MAX_MAILBOX
        || value.chars().any(|c| c == '\0' || c.is_control())
    {
        return Err("invalid mailbox".into());
    }
    Ok(())
}

fn domain_name(value: &str) -> Result<(), String> {
    if value.is_empty()
        || value.len() > MAX_HOST
        || value
            .bytes()
            .any(|byte| !(byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.')))
        || value.starts_with('.')
        || value.ends_with('.')
    {
        return Err("invalid from_domain".into());
    }
    Ok(())
}

fn credential_ref(value: &str) -> Result<(), String> {
    if value.len() > MAX_CREDENTIAL_REF
        || value.bytes().any(|byte| byte.is_ascii_whitespace())
        || value.chars().any(char::is_control)
        || value.contains(['@', '?', '#'])
    {
        return Err("credential ref contains unsafe syntax".into());
    }
    let Some((scheme, location)) = value.split_once("://") else {
        return Err("credential ref needs a geode:// scheme".into());
    };
    if scheme != "geode" {
        return Err("credential ref must use geode://".into());
    }
    if location.is_empty() || location.len() > MAX_CREDENTIAL_REF - scheme.len() - 3 {
        return Err("credential ref is outside its bounds".into());
    }
    for segment in location.split('/') {
        if segment.is_empty() || matches!(segment, "." | "..") {
            return Err("credential ref contains an invalid path segment".into());
        }
        token(segment, "credential ref", 64)?;
    }
    Ok(())
}

fn uidl_value(value: &str) -> Result<(), String> {
    if value.is_empty()
        || value.len() > 128
        || value.bytes().any(|byte| !(0x21..=0x7e).contains(&byte))
    {
        return Err("invalid uidl".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account(protocol: Protocol) -> Account {
        Account {
            schema_version: SCHEMA_VERSION,
            account_id: "work-mail".into(),
            protocol,
            host: "imap.example.com".into(),
            port: 993,
            tls: true,
            username: "avery@example.com".into(),
            credential_ref: "geode://mail/work/imap".into(),
            folders: vec!["INBOX".into()],
            poll_interval_ms: 30_000,
            capabilities: protocol.maximum_capabilities(),
        }
    }

    fn message() -> Message {
        Message {
            schema_version: SCHEMA_VERSION,
            account_id: "work-mail".into(),
            mailbox: "INBOX".into(),
            uidvalidity: 42,
            uid: 7,
            uidl: None,
            size_bytes: 4096,
            received_at_unix_ms: 1_700_000_000_000,
            flags: vec!["seen".into(), "answered".into()],
            metadata: Some(Metadata {
                from_domain: Some("example.com".into()),
                subject: Some("Quarterly plan".into()),
            }),
        }
    }

    fn delta() -> Delta {
        Delta {
            schema_version: SCHEMA_VERSION,
            account_id: "work-mail".into(),
            mailbox: "INBOX".into(),
            generation: "42".into(),
            cursor: 7,
            upserts: vec![message()],
            expunged: vec![3],
            observed_at_unix_ms: 1_700_000_000_000,
            valid_until_unix_ms: 1_700_000_060_000,
        }
    }

    #[test]
    fn account_round_trips_and_rejects_secret_bearing_refs() {
        let account = account(Protocol::Imap);
        account.validate().unwrap();
        let encoded = serde_json::to_string(&account).unwrap();
        assert!(!encoded.contains("password"));
        let decoded: Account = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, account);

        let mut bad = account.clone();
        bad.credential_ref = "geode://user:pass@mail/work".into();
        assert!(bad.validate().is_err());
        bad.credential_ref = "env://MAIL_PASSWORD".into();
        assert!(bad.validate().is_err());
    }

    #[test]
    fn pop3_cannot_claim_imap_capabilities() {
        let mut pop = account(Protocol::Pop3);
        pop.capabilities = Protocol::Imap.maximum_capabilities();
        assert!(pop.validate().is_err());
        pop.capabilities = Protocol::Pop3.maximum_capabilities();
        pop.validate().unwrap();
    }

    #[test]
    fn message_round_trips_without_a_body_field() {
        let message = message();
        message.validate().unwrap();
        let value = serde_json::to_value(&message).unwrap();
        assert!(value.get("body").is_none());
        assert_eq!(serde_json::from_value::<Message>(value).unwrap(), message);
    }

    #[test]
    fn imap_system_and_custom_flags_are_valid() {
        let mut message = message();
        message.flags = vec!["\\Seen".into(), "\\Flagged".into(), "$Forwarded".into()];
        message.validate().unwrap();
        message.flags = vec!["\\*".into()];
        message.validate().unwrap();
        message.flags = vec!["bad flag".into()];
        assert!(message.validate().is_err());
    }

    #[test]
    fn metadata_must_not_be_empty() {
        let mut message = message();
        message.metadata = Some(Metadata {
            from_domain: None,
            subject: None,
        });
        assert!(message.validate().is_err());
        message.metadata = None;
        message.validate().unwrap();
    }

    #[test]
    fn delta_requires_a_bounded_lease_and_a_consistent_mailbox() {
        let delta = delta();
        delta.validate().unwrap();

        let mut long = delta.clone();
        long.valid_until_unix_ms = long.observed_at_unix_ms + 300_001;
        assert!(long.validate().is_err());

        let mut other = delta.clone();
        other.upserts[0].mailbox = "Archive".into();
        assert!(other.validate().is_err());

        let mut ahead = delta;
        ahead.cursor = 6;
        assert!(ahead.validate().is_err());
    }

    #[test]
    fn expunged_and_upserted_uids_are_disjoint_and_unique() {
        let mut delta = delta();
        delta.expunged = vec![7];
        assert!(delta.validate().is_err());

        delta.expunged = vec![3, 3];
        assert!(delta.validate().is_err());

        delta.expunged = vec![3];
        delta.validate().unwrap();
    }

    #[test]
    fn fetch_requests_are_bounded_and_deduplicated() {
        let request = FetchRequest {
            schema_version: SCHEMA_VERSION,
            request_id: "fetch-1".into(),
            account_id: "work-mail".into(),
            mailbox: "INBOX".into(),
            uid: 7,
            parts: vec![FetchPart::Headers, FetchPart::Text],
            max_bytes: 64 * 1024,
        };
        request.validate().unwrap();

        let mut duplicate = request.clone();
        duplicate.parts = vec![FetchPart::Text, FetchPart::Text];
        assert!(duplicate.validate().is_err());

        let mut too_large = request;
        too_large.max_bytes = MAX_FETCH_BYTES + 1;
        assert!(too_large.validate().is_err());
    }

    #[test]
    fn unknown_fields_cannot_smuggle_a_body_or_a_secret() {
        let value = serde_json::json!({
            "schema_version": 1,
            "account_id": "work-mail",
            "mailbox": "INBOX",
            "uidvalidity": 42,
            "uid": 7,
            "size_bytes": 4096,
            "received_at_unix_ms": 1_700_000_000_000_u64,
            "body": "secret"
        });
        assert!(serde_json::from_value::<Message>(value).is_err());
    }
}
