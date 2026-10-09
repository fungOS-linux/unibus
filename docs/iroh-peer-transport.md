# Optional Iroh peer transport

This is a library integration, not an enabled fleet transport. Build
`unibus-iroh` with `transport`; its default feature set adds no Iroh runtime.
It carries bounded reliable Unibus envelopes and binary payloads using ALPN
`unibus/peer/1`, with one FIN-delimited QUIC stream per message. This slice does
not add unreliable media datagrams, A/V clock synchronization, or router wiring.

## Fleet identity without inner TLS

Enable `fleet-identity` to reuse the pinned `mycelium-peer-protocol` verifier.
Iroh provides native encryption and endpoint-key possession; no TLS session is
nested inside the stream. `FleetProof` carries an authority-signed membership
and a machine-signed endpoint credential. `FleetIdentity` verifies both against
the actual connected Iroh key, not the advertised key alone.

Configure `SessionConfig.endpoint_identity = Some(Arc::new(FleetIdentity::new(
local_proof, trust_source, producer_bindings)))` on both peers. Producer bindings
pin both the canonical machine and endpoint credential slot. Missing or invalid
proofs deny admission when configured; there is no retry that disables identity
verification. Proofs are exchanged in both the authentication and ready frames,
bounded to 64 KiB. Remote identity is rechecked for each message, including after
an asynchronous ACL query, so expiry, refreshed revocations and stale trust state
also stop existing sessions from exchanging further messages.

`FleetTrustSource` is a nonblocking local-state interface. Its implementation
must durably retain verified revocations and generation floors, reject unknown
slots, and supply a bounded-freshness snapshot from trusted enrollment state.
The adapter rejects stale/future snapshots and missing positive generation
floors. It does not fetch authority keys from callers or create a new identity
database. Production issuance, renewal and durable trust storage are embedding
responsibilities, not implemented by this library slice.

Identity does not replace the signed scoped session or `PeerAccessPolicy`.
The existing session-only mode remains available when no endpoint identity
verifier is configured. Mycelium is optional; this feature links its small
portable protocol library, not its daemon. Run
`cargo test -p unibus-iroh --features fleet-identity` to exercise real signed
proofs over actual encrypted loopback, including live revocation denial.

## Ownership and authorization

- Mycelium discovers application-owned endpoints and can manage grants.
- Unibus verifies session assertions and enforces identity, kind, destination,
  payload bounds, freshness, and expiry locally on each frame.
- Iroh supplies encrypted reachability, not application authorization or OIDC.
- An optional `PeerAccessPolicy` can further deny access. It cannot replace
  signed-session verification or endpoint-key binding.

An application endpoint is distinct from Mycelium's `mycelium/sync/1` endpoint.
Do not reuse the daemon's private key. Deployment owns a persistent endpoint
key, endpoint lifecycle, listening addresses, connection/stream limits, and
explicit relay configuration. Start with Iroh's `Minimal` preset, disable
public address publication and port mapping, and explicitly enable only desired
relays. `SessionConfig.allowed_relays` separately restricts discovered relay
origins; an advertisement cannot grant relay trust.

The injected `DirectSessionVerifier` must verify a trusted issuer's signature.
No production issuer or permissive JSON verifier is included. Signed claims
bind producer IDs to `iroh:<endpoint-public-key>` and expire within 300 seconds.
The JSON verifier in the loopback test is a fixture only.

## Discovery

`IrohDirectory` consumes the existing provider-neutral `ServiceDirectory`.
Trusted deployment bindings map a Unibus producer to an exact service and its
canonical owner. Observations must be fresh, owned by that node, and advertise
`unibus/peer/1`; Mycelium gossip-only endpoints are rejected. Expected attributes
are `node_id`, `endpoint_id`, and JSON arrays `alpns`, `direct_addresses`, and
`relay_urls`. Static `PeerCandidate` configuration is also supported.

## Optional managed ACL mode

`unibus-mycelium`'s `managed-acl` feature provides `MyceliumAccessPolicy`.
Operator-owned directional rules bind authenticated source/destination
producers and exact message kinds to canonical peers, a principal, and a grant
scope. These bindings are not read from caller payloads. Mycelium's local
authority validates its own grants; Unibus denies unavailable, revoked,
expired, unknown-peer, or unbound access. Blocking authority queries run outside
the async executor and are bounded to eight concurrent workers. No stale allow
cache is used.

This is appropriate for control messages, not an authority RPC per audio packet.
Before extending it to high-rate media, design an explicit expiring policy lease
with revocation semantics. Existing standalone authorization remains available;
Mycelium is not a hard dependency. OIDC login/enrollment and grant issuance remain
control-plane responsibilities and are not implemented by this adapter.

## Manual verification / embedding runbook

1. Build with `cargo check -p unibus-iroh --features transport`.
2. Run `cargo test -p unibus-iroh --features transport` for encrypted loopback.
3. Provision separate persistent application keys and a trusted session issuer.
4. Bind restricted endpoints with `unibus/peer/1`; bound concurrent admissions
   and streams at the embedding service, and disable unsolicited public lookup.
5. Configure exact producer/owner bindings and allowed relay origins.
6. Construct `IrohListener` and `IrohTransport` with a cryptographic verifier;
   optionally attach directional `MyceliumAccessPolicy` rules.
7. Confirm denied keys, stale discovery, revoked grants, and session expiry
   fail closed before connecting production publishers.

Admission is limited to 15 seconds and 64 KiB. Message operations have a five
second deadline capped by session expiry; idle receive therefore returns an
explicit timeout. Frames are bounded by Unibus's existing `MAX_FRAME`.
Applications must handle these errors and request fresh authorization explicitly.
