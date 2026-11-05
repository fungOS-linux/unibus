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

## Grants

Keep capabilities separate:

- `chat.metadata` — receive account/message/delta events.
- `chat.fetch` — request retained text for a known message.
- future `chat.send` — never part of the read-only adapter.

Discovery of a chat service is not authorization to read it.

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

## Next slices

1. Publisher loop: connect, publish `chat.account.v1`, emit deltas, answer
   `chat.fetch.v1` from the ring via the Unibus client.
2. IRCv3 SASL PLAIN/EXTERNAL for networks that require it.
3. Outbound `chat.send.v1` with explicit write grant and echo suppression.
4. Beeper/Matrix adapter behind the same `chat.*` contracts.
