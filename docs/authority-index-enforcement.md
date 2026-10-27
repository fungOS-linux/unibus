# Authority, index, enforcement

The split between Mycelium, Unibus, and connectors is deliberate.

| Layer | Owns | Does not own |
|---|---|---|
| **Mycelium** | node identity, membership, keys, grants, delegations, revocation, generation floors, signed authority records, discovery scopes, gossip | envelope routing, QoS, backend calls |
| **Unibus** | envelopes, routing, QoS, TTL/replay, leases, target index, derived route/service cache, enforcement, correlated receipts | issuing grants or identities |
| **Connectors** | wire translation, external credentials, OpenAPI/RPC target discovery | ACL decisions or new authority |

## Rule

Unibus's grant table is a verified, expiring projection of Mycelium authority.
If Mycelium is unreachable, Unibus enforces last-known authority only until the
lease/TTL/generation floor expires, then fails closed.

```text
Mycelium signed grant/revocation
        → Unibus verifies and caches
        → Unibus enforces on every envelope
```

## Discovery

Mycelium is the discovery authority. Unibus consumes discovery and adds runtime
state:

- what is routable now
- which producer/target is live
- which backend/target descriptor is current
- what local lease applies

Unibus may run local observers, but they feed Mycelium; they must not become a
second discovery database.

## External targets

`unibus-targets` ingests OpenAPI/RPC descriptors into a bounded, content-addressed
index. The index contains routing metadata, not grants. Mycelium grants reference
target ids; connectors refuse anything outside the index.

OpenAPI import is not an ACL operation. It produces candidates for review.
Removed operations go stale and require an explicit grant update.
