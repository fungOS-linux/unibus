//! A bounded IRC session for the Unibus chat bridge.
//!
//! The adapter owns the IRC connection. This crate owns the protocol subset it
//! needs and projects each event into [`unibus_chat::Message`] metadata:
//!
//! - CAP 302 negotiation for `message-tags` and `server-time`, best effort.
//! - NICK/USER registration, JOIN, PING/PONG, PRIVMSG/NOTICE/ACTION, JOIN/PART.
//! - Stable identity from IRCv3 `msgid`; otherwise a generation-scoped local
//!   identity that is explicitly not deduplicable across reconnects.
//! - A bounded in-memory body ring so `chat.fetch` can answer without putting
//!   text on the bus. Eviction is explicit, never silent.
//!
//! No authentication secret is logged. Credentials are supplied by the caller;
//! the host resolves a `geode://` reference before connecting.

use std::collections::{BTreeMap, VecDeque};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail, ensure};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use unibus_chat::{Identity, MessageKind};

use unibus_chat::Account;

/// Maximum IRC line accepted from upstream, excluding the line terminator.
pub const MAX_LINE: usize = 8192;
/// Default retained text budget for fetch replies.
pub const DEFAULT_RING_MESSAGES: usize = 1024;
pub const DEFAULT_RING_BYTES: usize = 8 * 1024 * 1024;

/// An IRCv3-tagged line, parsed only as far as the bridge needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IrcLine {
    pub tags: BTreeMap<String, String>,
    pub prefix: Option<String>,
    pub command: String,
    pub params: Vec<String>,
}

impl IrcLine {
    pub fn parse(line: &str) -> Result<Self> {
        let mut rest = line;
        let mut tags = BTreeMap::new();
        if let Some(tagged) = rest.strip_prefix('@') {
            let (tag_text, tail) = tagged
                .split_once(' ')
                .context("IRC tag block is not terminated")?;
            for pair in tag_text.split(';') {
                if pair.is_empty() {
                    continue;
                }
                let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
                if key.is_empty() || key.len() > 128 {
                    bail!("invalid IRC tag key");
                }
                tags.insert(unescape_tag(key), unescape_tag(value));
            }
            rest = tail;
        }
        let mut prefix = None;
        if let Some(prefixed) = rest.strip_prefix(':') {
            let (value, tail) = prefixed
                .split_once(' ')
                .context("IRC prefix is not terminated")?;
            ensure!(
                !value.is_empty() && value.len() <= 512,
                "invalid IRC prefix"
            );
            prefix = Some(value.to_owned());
            rest = tail;
        }
        let (command, params) = match rest.split_once(' ') {
            None => (rest, Vec::new()),
            Some((command, tail)) => {
                let mut params = Vec::new();
                let mut remaining = tail;
                loop {
                    if let Some(trailing) = remaining.strip_prefix(':') {
                        params.push(trailing.to_owned());
                        break;
                    }
                    match remaining.split_once(' ') {
                        None => {
                            if !remaining.is_empty() {
                                params.push(remaining.to_owned());
                            }
                            break;
                        }
                        Some((param, rest)) => {
                            params.push(param.to_owned());
                            remaining = rest;
                        }
                    }
                }
                (command, params)
            }
        };
        ensure!(!command.is_empty(), "IRC command is empty");
        ensure!(
            command
                .bytes()
                .all(|byte| byte.is_ascii_alphabetic() || byte.is_ascii_digit()),
            "invalid IRC command"
        );
        ensure!(params.len() <= 15, "too many IRC parameters");
        Ok(Self {
            tags,
            prefix,
            command: command.to_ascii_uppercase(),
            params,
        })
    }

    pub fn nick(&self) -> Option<&str> {
        self.prefix
            .as_deref()
            .map(|prefix| prefix.split('!').next().unwrap_or(prefix))
    }
}

fn unescape_tag(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some(':') => output.push(';'),
                Some('s') => output.push(' '),
                Some('\\') => output.push('\\'),
                Some('r') => output.push('\r'),
                Some('n') => output.push('\n'),
                Some(other) => output.push(other),
                None => output.push('\\'),
            }
        } else {
            output.push(c);
        }
    }
    output
}

/// One inbound message with its text retained locally for `chat.fetch`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Incoming {
    pub channel: String,
    pub message_id: String,
    pub identity: Identity,
    pub sender: String,
    pub sent_at_unix_ms: u64,
    pub kind: MessageKind,
    pub body: String,
    pub reply_to: Option<String>,
}

impl Incoming {
    pub fn metadata(&self, account_id: &str) -> unibus_chat::Message {
        unibus_chat::Message {
            schema_version: unibus_chat::SCHEMA_VERSION,
            account_id: account_id.to_owned(),
            channel: self.channel.clone(),
            message_id: self.message_id.clone(),
            identity: self.identity,
            sender: self.sender.clone(),
            sent_at_unix_ms: self.sent_at_unix_ms,
            kind: self.kind,
            body_bytes: self.body.len() as u64,
            reply_to: self.reply_to.clone(),
            flags: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IrcEvent {
    Ready { nick: String },
    Joined(String),
    Parted(String),
    Message(Incoming),
}

/// Bounded retained-text ring. Bodies are never published; they exist only to
/// answer an authorized `chat.fetch` for a message that is still retained.
#[derive(Debug)]
pub struct BodyRing {
    max_messages: usize,
    max_bytes: usize,
    bytes: usize,
    order: VecDeque<String>,
    entries: BTreeMap<String, Retained>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Retained {
    pub channel: String,
    pub body: String,
    pub retained_at_unix_ms: u64,
}

impl BodyRing {
    pub fn new(max_messages: usize, max_bytes: usize) -> Result<Self> {
        ensure!(max_messages > 0, "ring message bound must be positive");
        ensure!(max_bytes > 0, "ring byte bound must be positive");
        Ok(Self {
            max_messages,
            max_bytes,
            bytes: 0,
            order: VecDeque::new(),
            entries: BTreeMap::new(),
        })
    }

    pub fn default_ring() -> Self {
        Self::new(DEFAULT_RING_MESSAGES, DEFAULT_RING_BYTES).expect("valid defaults")
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn bytes(&self) -> usize {
        self.bytes
    }

    pub fn insert(
        &mut self,
        message_id: impl Into<String>,
        channel: impl Into<String>,
        body: String,
    ) {
        let message_id = message_id.into();
        if let Some(previous) = self.entries.remove(&message_id) {
            self.bytes = self.bytes.saturating_sub(previous.body.len());
            self.order.retain(|id| id != &message_id);
        }
        self.bytes += body.len();
        self.order.push_back(message_id.clone());
        self.entries.insert(
            message_id,
            Retained {
                channel: channel.into(),
                body,
                retained_at_unix_ms: now_ms(),
            },
        );
        while self.entries.len() > self.max_messages || self.bytes > self.max_bytes {
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            if let Some(entry) = self.entries.remove(&oldest) {
                self.bytes = self.bytes.saturating_sub(entry.body.len());
            }
        }
    }

    /// Return `(content_type, bytes, truncated)` for an authorized request.
    pub fn fetch(&self, request: &unibus_chat::FetchRequest) -> Option<(String, Vec<u8>, bool)> {
        let entry = self.entries.get(&request.message_id)?;
        if !entry.channel.eq_ignore_ascii_case(&request.channel) {
            return None;
        }
        let limit = request.max_bytes.min(unibus_chat::MAX_FETCH_BYTES) as usize;
        let mut end = entry.body.len().min(limit);
        while end > 0 && !entry.body.is_char_boundary(end) {
            end -= 1;
        }
        Some((
            "text/plain; charset=utf-8".into(),
            entry.body.as_bytes()[..end].to_vec(),
            end < entry.body.len(),
        ))
    }
}

impl Default for BodyRing {
    fn default() -> Self {
        Self::default_ring()
    }
}

/// IRC session over any async stream.
pub struct IrcSession<S> {
    reader: BufReader<tokio::io::ReadHalf<S>>,
    writer: tokio::io::WriteHalf<S>,
    account: Account,
    generation: String,
    sequence: u64,
    capabilities: BTreeMap<String, String>,
}

impl<S: AsyncRead + AsyncWrite + Unpin> IrcSession<S> {
    /// Register, negotiate optional IRCv3 capabilities, and join configured
    /// channels. `secret` is a server/SASL password already resolved by the
    /// host; it is never stored in this struct.
    pub async fn connect(
        stream: S,
        account: Account,
        secret: Option<&str>,
        generation: impl Into<String>,
    ) -> Result<Self> {
        account.validate().map_err(anyhow::Error::msg)?;
        let (reader, writer) = tokio::io::split(stream);
        let mut session = Self {
            reader: BufReader::new(reader),
            writer,
            account,
            generation: generation.into(),
            sequence: 0,
            capabilities: BTreeMap::new(),
        };
        session.raw("CAP LS 302").await?;
        if let Some(secret) = secret {
            ensure!(
                !secret.contains(['\r', '\n', '\0']),
                "IRC secret contains a line break"
            );
            session.raw(&format!("PASS {secret}")).await?;
        }
        session
            .raw(&format!("NICK {}", session.account.nick))
            .await?;
        session
            .raw(&format!(
                "USER {} 0 * :{}",
                session.account.nick, session.account.nick
            ))
            .await?;

        loop {
            let Some(line) = session.read_line().await? else {
                bail!("IRC closed during registration");
            };
            let parsed = IrcLine::parse(&line)?;
            match parsed.command.as_str() {
                "CAP" => {
                    let sub = parsed.params.get(1).map(String::as_str);
                    match sub {
                        Some("LS") | Some("NEW") => {
                            session.raw("CAP REQ :message-tags server-time").await?;
                        }
                        Some("ACK") => {
                            for capability in parsed
                                .params
                                .get(2)
                                .into_iter()
                                .flat_map(|v| v.split_whitespace())
                            {
                                session
                                    .capabilities
                                    .insert(capability.to_owned(), String::new());
                            }
                            session.raw("CAP END").await?;
                        }
                        Some("NAK") => {
                            session.raw("CAP END").await?;
                        }
                        _ => {}
                    }
                }
                "001" => {
                    session.raw("CAP END").await?;
                    break;
                }
                "PING" => {
                    session
                        .pong(parsed.params.last().map(String::as_str))
                        .await?
                }
                "ERROR" => bail!("IRC registration failed: {}", parsed.params.join(" ")),
                _ => {}
            }
        }
        for channel in session.account.channels.clone() {
            session.raw(&format!("JOIN {channel}")).await?;
        }
        Ok(session)
    }

    pub fn account(&self) -> &Account {
        &self.account
    }

    pub fn capabilities(&self) -> &BTreeMap<String, String> {
        &self.capabilities
    }

    /// Read the next semantic event. Protocol housekeeping (PING, CAP, MOTD) is
    /// handled here and never returned.
    pub async fn next_event(&mut self) -> Result<Option<IrcEvent>> {
        loop {
            let Some(line) = self.read_line().await? else {
                return Ok(None);
            };
            let parsed = IrcLine::parse(&line)?;
            match parsed.command.as_str() {
                "PING" => {
                    self.pong(parsed.params.last().map(String::as_str)).await?;
                }
                "PRIVMSG" => {
                    if let Some(event) = self.project_message(&parsed)? {
                        return Ok(Some(event));
                    }
                }
                "NOTICE" => {
                    if let Some(event) = self.project_notice(&parsed)? {
                        return Ok(Some(event));
                    }
                }
                "JOIN" => {
                    if let Some(channel) = parsed.params.first()
                        && self.account.has_channel(channel)
                    {
                        return Ok(Some(IrcEvent::Joined(self.canonical(channel))));
                    }
                }
                "PART" => {
                    if let Some(channel) = parsed.params.first()
                        && self.account.has_channel(channel)
                    {
                        return Ok(Some(IrcEvent::Parted(self.canonical(channel))));
                    }
                }
                "001" => {
                    let nick = parsed
                        .params
                        .first()
                        .cloned()
                        .unwrap_or_else(|| self.account.nick.clone());
                    return Ok(Some(IrcEvent::Ready { nick }));
                }
                "ERROR" => bail!("IRC error: {}", parsed.params.join(" ")),
                _ => {}
            }
        }
    }

    pub async fn send_message(&mut self, channel: &str, text: &str) -> Result<()> {
        ensure!(
            self.account.has_channel(channel),
            "channel is not configured"
        );
        ensure!(
            !text.contains(['\r', '\n', '\0']),
            "IRC text contains a line break"
        );
        self.raw(&format!("PRIVMSG {channel} :{text}")).await
    }

    fn project_message(&mut self, line: &IrcLine) -> Result<Option<IrcEvent>> {
        let Some(target) = line.params.first() else {
            return Ok(None);
        };
        if !target.starts_with('#') && !target.starts_with('&') {
            // Private messages are not part of the channel contract.
            return Ok(None);
        }
        if !self.account.has_channel(target) {
            return Ok(None);
        }
        let Some(body) = line.params.get(1) else {
            return Ok(None);
        };
        let (kind, body) = if let Some(action) = body
            .strip_prefix('\u{1}')
            .and_then(|value| value.strip_prefix("ACTION "))
            .and_then(|value| value.strip_suffix('\u{1}'))
        {
            (MessageKind::Action, action.to_owned())
        } else if body.starts_with('\u{1}') {
            // Other CTCP requests are protocol, not chat text.
            return Ok(None);
        } else {
            (MessageKind::Message, body.clone())
        };
        let sender = line.nick().unwrap_or("unknown").to_owned();
        let message_id = line.tags.get("msgid").filter(|id| !id.is_empty()).cloned();
        let (message_id, identity) = match message_id {
            Some(id) => (id, Identity::Server),
            None => {
                self.sequence = self.sequence.wrapping_add(1);
                (
                    format!("local:{}:{}", self.generation, self.sequence),
                    Identity::Local,
                )
            }
        };
        let sent_at_unix_ms = line
            .tags
            .get("time")
            .and_then(|value| parse_rfc3339_millis(value))
            .unwrap_or_else(now_ms);
        let reply_to = line
            .tags
            .get("+draft/reply")
            .or_else(|| line.tags.get("reply"))
            .cloned();
        Ok(Some(IrcEvent::Message(Incoming {
            channel: self.canonical(target),
            message_id,
            identity,
            sender,
            sent_at_unix_ms,
            kind,
            body,
            reply_to,
        })))
    }

    fn project_notice(&mut self, line: &IrcLine) -> Result<Option<IrcEvent>> {
        let Some(target) = line.params.first() else {
            return Ok(None);
        };
        if !self.account.has_channel(target) {
            return Ok(None);
        }
        let Some(body) = line.params.get(1) else {
            return Ok(None);
        };
        self.sequence = self.sequence.wrapping_add(1);
        let sent_at_unix_ms = line
            .tags
            .get("time")
            .and_then(|value| parse_rfc3339_millis(value))
            .unwrap_or_else(now_ms);
        Ok(Some(IrcEvent::Message(Incoming {
            channel: self.canonical(target),
            message_id: line
                .tags
                .get("msgid")
                .cloned()
                .unwrap_or_else(|| format!("local:{}:{}", self.generation, self.sequence)),
            identity: if line.tags.contains_key("msgid") {
                Identity::Server
            } else {
                Identity::Local
            },
            sender: line.nick().unwrap_or("unknown").to_owned(),
            sent_at_unix_ms,
            kind: MessageKind::Notice,
            body: body.clone(),
            reply_to: None,
        })))
    }

    fn canonical(&self, channel: &str) -> String {
        self.account
            .channels
            .iter()
            .find(|candidate| candidate.eq_ignore_ascii_case(channel))
            .cloned()
            .unwrap_or_else(|| channel.to_owned())
    }

    async fn pong(&mut self, token: Option<&str>) -> Result<()> {
        match token {
            Some(token) => self.raw(&format!("PONG :{token}")).await,
            None => self.raw("PONG").await,
        }
    }

    async fn read_line(&mut self) -> Result<Option<String>> {
        let mut line = String::new();
        let read = self
            .reader
            .read_line(&mut line)
            .await
            .context("read IRC line")?;
        if read == 0 {
            return Ok(None);
        }
        ensure!(read <= MAX_LINE, "IRC line exceeds bound");
        if line.ends_with('\n') {
            line.pop();
            if line.ends_with('\r') {
                line.pop();
            }
        }
        Ok(Some(line))
    }

    async fn raw(&mut self, line: &str) -> Result<()> {
        self.writer
            .write_all(line.as_bytes())
            .await
            .context("write IRC line")?;
        self.writer.write_all(b"\r\n").await?;
        self.writer.flush().await?;
        Ok(())
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

/// Parse the IRCv3 `server-time` form: `YYYY-MM-DDTHH:MM:SS[.fff]Z`.
pub fn parse_rfc3339_millis(value: &str) -> Option<u64> {
    let value = value.strip_suffix('Z')?;
    let (date, time) = value.split_once('T')?;
    let mut date = date.split('-');
    let year: i64 = date.next()?.parse().ok()?;
    let month: u32 = date.next()?.parse().ok()?;
    let day: u32 = date.next()?.parse().ok()?;
    if date.next().is_some() || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let (clock, fraction) = time.split_once('.').unwrap_or((time, "0"));
    let mut clock = clock.split(':');
    let hour: u64 = clock.next()?.parse().ok()?;
    let minute: u64 = clock.next()?.parse().ok()?;
    let second: u64 = clock.next()?.parse().ok()?;
    if clock.next().is_some() || hour > 23 || minute > 59 || second > 60 {
        return None;
    }
    let millis: u64 = {
        let digits: String = fraction.chars().take(3).collect();
        let digits = format!("{digits:0<3}");
        digits.parse().ok()?
    };
    let days = days_from_civil(year, month, day)?;
    days.checked_mul(86_400)?
        .checked_add(hour * 3_600 + minute * 60 + second)?
        .checked_mul(1_000)?
        .checked_add(millis)
}

fn days_from_civil(year: i64, month: u32, day: u32) -> Option<u64> {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let yoe = (year - era * 400) as u64;
    let month = month as u64;
    let doy = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + day as u64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era as i128 * 146_097 + doe as i128 - 719_468;
    u64::try_from(days).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_tags_prefix_and_trailing_parameter() {
        let line = IrcLine::parse(
            "@msgid=abc;time=2026-11-04T23:00:00.250Z;+draft/reply=$1 :avery!u@h PRIVMSG #agents :hello : world",
        )
        .unwrap();
        assert_eq!(line.tags["msgid"], "abc");
        assert_eq!(line.nick(), Some("avery"));
        assert_eq!(line.command, "PRIVMSG");
        assert_eq!(line.params, ["#agents", "hello : world"]);
    }

    #[test]
    fn rfc3339_matches_known_epochs() {
        assert_eq!(parse_rfc3339_millis("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(
            parse_rfc3339_millis("2026-11-04T23:00:00.250Z"),
            Some(1_793_833_200_250)
        );
        assert!(parse_rfc3339_millis("not-a-time").is_none());
    }

    #[test]
    fn ring_evicts_by_messages_and_bytes() {
        let mut ring = BodyRing::new(2, 100).unwrap();
        ring.insert("a", "#agents", "one".into());
        ring.insert("b", "#agents", "two".into());
        ring.insert("c", "#agents", "three".into());
        assert_eq!(ring.len(), 2);
        assert!(!ring.entries.contains_key("a"));

        let mut small = BodyRing::new(10, 10).unwrap();
        small.insert("a", "#agents", "abcdef".into());
        assert_eq!(small.bytes(), 6);
        small.insert("b", "#agents", "xy".into());
        assert_eq!(small.bytes(), 8);
        // A single body larger than the whole budget is not retained at all.
        let mut too_small = BodyRing::new(10, 5).unwrap();
        too_small.insert("a", "#agents", "abcdef".into());
        assert_eq!(too_small.bytes(), 0);
        assert!(!too_small.entries.contains_key("a"));
    }

    #[test]
    fn fetch_is_channel_bound_and_truncates_on_utf8_boundaries() {
        let mut ring = BodyRing::new(10, 1000).unwrap();
        ring.insert("m1", "#agents", "héllo".into());
        let request = unibus_chat::FetchRequest {
            schema_version: unibus_chat::SCHEMA_VERSION,
            request_id: "r1".into(),
            account_id: "irc-fleet".into(),
            channel: "#agents".into(),
            message_id: "m1".into(),
            max_bytes: 2,
        };
        let (content_type, bytes, truncated) = ring.fetch(&request).unwrap();
        assert_eq!(content_type, "text/plain; charset=utf-8");
        assert_eq!(bytes, b"h");
        assert!(truncated);

        let wrong_channel = unibus_chat::FetchRequest {
            channel: "#ops".into(),
            ..request
        };
        assert!(ring.fetch(&wrong_channel).is_none());
    }

    #[test]
    fn projection_never_contains_the_body() {
        let incoming = Incoming {
            channel: "#agents".into(),
            message_id: "abc".into(),
            identity: Identity::Server,
            sender: "avery".into(),
            sent_at_unix_ms: 1,
            kind: MessageKind::Message,
            body: "secret text".into(),
            reply_to: None,
        };
        let metadata = incoming.metadata("irc-fleet");
        metadata.validate().unwrap();
        let json = serde_json::to_string(&metadata).unwrap();
        assert!(!json.contains("secret text"));
        assert_eq!(metadata.body_bytes, 11);
    }

    async fn next_server_line(
        lines: &mut tokio::io::Lines<BufReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>>,
    ) -> String {
        tokio::time::timeout(std::time::Duration::from_secs(2), lines.next_line())
            .await
            .expect("server read timed out")
            .expect("server read failed")
            .expect("server stream closed")
    }

    #[tokio::test]
    async fn registers_negotiates_and_emits_tagged_messages() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, duplex};

        let (client, server) = duplex(4096);
        let server_task = tokio::spawn(async move {
            let (read, mut write) = tokio::io::split(server);
            let mut lines = BufReader::new(read).lines();
            let mut announced = false;
            loop {
                let line = next_server_line(&mut lines).await;
                match line.as_str() {
                    "CAP LS 302" => {
                        write
                            .write_all(b":srv CAP * LS :message-tags server-time\r\n")
                            .await
                            .unwrap();
                    }
                    "CAP REQ :message-tags server-time" => {
                        write
                            .write_all(b":srv CAP * ACK :message-tags server-time\r\n")
                            .await
                            .unwrap();
                    }
                    "CAP END" if !announced => {
                        announced = true;
                        write
                            .write_all(b":srv 001 mycelium :welcome\r\n")
                            .await
                            .unwrap();
                    }
                    "CAP END" => {}
                    line if line.starts_with("NICK ") || line.starts_with("USER ") => {}
                    line if line.starts_with("JOIN ") => {
                        write
                            .write_all(b":mycelium!u@h JOIN #agents\r\n")
                            .await
                            .unwrap();
                        write.write_all(b"PING :tok\r\n").await.unwrap();
                        assert_eq!(next_server_line(&mut lines).await, "PONG :tok");
                        write
                            .write_all(
                                b"@msgid=abc;time=2026-11-04T23:00:00.000Z;+draft/reply=$0 :avery!u@h PRIVMSG #agents :hello\r\n",
                            )
                            .await
                            .unwrap();
                        return;
                    }
                    _ => {}
                }
            }
        });

        let mut account = tests_account();
        account.channels = vec!["#agents".into()];
        let mut session = IrcSession::connect(client, account, None, "conn-1")
            .await
            .unwrap();
        assert_eq!(
            session.next_event().await.unwrap(),
            Some(IrcEvent::Joined("#agents".into()))
        );
        let Some(IrcEvent::Message(message)) = session.next_event().await.unwrap() else {
            panic!("expected a chat message");
        };
        assert_eq!(message.channel, "#agents");
        assert_eq!(message.message_id, "abc");
        assert_eq!(message.identity, Identity::Server);
        assert_eq!(message.sender, "avery");
        assert_eq!(message.kind, MessageKind::Message);
        assert_eq!(message.body, "hello");
        assert_eq!(message.sent_at_unix_ms, 1_793_833_200_000);
        assert_eq!(message.reply_to.as_deref(), Some("$0"));
        server_task.await.unwrap();
    }

    fn tests_account() -> Account {
        Account {
            schema_version: unibus_chat::SCHEMA_VERSION,
            account_id: "irc-fleet".into(),
            transport: unibus_chat::Transport::Irc,
            network: "fleet".into(),
            host: "irc.example.net".into(),
            port: 6697,
            tls: true,
            nick: "mycelium".into(),
            credential_ref: String::new(),
            channels: vec!["#agents".into()],
            reconnect_interval_ms: 15_000,
            capabilities: unibus_chat::Transport::Irc.maximum_capabilities(),
        }
    }
}
