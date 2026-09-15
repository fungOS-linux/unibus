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

| Path | What |
|---|---|
| `src/envelope.rs` | The frozen envelope shape, TTL/installation validation |
| `src/framing.rs` | `[u32 BE len][JSON][binary payload]` codec |
| `src/qos.rs` | command / stream / snapshot classification |
| `src/grants.rs` | Node grants: zones, namespaced capabilities, anchors, site, role |
| `src/auth.rs` | fpl-auth-aligned JWT claim shape + offline freshness rules |
| `tests/wire_compat.rs` | Golden JARVIS v1 frames — the cross-repo contract |
| `DESIGN.md` | The normative contract |

Related: `FuturePresentLabs/dock` — `docs/unibus.md` (Canvas's use of the
bus); JARVIS — `jarvis-router`, `jarvis-edge`, `distributed-topology.md`.
