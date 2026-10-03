# Unibus — the contract

Normative text for L1. The wider system design (presence-driven display
policy, content plane, caches, Canvas integration) lives in
`FuturePresentLabs/dock/docs/unibus.md`; this file is only what the bus
itself guarantees. Lifted from, and wire-compatible with, JARVIS master
`jarvis-core/src/edge_transport.rs` (router protocol v1).

## Bus, not network

| Layer | Owner | Provides |
|---|---|---|
| L0 reachability | provider (`PeerDirectory` + `PeerTransport`) | peer candidates and authenticated paths across machines/OSes |
| L1 transport | `unibus` (this crate) | framing, envelopes, grants, registry semantics, QoS mapping |
| L2 contracts | domain crates | typed versioned message bodies, validation, reconcile |

No route computation, no gossip, no multi-hop, no store-and-forward
intelligence in Unibus core. Direct peer delivery is preferred when a provider
can resolve and connect both peers; a hub remains an optional gateway, relay,
and compatibility path. Static and DNS providers keep Unibus independently
operable. The optional `unibus-mycelium` adapter consumes Mycelium's public
local RPC interface; neither core project imports the other.
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
- Schema-v2 grants also separate ingress authority from receiver capability:
  exact `publishes` and allowed zone, screen-anchor, and producer destinations.
  A receiver capability never implicitly grants permission to publish.
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

The TCP hub implements these semantics with a bounded per-peer outbound
scheduler: commands occupy a reliable FIFO and fail loudly at capacity,
streams evict replaceable work rather than building latency, and snapshots
replace an older queued value with the same producer/kind/destination key.
Non-command frames are discarded if their TTL expires before transmission.
QoS mappings are exact, contract-owned names; unknown kinds fail safe to the
reliable command class rather than inheriting weaker semantics from a prefix.

QUIC is the preferred transport (`quinn`): built-in TLS 1.3, per-stream
ordering with no cross-stream head-of-line blocking, unreliable datagrams
(RFC 9221), connection migration. The TCP fallback reuses the framing above
unchanged; envelope semantics are identical on either.

The implemented QUIC adapter uses the JSON-plus-binary frame body directly:
small eligible stream/snapshot messages use negotiated datagrams, while
commands and datagram-ineligible frames use reliable one-way streams. Both
enter the same authenticated hub session and routing code as TCP.

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
- Shared bearer tokens are consumed only by registration and are cleared at
  the authenticated session boundary; relayed envelopes never contain them.

The router accepts an injected `OfflineVerifier` and binds its verified claims
to the configured subject, producer, audience, installation, assertion lifetime,
and granted capability/zone/anchor scope. Signature/JWKS ownership remains in
the fpl-auth integration rather than being duplicated in this crate.

## Invariants

- Grants and signed direct-session assertions remain authoritative regardless
  of which discovery or transport provider supplies the path.
- A hub is optional. Its absence must not prevent an authorized direct session
  when a peer directory and transport can connect the participants.
- Hub listens on tailnet (+ loopback for the brain itself); TCP fallback is
  loopback-only behind SSH tunnels. Plaintext wildcard TCP requires the
  explicit legacy compatibility switch `allow_insecure_lan`.
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
