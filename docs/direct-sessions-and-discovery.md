# Direct sessions and router discovery

The Unibus router remains the authorization and rendezvous control plane. It
does not need to remain on the data path for an authorized peer session.

1. Both peers register with a trusted router.
2. The initiator requests a direct session for exact message kinds.
3. The router checks both grants and signs `DirectSessionClaims` binding both
   producer identities, both peer keys, candidate endpoints, limits, and a
   short expiry.
4. Each peer validates the signature through its configured trust provider,
   validates the claims locally, and attempts candidates in policy order.
5. If direct QUIC cannot connect, the ordinary routed path remains available.

Discovery proves reachability, never authority. Candidates from a local probe,
DNS, static configuration, or Mycelium all enter through `EndpointSource` as
validated `NetworkEndpoint` facts and
have the same untrusted status until TLS and the signed session bind them to an
expected identity.

## DNS boundary

Router discovery uses DNS SRV semantics. QUIC routers for installation
`wagar-house` under `mesh.fpl.dev` are published at:

```text
_unibus._udp.wagar-house.mesh.fpl.dev. 60 IN SRV 10 100 18791 home-pi.mesh.fpl.dev.
_unibus._udp.wagar-house.mesh.fpl.dev. 60 IN SRV 20 100 18791 studio.mesh.fpl.dev.
```

The SRV target resolves through ordinary A/AAAA records. The target name is
also the TLS/QUIC server name. IP literals require an explicit authenticated
server name; discovery must never disable certificate verification.

Mycelium may reconcile these records from observed topology and router health,
or expose an `EndpointSource` adapter. Unibus does not import Mycelium types,
read Mycelium state, or require a Mycelium daemon. The same installation works
with hand-managed DNS, another DNS controller, or static endpoints.

DNS publication is desired state and should be conservative: only configured
Unibus router identities may receive records. Merely discovering a process or
open port does not authorize publication. Low TTLs support failover; clients
retain the last valid answer only through its TTL and fail loudly when every
candidate is unavailable.
