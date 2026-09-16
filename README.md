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
| `src/registry.rs` | Live node registry: heartbeats (3 s / 10 s TTL), deterministic selection |
| `src/config.rs` | A v1 `router.json` loads unchanged; v2 adds routes table + site/role |
| `src/plugin.rs` | `HubPlugin`/`Sink` seam — stateful domain behavior (like presence fan-out) lives with its domain |
| `src/auth.rs` | v1 shared-token mode until the fpl-auth `OfflineVerifier` lands; credentials terminate at registration |
| `tests/hub.rs` | End-to-end over loopback: routing, echoes, anchors, plugin gating |

Routing is configuration: the hub maps message kinds to capabilities via a
`routes` table; unknown kinds without a plugin fail loudly. The hub parses
only `register` — every data body stays opaque.

Schema-v2 node grants must explicitly list `publishes` plus their permitted
`publish_zones`, `publish_screen_anchors`, and `publish_producers`. Frozen v1
peers with no publish policy retain compatibility. Plaintext TCP binds are
loopback-only unless `allow_insecure_lan: true` is explicitly set; use a secure
tunnel for remote TCP deployments.

Related: `FuturePresentLabs/dock` — `docs/unibus.md` (Canvas's use of the
bus); JARVIS — `jarvis-router`, `jarvis-edge`, `distributed-topology.md`.
