# AgentMail over IMAP

AgentMail supports standard IMAP, so it uses `unibus-mail-imap` with **no OAuth
and no new adapter**. The API key is the IMAP password.

| Setting | Value |
| --- | --- |
| Host | `imap.agentmail.to` |
| Port | `993`, TLS required |
| Username | the inbox address (`myinbox@agentmail.to`) |
| Password | an AgentMail API key |
| Folders | `INBOX`, `Sent`, `Trash`, `Spam` (Drafts is listed but empty) |
| Push | IMAP `IDLE` is supported by AgentMail; the current adapter polls |

## API key permissions

An unrestricted API key works as-is. A **restricted** key must include all of:

- `inbox_read` (LOGIN)
- `message_read` (message + event log sync)
- `message_update` (STORE/MOVE/COPY)
- `message_send` (SMTP; not used by this read-only bridge)
- `label_spam_read`, `label_trash_read`
- `label_blocked_read`, `label_unauthenticated_read`

Symptom of a missing permission: LOGIN succeeds but new mail never appears,
because IMAP cannot read the inbox event log. Start with an unrestricted key for
the first bring-up, then narrow it.

## Bring-up

```sh
export AGENTMAIL_API_KEY=...
# or, with geoded, store it at geode://agentmail/inbox/api-key

unibus-mail-imap config/mail-agentmail.example.json
```

The example uses `geode://agentmail/inbox/api-key`; with the current
`unibus-credentials` env mapping that reads
`GEODE_AGENTMAIL_INBOX_API_KEY`. Replace with `env://AGENTMAIL_API_KEY` for a
quick local test, or point geoded at the same reference once it is running.

## Scope

Read-only ingest. SMTP sending is out of scope, as with every other mail
account. AgentMail's webhooks/WebSocket API is a future lower-latency
alternative to IMAP polling, but IMAP keeps one code path for all providers.
