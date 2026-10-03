# Direct sessions and router discovery

The Unibus router may provide authorization and rendezvous, but it is an
optional role rather than the center of the data plane. Signed installation
policy or another trusted issuer can authorize the same direct-session
contract.

1. Both peers register with a trusted router.
2. The initiator requests a direct session for exact message kinds.
3. The router checks both grants and signs `DirectSessionClaims` binding both
   producer identities, both peer keys, candidate endpoints, limits, and a
   short expiry.
4. Each peer validates the signature through its configured trust provider,
   validates the claims locally, and attempts candidates in policy order.
5. If direct QUIC cannot connect, the ordinary routed path remains available.

Unibus core exposes two independent boundaries:

- `PeerDirectory` resolves an already-authorized peer identity into fresh
  `PeerCandidate` endpoints.
- `PeerTransport` opens a message-oriented session using a signed session
  assertion.

`unibus-quic` adds a third, transport-specific boundary: `PeerCredentials`
loads a certificate chain, private key, and trust roots from the deployment's
identity provider. Unibus consumes those credentials for its own mTLS session;
it does not issue or rotate them. `PemPeerCredentials` is the standalone file
provider. `MyceliumCredentials` uses the stable `pki/{ca,node,node-key}.pem`
layout installed by Mycelium enrollment.

`StaticPeerDirectory` provides the standalone baseline. DNS implementations
can project SRV/A/AAAA answers into the same candidates. The optional
`unibus-mycelium` crate queries Mycelium's newline-delimited local JSON-RPC
socket and performs the same projection without linking Mycelium libraries or
introducing any Unibus dependency into Mycelium.

## Direct QUIC transport

`unibus-quic` owns the QUIC framing and TLS implementation shared by the hub
and direct peers. `DirectQuicTransport` implements the client side of
`PeerTransport`; `DirectQuicListener` accepts direct sessions without starting
a hub.

Direct sessions require mutual TLS. Each side hashes the other peer's leaf
certificate and requires the resulting `sha256:<hex>` value to equal that
peer's `key_binding` in the verified `DirectSessionClaims`. The first frame is
an authentication frame carrying the signed assertion. Subsequent frames must:

- come from the producer bound to that side of the connection;
- use the authorized installation and exact allowed message kind;
- stay within the session payload bound;
- carry no registration bearer token; and
- remain within the ordinary Unibus envelope TTL.

The standalone flow is:

```text
Static/DNS PeerDirectory -> DirectQuicTransport -> peer
```

The Mycelium-assisted flow may reuse identity and discovery independently:

```text
MyceliumDirectory   -> peer endpoints
MyceliumCredentials -> Unibus-owned QUIC/mTLS -> peer
```

Mycelium never carries or interprets Unibus frames. Unibus authorization and
session assertions remain separate from certificate issuance: the certificate
authenticates the peer key, while verified Unibus claims decide which messages
that peer may publish or consume. Deployments may use either adapter, both, or
neither.

`connect_direct_or_else` exposes routed fallback as an explicit callback and
returns whether the selected path was `Direct` or `Fallback`. A failed direct
connection never silently weakens authentication or chooses a router.

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
