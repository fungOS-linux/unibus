# QUIC transport runbook

The shared `unibus-quic` crate owns QUIC framing and TLS primitives. The
library hub exposes `Hub::start_quic(address, server_config)` alongside the
v1-compatible TCP listener. The `unibus-hub` CLI starts QUIC automatically when
its configuration contains a `quic` object. QUIC is TLS 1.3-capable through Quinn/Rustls and
uses the same envelope codec, registration assertions, grants, routing, and
plugins as TCP.

## Provision trust material

1. Issue a server certificate whose SAN matches the name clients use.
2. Keep its private key readable only by the hub service account.
3. Pin the issuing CA/certificate in each client `RootCertStore`.
4. Construct the server with `quic::server_config` and clients with
   `quic::client_config`. Empty certificate chains and empty client root stores
   fail closed.
5. Do not use the test-only `rcgen` dependency for production certificates.

TLS authenticates and encrypts the channel. It does not replace the first
Unibus `register` frame: nodes must still present their shared compatibility
token or fpl-auth assertion, and every later envelope is bound to that producer.

## Start and verify

1. Add the listener and PEM paths to `router.json`:

   ```json
   "quic": {
     "listen": "0.0.0.0:18791",
     "cert_file": "tls/server.pem",
     "key_file": "tls/server-key.pem"
   }
   ```

   Paths are relative to the configuration file. Missing, empty, or malformed
   PEM material fails startup before the daemon enters its service loop.

2. Run `unibus-hub /path/to/router.json`. The configured TCP compatibility
   listener and QUIC listener start together.
3. When embedding, build the normal `Hub` and install the fpl-auth `OfflineVerifier` when any
   grant uses `auth_subject`.
4. Call `start_quic` with an explicit UDP address and TLS server configuration.
5. Connect a `QuicEndpoint` client using the pinned roots and expected DNS name.
6. Send `register` as the first frame and require a successful `reply`.
7. Send a command and verify its correlated reply and destination delivery.
8. Send a small `audio.playback_chunk` or `presence.snapshot`; `QuicPeer::send`
   should report `Datagram` when the negotiated maximum permits it.
9. Send a frame larger than the negotiated datagram size and verify it reports
   `OneWayStream`.

Commands and datagram-ineligible messages use reliable one-way streams. QUIC
stream acceptance order preserves command submission order. Eligible stream and
snapshot traffic uses RFC 9221 datagrams when it fits, otherwise a one-way
stream. All variants carry the identical JSON-plus-binary frame body.

## Reconnect and rollback

Quinn handles connection migration while the connection remains valid. After a
connection is lost, reconnect and register again; the hub re-runs authentication
and grant admission before restoring the producer slot. Never transfer session
state to a new connection without registration.

Rollback is transport selection, not a protocol conversion: stop accepting the
QUIC UDP listener and reconnect through loopback TCP behind an SSH/Tailscale
tunnel. Do not enable `allow_insecure_lan` as an automatic fallback. The golden
v1 TCP vectors and the QUIC tests exercise the same frame codec.

For hub-free peer sessions, use `DirectQuicListener` and
`DirectQuicTransport` with mutual TLS plus verified `DirectSessionClaims`.
Certificate fingerprints must match the claims' peer key bindings. See
`direct-sessions-and-discovery.md`; ordinary router QUIC and direct QUIC share
the same codec rather than maintaining separate implementations.
