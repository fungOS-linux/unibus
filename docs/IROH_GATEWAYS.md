# Iroh relay gateways

`unibus-iroh` projects gateway facts; upstream `iroh-relay` moves encrypted
packets. Neither a discovery observation nor relay admission authorizes an
application request. Yggdrasil owns its HTTP tunnel, and Unibus owns its payload
and grants. Mycelium remains the discovery owner.

## Narrow interface

`gateways(&dyn ServiceDirectory, now_unix_s)` returns bounded, fresh,
credential-free HTTPS relay origins with service identity and observer
provenance. Existing `MyceliumServiceDirectory` work can supply this trait; the
adapter does not scan networks or reproduce Mycelium discovery. A deployment
observer can also supply `StaticServiceDirectory`.

`unibus.iroh.gateways.v1` contains `{schema_version: 1, gateways: [...]}`.
The router treats it as a snapshot. Observed time and expiry belong to the
observer, not the publisher; the publisher never extends a lease. Conflicting
identities and unsafe URL forms fail closed. Query tokens, credentials, private
keys, and TLS private material never enter the catalog.

The CLI accepts a JSON file containing `observations` (the public Unibus
`ServiceObservation` contract) and optionally `client` (`ClientConfig`).

```text
unibus-iroh list DIRECTORY.json
unibus-iroh publish DIRECTORY_AND_CLIENT.json
```

Publication requires an authenticated producer authorized for the catalog kind
and a router route to holders of `iroh.gateway.viewer`. Producer registration
and subscriber enrollment remain deployment policy. A gateway being visible
does not mean a caller is on its endpoint-key allowlist, or that TLS trust has
been installed. The CLI currently loads an explicit directory snapshot once;
use the Rust trait for a live observer-backed directory.

## Agora gateway

The deployment source is `shared-infra/projects/iroh-relay`. It uses maintained
upstream `iroh-relay` 1.3.0, not another custom proxy. Initial access is deny-all.
HTTPS is planned at `https://192.168.2.67:14041/` (LAN) and
`https://100.80.85.86:14041/` (private overlay), with a scoped certificate from
the existing Mycelium mesh CA. Only advertise reachable, verified origins.

Iroh clients explicitly configure trust using
`iroh::tls::CaTlsConfig::custom_roots(...)` or `.with_extra_roots(...)`, supplied
to `Endpoint::builder(...).ca_tls_config(...)`. Never disable verification.
TLS cert/key material lives on a dedicated read-only guest block volume, not
in the rootfs artifact. Adding endpoint public keys to the relay allowlist is a
separate operation from granting access to Unibus topics or inference.

## Verification

Build on Agora: `cargo check -p unibus-iroh`, then
`cargo test -p unibus-iroh`, then `cargo build -p unibus-iroh`.
Test fresh/expired leases, credential-bearing URL rejection, conflict rejection,
and provider provenance. Live qualification additionally requires verified TLS,
two allowed endpoints exchanging bytes through the relay, an unlisted endpoint
being denied, and receipt of the authenticated Unibus catalog. HTTP readiness
alone is not that qualification.
