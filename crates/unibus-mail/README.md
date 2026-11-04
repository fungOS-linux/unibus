# unibus-mail

Bounded POP3/IMAP metadata contracts for the Unibus mail bridge.

This crate contains no network code, no credentials, and no message bodies. It
defines what may cross the bus; the adapter owns the mailbox session.

- `Account` — protocol, non-secret login identity, `geode://` credential
  reference, allowed folders, poll cadence, negotiated capabilities.
- `Message` — metadata notification. Stable identity is
  `uidvalidity + uid` (IMAP) or `uidl`/local sequence (POP3).
- `Delta` — one bounded change window with a generation and cursor.
- `FetchRequest` — an authorized request for headers/text/html/attachments.
  The adapter replies only to the caller; bodies are never broadcast.

POP3 is represented as a degraded protocol: inbox only, poll only, no folders,
no flags, no push. Consumers must read `Account::capabilities` rather than
assume IMAP semantics.

Credentials are referenced as `geode://...`, never as a password or token.
`geode` is the host-owned credential broker; the adapter resolves the reference
locally at connect time and must not log or serialize the result.
