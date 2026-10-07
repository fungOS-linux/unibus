# unibus-holodeck: room sharing between LANs

Status: source-grounded design, not an implemented or deployed adapter.

## User scenario

Titan participates in Mycelium and the private tailnet. A nearby Quest is only
on Titan's LAN. The Quest should discover and join an explicitly shared remote
Holodeck room without installing Tailscale, and a locally hosted room should be
shareable in the opposite direction.

## Existing protocol

Inspected Holodeck source in the adjacent `../../../holodeck` repository:

- `discovery/src/lib.rs` owns the versioned ten-byte discovery codec: probe
  `HLDISC1?` plus two zero bytes; reply `HLDISC1!` plus a nonzero big-endian
  ENet port. Discovery listens on UDP/6788.
- `room-plugin/src/lan.rs` broadcasts probes every two seconds, retains replies
  for six seconds and caps its list at sixteen endpoints. The join endpoint is
  **reply source IP + advertised port**, not an address embedded in the reply.
- `server/src/discovery.rs` answers exact probes from private, link-local or
  loopback IPv4 sources; this source check is not user authentication.
- `server/src/main.rs` hosts ENet/UDP, defaulting to loopback UDP/6789, with two
  channels. It has no transport-authentication gate in its inspected event loop.
  This observation does not establish whether higher-level room messages have
  additional authorization.

Reuse Holodeck's codec through a pinned shared dependency when implementing;
do not duplicate wire constants or reach into its server internals. Resolve
dependency packaging deliberately rather than relying on a developer-only
absolute path.

## Boundaries

Mycelium discovers bridge hosts and advertised capabilities. Unibus carries
expiring room projections, authorized requests and the private bridge transport.
`unibus-holodeck` owns LAN probing, export/import policy, route leases and local
discovery replies. Holodeck owns room hosting, membership and room semantics.
The adapter may invoke a supported host command but must not implement a second
room server. Discovery or tailnet membership alone never grants room access.

## Connection path

```text
Quest on buddy LAN
  -> Titan LAN relay port
  -> authenticated private Unibus link
  -> origin bridge's per-client UDP socket
  -> explicitly exported Holodeck ENet endpoint
```

Titan answers the Quest's ordinary discovery probe with a dedicated local UDP
port for each imported room. Allocate and bind that port before advertising it.
The reply must originate from Titan's reachable LAN address. Never advertise
the origin's LAN address or a tailnet address that the Quest cannot reach.

Maintain one origin-side UDP socket per admitted client/session, preserving
ENet datagram boundaries and return routing. Do not terminate ENet or collapse
all Quest clients onto one upstream socket. Avoid running the realtime room
payload through retained/broadcast event streams. Reuse an authenticated Unibus
peer transport with a bounded datagram/session interface; qualify MTU behavior
before selecting it. No silent reliable-stream fallback with unbounded backlog.

The reverse direction uses the same mechanism: Titan probes explicitly selected
LAN interfaces, exports opted-in local room endpoints, and another bridge imports
them. A host running on Titan itself must have a reachable bind or an explicitly
allowed loopback export; its default loopback bind is not LAN hosting.

## Narrow projection and policy

Define a versioned expiring room lease with stable origin identity, stable room
identity where Holodeck can supply it, generation, label, opaque route reference,
observation/expiry times and provenance. Existing LAN discovery identifies only
endpoints, not room names or stable IDs; do not pretend it supplies richer data.
Until a richer host interface exists, configured labels and endpoint-scoped
identities must be marked as such. Endpoint identity must include origin/site,
not just IP:port: identical subnets at different sites are not identical rooms.

Publish no credentials or arbitrary client-supplied upstream addresses. Export
allowlists identify rooms and importing peer identities; import policy controls
which rooms become visible to the local LAN. Resolve route references only at
the owning bridge, against authorized exports. Prevent bridge loops by retaining
origin/provenance and rejecting re-exported imports. Expired projections withdraw
discovery immediately; define whether existing sessions drain or close under an
explicit policy. Bound room count, clients, sockets, bytes and idle time.

Authenticated bridge peers protect the cross-site leg, not the Quest-to-Titan
LAN leg. Initially restrict local ingress to an explicitly selected interface
and permitted clients, with rate limits and no WAN listener. IP allowlisting is
not cryptographic Quest identity; genuine per-user room admission remains a
Holodeck capability to inspect and qualify.

## Implementation and verification slices

1. Testable room lease/policy contract and read-only LAN discovery using the
   existing codec; no public listeners or deployment.
2. Local-loopback ENet forwarding harness with two simultaneous clients and
   authenticated bridge identities. Check ordered/reliable ENet behavior,
   disconnect/reconnect, packet loss/reordering, oversize rejection and expiry.
3. Local discovery responder tied to a ready relay lease. Test source IP/port,
   multiple rooms, withdrawal, duplicate origins, unauthorized exports and loops.
   Coordinate discovery UDP/6788 ownership with a co-located Holodeck server:
   socket reuse is not a reliable substitute for one explicit responder owner.
4. Integrate supported Mycelium advertisements and Unibus projections; verify
   stale or unauthorized room data cannot create a route.
5. With deployment authorization, prove both directions on real Quests: find,
   join, room updates, voice if present, disconnect and host restart. Measure
   latency/loss and report degraded links visibly. Hosting and discovery are
   not complete until a real client can join successfully.

No blanket multicast forwarding, public exposure, automatic room sharing or
firewall/routing changes are authorized by this design document.
