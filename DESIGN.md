# Unibus — the contract

Normative text for L1. The wider system design (presence-driven display
policy, content plane, caches, Canvas integration) lives in
`FuturePresentLabs/dock/docs/unibus.md`; this file is only what the bus
itself guarantees. Lifted from, and wire-compatible with, JARVIS master
`jarvis-core/src/edge_transport.rs` (router protocol v1).

## Bus, not network

| Layer | Owner | Provides |
|---|---|---|
| L0 reachability | Tailscale (externally owned) | tailnet identity, IP across machines/OSes |
| L1 transport | `unibus` (this crate) | framing, envelopes, grants, registry semantics, QoS mapping |
| L2 contracts | domain crates | typed versioned message bodies, validation, reconcile |

No route computation, no gossip, no multi-hop, no store-and-forward
intelligence. Hub-and-spoke per installation; "mesh" means the tailnet.
**L1 stays dumb:** anything that parses a message body is a contract. A new
capability or domain must never require an L1 release.

## Wire

`[u32 BE total length][JSON envelope][optional binary payload]`, frames
≤ 16 MiB. Envelope fields (v1, frozen):

```json
{ "schema_version": 1, "installation_id": "home", "producer_id": "home-pi-tv",
  "destination": null, "target_screen": null, "request_id": "r-1",
  "issued_at_unix_ms": 0, "ttl_ms": 30000, "payload_bytes": 0, "token": "...",
  "message": { "message_type": "register", "...": "opaque body" } }
```

- `message_type` routes; the rest of the message object is opaque to L1.
  v2 introduces namespaced kinds (`canvas.command`, `audio.playback_chunk`,
  `presence.snapshot`, `sensor.*`); unknown kinds route by `destination`
  without the relay understanding them. v1 snake_case tags remain valid.
- `payload_bytes` must equal the binary payload length exactly, both
  directions; mismatch fails the frame.
- Validation on every inbound envelope: supported `schema_version`,
  `installation_id` match, non-empty ids, TTL freshness with ≤ 5 s future
  skew. Fail loud — no silent drop on a routing miss; producers get a
  `reply{success:false}` or a logged loud no-target.

## Registration, grants, registry

- The first frame on a connection must be `register` with `capabilities`,
  `zones`, `screen_anchors`.
- A hub **grant** (static config or provisioned record) pins:
  `producer_id`, `auth_subject` (fpl-auth OIDC subject), permitted zones,
  capability whitelist (namespaced strings, `^[a-z0-9][a-z0-9_-]*(\.[a-z0-9][a-z0-9_-]*)+$`),
  screen anchors, `site` (`home-lan`, `wan`, ...), `role`
  (`origin` | `cache` | `edge`). Claims outside the grant are rejected.
  Unknown grant fields are rejected — a typo must not widen scope.
- Heartbeats keep registrations alive (send 3 s, TTL 10 s); dead nodes are
  pruned and must re-register (re-authenticated). A dead node cannot
  heartbeat back to life.
- Selection: zone + capability, deterministic tie-break (longest-connected,
  then `producer_id`); or direct addressing by `target_screen` anchor.

## QoS classes

L1 maps every message kind to exactly one class (v1 tags and v2 names agree):

| Class | Examples | Delivery |
|---|---|---|
| command | `canvas.command`, control, desired-state | reliable ordered stream; TTL; idempotent via revision; loud failure |
| stream | audio PCM, sensor samples | latency over completeness; datagram when it fits, else one-way stream the reader may reset; stale bytes dropped, never queued |
| snapshot | presence, index/state facts | latest-value replaceable; fire-and-forget; stale → *unknown*, never *empty* |

QUIC is the preferred transport (`quinn`): built-in TLS 1.3, per-stream
ordering with no cross-stream head-of-line blocking, unreliable datagrams
(RFC 9221), connection migration. The TCP fallback reuses the framing above
unchanged; envelope semantics are identical on either.

## Auth — aligned with fpl-auth

Identity root is **fpl-auth** (`sso.fpl.dev`), not an invented PKI:

- Nodes enroll as per-user devices, following the existing *desktop bridge*
  pattern (enroll → device token → heartbeat → revoke in the user's device
  list).
- A node presents short-lived RS256 JWTs (fpl-auth-issued, `kid`-resolved)
  whose claims include the grant scope (`unibus_zones`,
  `unibus_capabilities`, `unibus_screen_anchors`, installation, producer).
  `exp − iat ≤ 300 s`; expiry is the revocation window — no CRL machinery.
- Validation is **offline**: any holder of the pinned/cached JWKS validates,
  including a peer in a direct media session with no hub round-trip. The
  `unibus` crate defines the claim shape and freshness rules
  (`NodeClaims::validate`); signature verification is the integrator's
  `OfflineVerifier` implementation against fpl-auth's published keys.
  Accepting `alg: none` or unknown critical extensions is a conformance
  failure.
- TLS proves the channel; the assertion proves the application identity —
  both transports (QUIC, TCP-over-SSH) enforce the same plane.

## Invariants

- One authoritative hub per installation; no elected failover in v1.
- Hub listens on tailnet (+ loopback for the brain itself); TCP fallback is
  loopback-only behind SSH tunnels.
- A disconnected node is simply absent from selection; the installation
  keeps operating. Degrade loudly, never silently to the wrong screen.
- Adapters expose unconfigured/unsupported backends as *unavailable* — never
  as success from a local placeholder.

## Compatibility policy

`tests/wire_compat.rs` holds golden frames as JARVIS master emits them.
Breaking the wire requires a new `schema_version`, and both versions must
coexist for one release cycle. Capability names and message kinds are open
sets — adding one is a data change, never a protocol bump.

## Out of scope

Content bytes (refs on the bus, pulls in the cache), pixel streaming
(Moonlight/Sunshine owns that), multi-site replication, sensor fusion,
display policy — all L2/L3 concerns.
