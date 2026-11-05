# Gmail support

Gmail is not a separate mail protocol. It is one of two adapters behind the same
`email.*` contracts:

| Path | Identity | Least privilege | Effort | Status |
| --- | --- | --- | --- | --- |
| **IMAP + XOAUTH2** | `UIDVALIDITY` + `UID` | Poor: requires `https://mail.google.com/` | Small | auth seam implemented |
| **Gmail API** | Gmail `message.id` + `historyId` | Good: `gmail.metadata` + `gmail.readonly` | Medium | next adapter |

## Why not app passwords

An app password works only on accounts where Google still allows it, is a
long-lived bearer secret, and bypasses the OAuth lifecycle the rest of the fleet
uses. Do not build the bridge on it. `unibus-mail-imap` accepts `LOGIN` for local
test servers and `XOAuth2` for real providers.

## Path 1: IMAP + XOAUTH2 (smallest change)

Settings:

```json
{
  "protocol": "imap",
  "host": "imap.gmail.com",
  "port": 993,
  "tls": true,
  "username": "you@gmail.com",
  "credential_ref": "geode://oauth/google.you/access"
}
```

The adapter already performs `AUTHENTICATE XOAUTH2` with
`user=<login>\x01auth=Bearer <token>\x01\x01`.

### The catch

Google requires the **`https://mail.google.com/`** scope for IMAP and SMTP. That
is full mailbox access, including send and delete. It is broader than a
read-only bridge needs, and it is a restricted scope: a public multi-user app
needs Google verification and a security assessment. For a single owner's
account, it is a deliberate trade.

Token lifecycle:

1. Create a Google Cloud project and an OAuth **Desktop app** client.
2. Authorize once on a trusted machine with a loopback redirect.
3. Put the refresh token in geode:
   `geode://oauth/google.<account>/refresh` with purpose `oauth2.refresh`.
   The reference has exactly three components; use a dotted account
   (`google.avery`), not a fourth path segment.
4. The host resolver leases an access token from `geode-oauth` (refreshing when
   needed) and returns `MailAuth::XOAuth2(token)` to `unibus-mail-imap`.
5. The adapter never sees the refresh token and never caches the access token.

### Folders

Gmail exposes labels as IMAP folders. A message can appear in several folders
with different UIDs, so bridging both `INBOX` and `[Gmail]/All Mail` duplicates
it. Pick one stream:

- `INBOX` for new mail only;
- `[Gmail]/All Mail` for everything once, if you accept the larger sync.

Never enumerate every label by default. `UIDVALIDITY` can change; the adapter
reports it per mailbox and consumers must reset on a generation change.

### Push

Gmail supports IMAP `IDLE`. The first engine uses bounded polling; `IDLE` is the
next improvement. Do not claim push until the adapter actually sends `IDLE`.

## Path 2: Gmail API (least privilege, recommended long term)

Use the REST API instead of IMAP:

- `users.getProfile` → current `historyId`.
- `users.history.list(startHistoryId=...)` → `messagesAdded`,
  `messagesDeleted`, label changes. This is a real cursor, unlike polling.
- `users.messages.get(format=metadata, metadataHeaders=From,Subject)` → headers,
  `internalDate`, `sizeEstimate`, label ids. No body.
- `users.messages.get(format=raw)` only for an authorized `email.fetch.v1`,
  bounded and truncated.
- `users.watch` + Pub/Sub for push, when a public receiver is acceptable.

Scopes:

- `gmail.metadata` — headers and labels, no body. Enough for `email.message`.
- `gmail.readonly` — add body fetch. Restricted; verification required for a
  public app.
- Do **not** request `gmail.modify` or `https://mail.google.com/` for a read-only
  bridge.

### Contract implication

The mail contract uses `uidvalidity: u64` + `uid: u64`. Gmail message IDs are
opaque strings. The API adapter needs one of:

1. A bounded `native_id: Option<String>` plus a stable `generation` (preferred,
   additive), or
2. A separate `email.gmail` document family.

Do not hash a Gmail message id into a `u64` and pretend it is collision-free.
Decide the identity field before writing the API adapter.

### OAuth client

- Workspace: an Internal OAuth app avoids public verification.
- Personal Gmail: External app in testing mode with the owner as a test user, or
  accept the restricted-scope review later.
- Headless hosts: authorize on a laptop, store only the refresh token in geode;
  the host never needs a browser.

## Google Workspace

Yes, Workspace accounts work through the same IMAP + OAuth2 path, with two
admin switches:

1. **Enable IMAP** in the Admin console: Apps → Google Workspace → Gmail →
   End User Access → IMAP access. If the domain disables IMAP, XOAUTH2 login
   fails even with a valid token.
2. **Allow the OAuth client**: an **Internal** app is limited to the Workspace
   org and skips public verification. The scope is still
   `https://mail.google.com/` for IMAP.

Usernames are the full Workspace address (`avery@example.com`); the IMAP host is
still `imap.gmail.com`. Delegated/shared mailboxes are not supported in v1: run
one bridge config and one producer per mailbox. If Workspace policy forbids
IMAP or full-scope OAuth, use the Gmail API adapter instead.

The example config is `config/mail-gmail-imap.example.json`. With `geoded`
running, store the refresh token and client secret at
`geode://oauth/google.avery/refresh` and
`geode://oauth/google.avery/client-secret`. For a quick test without geoded,
set `GEODE_ENV_FALLBACK=1` and use
`GEODE_OAUTH_GOOGLE_AVERY_REFRESH` / `..._CLIENT_SECRET`.

## Recommendation

- Need Gmail **today**, single owner, accept full-scope IMAP: use Path 1. It is
  already implemented at the engine level.
- Building for other users or wanting read-only scope: do Path 2, and add the
  string identity field to `email.message.v1` first.

Neither path stores a Google password, an app password, or a refresh token in
Unibus, configuration, or logs. The bus sees `email.*` metadata only.
