# Chat bridge runbook (IRC first)

Status: **contracts + IRC session engine**. `unibus-chat` defines the documents;
`unibus-irc` owns a real IRC connection and projection. The daemon loop that
publishes to Unibus and answers `chat.fetch.v1` is the next slice.

## Scope

Read-only inbound chat ingest, IRC first:

- IRC registration, optional server password, channel JOIN, PING/PONG.
- IRCv3 `message-tags` and `server-time` negotiation, best effort.
- `PRIVMSG`, `NOTICE`, CTCP `ACTION`, JOIN/PART.
- No outbound `chat.send` yet. Sending is a write action with approval, audit,
  and echo suppression; it should not be smuggled into the read path.

Beeper/Matrix reuse the same contracts later. They are separate transports, not
different semantics.

## Documents

| Kind | Direction | Contains |
| --- | --- | --- |
| `chat.account.v1` | adapter → bus | Transport, network, host/port/TLS, nick, `geode://` credential ref, observed channels, reconnect cadence, capabilities. |
| `chat.message.v1` | adapter → bus | Channel, stable message id, identity (`server`/`local`), nick, sent time, kind, `body_bytes`, optional `reply_to`, flags. No text. |
| `chat.delta.v1` | adapter → bus | Generation, cursor, bounded upserts/expunges, lease. |
| `chat.fetch.v1` | caller → adapter | Request for retained text, bounded `max_bytes`. Reply goes to that caller only. |
| `chat.fetch.reply.v1` | adapter → caller | Metadata plus the text in the binary payload; never broadcast. |

## Identity

IRC has no message identity in the base protocol.

- `msgid` tag present → `identity = server`, `message_id = msgid`. Safe to
  deduplicate.
- No `msgid` → `identity = local`, `message_id = local:<generation>:<sequence>`.
  A reconnect starts a new generation. Consumers must not treat a local id as
  stable across generations.

This mirrors POP3 UIDL behavior in mail. Do not synthesize a hash of sender + text
and call it stable; repeats are meaningful and collisions are silent.

## Text retention and fetch

The bus never carries message text. IRC does not offer a reliable history API, so
the adapter retains a bounded in-memory ring:

- default 1024 messages / 8 MiB total;
- per-message fetch truncated to the request's `max_bytes` on a UTF-8 boundary;
- evicted messages return "not retained", never an empty body pretending to be
  content;
- the ring is not persisted. A restart drops history.

`chathistory` (IRCv3) or a Matrix/Beeper backend can replace the ring later.
Until then, be honest that backfill is limited.

## Credentials

`chat.account.v1.credential_ref` is `geode://chat/<network>/<account>` or empty
for an open local server. The host resolves it to a server password / SASL
token; the adapter never logs it and never stores it in the session struct.

Do not put a bridge appservice token or Matrix access token in the IRC path. The
IRC adapter speaks IRC; Beeper/Matrix gets its own adapter.

## Grants: read and send are separate

Keep capabilities separate:

- `chat.metadata` — receive account/message/delta events.
- `chat.fetch` — request retained text for a known message.
- `chat.send` — publish `chat.send.v1` and make the adapter send upstream.

**Read never implies send.** A producer granted `chat.metadata` and
`chat.fetch` cannot send. `chat.send.v1` is a distinct message kind, so the
hub's `publishes` grant can authorize it independently.

Sending is off by default and needs all of:

1. `send.enabled = true` in the adapter config;
2. the producer id in `send.allowed_producers`;
3. `chat.send` in the adapter's bus capabilities;
4. a hub grant that lets that caller publish `chat.send.v1` to this adapter.

The adapter advertises `capabilities.send = false` unless enabled. Rejected
sends return `chat.send.reply.v1` with a closed `SendRejection` reason. Text
travels in the binary payload, is bounded, and is never logged.

## Outbound send

`chat.send.v1` carries `{request_id, account_id, channel, text_bytes}`; the
UTF-8 text is the binary payload. The adapter checks the producer allowlist,
channel allowlist, size bound, and transport result before replying. The echoed
message comes back through the normal read path, so it is retained and
fetchable like any other message.

This is still not an approval workflow. If a deployment needs two-person
approval or a time-bound escalation, put it in front of `chat.send.v1`; the
adapter only enforces the explicit grant.

## Configuration shape

```json
{
  "schema_version": 1,
  "account_id": "irc-fleet",
  "transport": "irc",
  "network": "fleet",
  "host": "irc.example.net",
  "port": 6697,
  "tls": true,
  "nick": "mycelium",
  "credential_ref": "geode://chat/fleet/irc",
  "channels": ["#agents", "#ops"],
  "reconnect_interval_ms": 15000,
  "capabilities": {
    "push": true,
    "history": false,
    "send": true,
    "server_ids": true
  }
}
```

`channels` is an allowlist. A message for an unlisted channel is ignored, even if
the server delivers it. Never enumerate or join server-wide.

## Running the bridge

`unibus-irc` is the publisher loop:

```sh
unibus-irc config/irc-bridge.example.json
```

It registers as `producer_id`, advertises `chat.bridge`, publishes
`chat.account.v1` once, then one delta per observed message. It polls the bus
every 100 ms for `chat.fetch.v1` and replies to the requester only.

Credentials: `env://NAME`, or `geode://realm/account/slot` leased from a
running `geoded` socket. Set `GEODE_ENV_FALLBACK=1` for geode's env mapping in
tests.

## Next slices

1. IRCv3 SASL PLAIN/EXTERNAL for networks that require it.
2. Outbound `chat.send.v1` with explicit write grant and echo suppression.
3. Host geode resolver for the binary (no `env://` requirement).
4. Beeper/Matrix adapter behind the same `chat.*` contracts.
