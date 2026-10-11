# Optional daemon viewer

The router's read-only HTTPS index uses Coherence's product design system.
It is disabled by default and does not discover hosts, forward arbitrary URLs,
or expose service controls. Mycelium remains responsible for discovery and DNS.

## Enable

Build `cargo build -p unibus-router --features viewer`. Set
`UNIBUS_VIEWER_CONFIG` to an absolute path to an owner-only JSON file (mode
0600). Without that variable, the feature-enabled binary opens no viewer
listener. Setting it on a binary without the feature is an explicit error.

Example configuration, replacing all credential paths and certificate digest:

```json
{
  "listen": "127.0.0.1:8444",
  "ca_file": "/absolute/path/ca.pem",
  "cert_file": "/absolute/path/server.pem",
  "key_file": "/absolute/path/server-key.pem",
  "certificate_bindings": {
    "REPLACE_WITH_64_LOWERCASE_HEX_SHA256_OF_CLIENT_CERTIFICATE_DER": "viewer-reader"
  }
}
```

Use an explicit interface address, not a wildcard. The server certificate must
cover the hostname used by the browser. The browser needs its own client
certificate signed by the configured CA; never export a peer's private key to
make browser access work. Calculate the client certificate fingerprint with
`openssl x509 -in client.pem -outform DER | openssl dgst -sha256`.

Bind that fingerprint to an existing router node grant identity. That grant
must include `unibus.viewer`, the capabilities it may inspect, and the zones
it may inspect. CA membership alone is not authorization. These are existing
static router grants, not a new OIDC or Mycelium signed-grant implementation.

Open `https://HOST:8444/` with the authorized client certificate. `/api/index`
returns the same narrow JSON projection used to render the page. Search and
refresh are normal GET navigation; this first slice does not auto-refresh.

## What the index means

- Registered nodes appear only when both zone and capability intersect the
  reader's grant. Heartbeat age determines live versus stale.
- Capability-targeted configured routes appear only for allowed capabilities.
  "Configured" does not claim a working producer or healthy stream.
- Credentials, endpoint URLs, screen anchors, raw payloads, producer-targeted
  routes, and other zones are not exposed.
- This is the local daemon's authorized view, not a complete mesh inventory.
  Media previews and adapter-specific detail contracts are future slices.

Requests are GET-only, bounded by header/body budgets, 32 concurrent handlers,
and a 10-second connection deadline. Responses forbid caching and framing,
use a restrictive CSP, and expose no cross-origin API permission. Oversized
inventories fail rather than silently truncating.

Certificate bindings are reread per request: removing a binding revokes access
without a restart. Invalid configuration fails closed. Static router grant
changes, server certificate changes, and listener changes require restart.
Unset the variable and restart to disable the listener.

## DNS boundary

A Mycelium-managed DNS name can point at this listener once appropriate routes,
TLS identity, and access policy exist. DNS is not authorization, and picking a
reachable resolver or address is not IP anycast. This feature does not change
DNS, firewall rules, Mycelium advertisements, or fleet configuration.

## Verify

```sh
cargo check -p unibus-router
cargo check -p unibus-router --features viewer
cargo test -p unibus-router --features viewer viewer::tests
```

Tests exercise capability/zone filtering, stale heartbeats, route semantics,
escaping, request parsing, configuration limits, and inventory overflow.
Before LAN deployment, additionally verify client-certificate acceptance,
unbound-certificate rejection, and binding revocation end to end with the
deployment CA. No live deployment is implied by these unit tests.
