# Mail bridge runbook

Status: **contracts + IMAP owner engine**. `unibus-mail` defines the documents;
`unibus-mail-imap` owns a real IMAP session and the credential seam. The daemon
loop that publishes to Unibus and serves fetch requests is the next slice.

Gmail is the same engine with XOAUTH2; see [`mail-gmail.md`](mail-gmail.md) for
the scope trade-off between IMAP and the Gmail API.

## Running the bridge

`unibus-mail-imap` is the publisher loop:

```sh
unibus-mail-imap config/mail-imap-bridge.example.json
```

It registers with `mail.bridge`, publishes `email.account.v1` once, polls each
configured folder on the account cadence, and answers `email.fetch.v1` with a
bounded raw message (`message/rfc822`).

Current binary limitation: `env://NAME` credentials only. `geode://` references
need a host process with the geode client; `ImapSession::connect` already takes
any `CredentialResolver`, including one that returns `MailAuth::XOAuth2` for
Gmail.

## Scope

The bridge is a read-only mail ingest path into Unibus:

- IMAP: folders, flags, UIDVALIDITY/UID identity, `IDLE` observation.
- POP3: inbox only, poll only, no flags, no folders, no push. It is a degraded
  protocol and says so in `Account::capabilities`.
- SMTP/outbound is out of scope. Sending mail is a write action with approval,
  audit, and bounce semantics; it should not be smuggled into the read path.

The bridge owns the mailbox session. Unibus L1 routes opaque envelopes and
never reads a message body.

## Documents

| Kind | Direction | Contains |
| --- | --- | --- |
| `email.account.v1` | adapter → bus | Protocol, host/port/TLS, non-secret login, `geode://` credential reference, allowed folders, poll cadence, negotiated capabilities. |
| `email.message.v1` | adapter → bus | Stable identity (`uidvalidity` + `uid` or `uidl`), size, received time, flags, optional metadata. No body. |
| `email.delta.v1` | adapter → bus | One bounded window: generation, cursor, upserts, expunges, lease. |
| `email.fetch.v1` | authorized caller → adapter | Request for headers/text/html/attachments. The reply goes to that caller only. |

`Message` uses IMAP `UIDVALIDITY` + `UID` as identity, not `Message-ID`, which a
sender can forge or duplicate. POP3 uses UIDL when available and always keeps a
stable local sequence.

## Credentials: `geode`

The account config contains only a reference:

```json
"credential_ref": "geode://mail/work/imap"
```

`geode` is the host-owned credential broker. The mail owner resolves the
reference locally at connect time, keeps the secret in memory only, and must
not log, serialize, or publish it. The bus never carries a password, OAuth
token, or credential-bearing URL.

Recommended first backend: the OS keyring on macOS (Keychain) and Linux Secret
Service, with `systemd-creds`/TPM for headless hosts. Do not build a second
plaintext vault. If a replicated vault arrives later, the field-by-field
metadata decision in `dock-ybc.12.2` comes first: subject, sender, folder,
flags, timestamps, attachment names, item count/size.

## Metadata boundary

Publish metadata only when the account is explicitly configured to do so.
Default is identity, size, time, and flags. `Metadata` is optional and may carry
only `from_domain` and `subject`; it must not be used as a search index.

Never publish:

- message bodies, HTML, or attachment bytes;
- full headers (`Received` chains expose infrastructure and addresses);
- credentials, tokens, or account passwords;
- an unbounded mailbox dump.

`email.fetch.v1` requires `mail.fetch` and a bounded `max_bytes`. The adapter
must not use fetch results as bus publications.

## Authorization and lifecycle

Required capabilities, kept separate:

- `mail.metadata` — receive account/message/delta events.
- `mail.fetch` — request bounded content for a known account/message.
- future `mail.send` — never part of the read-only bridge.

The adapter's grants must list the exact publish kinds and producers. Discovery
of a mail service is not authorization to read it.

On startup the adapter validates its config, resolves no credentials yet, and
connects lazily. Reconnect re-resolves the credential through `geode`; it does
not cache a password in a long-lived struct. `UIDVALIDITY` changes reset the
mailbox generation and force consumers to discard stale cursors.

## Next slices

1. IMAP owner adapter: TLS, one account, one or more configured folders, UID
   cursor, `IDLE`, metadata-only publications.
2. Fake transport tests plus a Dovecot/GreenMail integration test.
3. `geode` macOS Keychain backend and a `geode` client used by the adapter.
4. POP3 adapter after the contract has proven itself.
