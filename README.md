# Unibus

One household bus for every surface. Display, audio, presence, sensor, and
control modules are all just grant-bearing slots on the same backplane —
heterogeneous machines (mac-studio, macbook-neo, home-pi + TV, Agora),
one narrow protocol. The name is the DEC UNIBUS bet, restated.

This crate is **L1**: envelopes, framing, grants, QoS classification. It is
deliberately dumb — it routes by destination, screen anchor, zone +
capability, and TTL; it never reads a message body. Domain meaning lives in
L2 contract crates (`canvas.*`, `audio.*`, `presence.*`, ...) that each
domain owns.

## Status

Contract seed, lifted from JARVIS master (`jarvis-core::edge_transport`) and
kept wire-compatible with router v1. The JARVIS-side extraction and the
Canvas-side edge connector are tracked downstream (`dock-2ex7` epic).

## Build

Use current stable Rust and Git. No sibling checkout is required: external
Rust integrations use revision-pinned HTTPS Git dependencies. On Linux, full
workspace builds require `pkg-config` and `libasound2-dev` for MIDI/audio.
The private Holodeck integration is an opt-in independent workspace; see
[its runbook](docs/holodeck-bridge.md) for manifest-path builds and repository access.

For only the portable ADB bridge (no audio development libraries required):

```sh
cargo check --locked -p unibus-adb
cargo test --locked -p unibus-adb
cargo install --locked --path crates/unibus-adb
```

See the [portable ADB quickstart](docs/adb-portable-quickstart.md) for two-machine
setup, credential handoff, USB permissions, and static SSH without Mycelium.

```sh
cargo test          # unit + wire-compat golden vectors
cargo clippy --all-targets -- -D warnings
cargo fmt --all
```

## Layout

`crates/unibus` — the L1 contract:

| Path | What |
|---|---|
| `src/envelope.rs` | The frozen envelope shape, TTL/installation validation |
| `src/framing.rs` | `[u32 BE len][JSON][binary payload]` codec |
| `src/kinds.rs` | Control-plane kinds (`register`/`heartbeat`/`reply`, v1+v2) |
| `src/qos.rs` | command / stream / snapshot classification |
| `src/grants.rs` | Node grants: zones, namespaced capabilities, anchors, site, role |
| `src/auth.rs` | fpl-auth-aligned JWT claim shape + offline freshness rules |
| `tests/wire_compat.rs` | Golden JARVIS v1 frames — the cross-repo contract |

`crates/unibus-router` — the hub daemon, lifted from `jarvis-router`:

| Path | What |
|---|---|
| `src/hub.rs` | Accept loop, registration/auth, ack policy, anchor/zone/capability relay |
| `src/outbound.rs` | Bounded per-peer command/stream/snapshot scheduler and writer |
| `src/quic.rs` | Quinn/Rustls endpoint, explicit trust configuration, datagram/stream mapping |
| `src/registry.rs` | Live node registry: heartbeats (3 s / 10 s TTL), deterministic selection |
| `src/config.rs` | A v1 `router.json` loads unchanged; v2 adds routes table + site/role |
| `src/plugin.rs` | `HubPlugin`/`Sink` seam — stateful domain behavior (like presence fan-out) lives with its domain |
| `src/auth.rs` | shared-token compatibility plus verifier-backed fpl-auth sessions; credentials terminate at registration |
| `tests/hub.rs` | End-to-end over loopback: routing, echoes, anchors, plugin gating |

`crates/unibus-mqtt` — a bidirectional MQTT 3.1.1 edge adapter. Local MQTT
clients publish and subscribe through their local broker; adapters carry exact
topics and binary payloads over the reliable `mqtt.publish.v1` Unibus contract.
Broker sessions, retained state, and MQTT acknowledgements remain broker-owned.
See [`docs/mqtt-bridge-runbook.md`](docs/mqtt-bridge-runbook.md).

`crates/unibus-mail` — bounded POP3/IMAP metadata contracts. The mail owner
keeps the mailbox session and publishes only `email.account.v1`,
`email.message.v1`, and `email.delta.v1`; content is fetched through an
authorized `email.fetch.v1` request and is never broadcast. Credentials are
named as `geode://...` references and resolved only by the local owner. See
[`docs/mail-bridge-runbook.md`](docs/mail-bridge-runbook.md).

`crates/unibus-mail-imap` — the IMAP owner adapter. It owns the TLS session,
fetches metadata-only windows with `UIDVALIDITY`/`UID` identity, answers bounded
`email.fetch.v1` requests with `BODY.PEEK[]`, and supports `LOGIN` plus XOAUTH2
(needed for Gmail and other OAuth2 providers). See
[`docs/mail-gmail.md`](docs/mail-gmail.md).

`crates/unibus-chat` and `crates/unibus-irc` — chat contracts and the first
transport. The IRC adapter negotiates `message-tags`/`server-time`, uses IRCv3
`msgid` when present and an explicitly local identity otherwise, and keeps a
bounded in-memory text ring so `chat.fetch.v1` never broadcasts message bodies.
See [`docs/chat-bridge-runbook.md`](docs/chat-bridge-runbook.md).

Routing is configuration: the hub maps message kinds to capabilities via a
`routes` table; unknown kinds without a plugin fail loudly. The hub parses
only `register` — every data body stays opaque.

Schema-v2 node grants must explicitly list `publishes` plus their permitted
`publish_zones`, `publish_screen_anchors`, and `publish_producers`. Frozen v1
peers with no publish policy retain compatibility. Plaintext TCP binds are
loopback-only unless `allow_insecure_lan: true` is explicitly set; use a secure
tunnel for remote TCP deployments.

Nodes with `auth_subject` are authenticated through the `OfflineVerifier`
installed with `Hub::with_offline_verifier`; subject, audience, installation,
producer, lifetime, and asserted grant scope are all bound before registration.
Nodes without `auth_subject` remain in explicit shared-token compatibility mode.

The library hub can additionally listen over QUIC/TLS with `Hub::start_quic`.
The standalone `unibus-hub` starts both TCP and QUIC when configuration includes:

```json
"quic": {
  "listen": "0.0.0.0:18791",
  "cert_file": "tls/server.pem",
  "key_file": "tls/server-key.pem"
}
```

Certificate paths resolve relative to `router.json`; invalid or missing PEM
material stops startup. See [`docs/quic-runbook.md`](docs/quic-runbook.md).

Related: `FuturePresentLabs/dock` — `docs/unibus.md` (Canvas's use of the
bus); JARVIS — `jarvis-router`, `jarvis-edge`, `distributed-topology.md`.
