# unibus-holodeck: room sharing between LANs

Status: the first static, mutually authenticated bridge is implemented and
deployed on Agora and Titan. Headless discovery and ENet joins passed in both
directions with two independent clients. Titan's selected `192.168.1.0/24` LAN
is explicitly authorized for roaming headsets, per the user's instruction.
A real Quest join, rendering and voice remain unverified.

Verification on 2026-10-06: six adapter tests passed on Neo and Agora, eight
existing Holodeck server tests passed on Agora, and both deployed hosts passed
limited-broadcast discovery plus two-client ENet joins. Titan also passed the
unicast path, and Neo joined Titan through Agora's real LAN endpoint. A live
exporter restart recovered in approximately eight seconds with the lease-bound
reconnect logic. These join/recovery measurements are not steady-state pose or
voice latency benchmarks.

## Implemented slice

`unibus-holodeck run CONFIG.json` exports opted-in Holodeck host endpoints or
imports them onto a selected LAN. `check CONFIG.json` validates policy without
opening listeners. It reuses `unibus-quic` mTLS configuration/endpoints and pins
the expected peer certificate; it does not require a regional router. LAN imports
currently require Linux; other platforms can run loopback tests only. The LAN
codec is a Git-pinned dependency on Holodeck's shared discovery crate.

Each imported host has a distinct local UDP relay port. Each admitted client's
source IP/port gets a separate upstream socket. QUIC datagrams carry bounded
fragments, preserving the original UDP datagram on reassembly without a reliable
stream fallback. Pending fragments expire after 250 ms. The adapter supports up
to sixteen imported host endpoints, thirty-two clients per host connection, and
8192-byte room datagrams. These are admission/resource bounds, not benchmarks
of sixteen simultaneous occupied rooms.

Linux imports receive both unicast discovery and the Quest's limited-broadcast
probes. They bind a separate exact `255.255.255.255` discovery socket and send all
replies through the selected LAN-address socket; no wildcard listener or socket
reuse is needed. One import process owns the broadcast port per host. Local
source-address/subnet policy applies to both kinds of probe. This is not a generic
multicast forwarder or a cryptographic LAN-interface identity check.

An export probes its configured Holodeck discovery endpoint every two seconds;
the importer withdraws discovery when the host reports unavailable or its
six-second connection-scoped lease expires. Importers reconnect automatically
after lease expiry rather than waiting for QUIC's longer idle timeout. Established
headset sessions may need to rejoin after a bridge restart; this does not preserve
ENet host state across transport generations.
Client sockets retire after sixty seconds without client traffic. Named room
creation/membership remains in Holodeck; sharing a host endpoint permits access
to the logical rooms hosted there, not just one named room. Room-level access
control would need a supported Holodeck admission interface.

Mycelium resolved the hosts for deployment; this slice uses explicit private
endpoint configuration as its bootstrap. Automatic Mycelium service promotion,
gossiped room projections and grant-bound bus control publication remain follow-up
work. It does not silently turn listeners into discoverable capabilities.

### Deployment endpoints

| Host | Imported remote host | Local discovery | Local join relay | Private export |
| --- | --- | --- | --- | --- |
| Titan | Agora | `192.168.1.9:6788/udp` | `192.168.1.9:6790/udp` | `100.83.7.116:16788/udp` |
| Agora | Titan | `192.168.2.67:6788/udp` | `192.168.2.67:6790/udp` | `100.80.85.86:16788/udp` |

Both room servers bind only `127.0.0.1:6789` with discovery on loopback UDP/6788.
Their native ports are not exposed across the tailnet. Each host has enabled user
services `holodeck-room-host`, `unibus-holodeck@export` and
`unibus-holodeck@import`; user lingering keeps them running after logout.
Dedicated certificates expire after 365 days and need explicit renewal and peer
binding updates. Private keys stay on their host; the dedicated CA key is retained
on Agora under the owner's private configuration directory.

### Manual runbook

1. Build and test with `cargo check -p unibus-holodeck`, then
   `cargo test -p unibus-holodeck`. Build release artifacts only for deployment.
   Linux artifacts were built on Agora. For builders without private Git access,
   transfer a verified bundle of the pinned Holodeck revision and use a scoped Git
   URL rewrite for that build; never transfer Git credentials or change global
   Git configuration.
2. Install the bridge, `holodeck-probe` and the existing `holodeck-server` into
   the owner's `.local/bin`. Adapt the example JSON configs with independently
   verified private addresses and certificate SHA256 bindings. A placeholder
   zero binding in an example is not a usable deployment identity.
3. Keep TLS keys and configs under `.config/unibus-holodeck` with owner-only
   permissions. Validate both configs before enabling the supplied user services.
   Use a distinct LAN bind from the loopback room-server discovery responder.
4. Add authorized local clients to `allowed_clients`, or authorize the selected
   LAN explicitly using `allowed_subnets` (Titan uses `192.168.1.0/24`). Subnets
   must be private and contain the chosen LAN bind address; public/wildcard or
   unrelated-site ranges fail validation. All devices in an allowed subnet can
   join exported hosts; LAN/subnet membership is not cryptographic identity.
   Restart the importer with `systemctl --user restart unibus-holodeck@import`.
5. Verify `systemctl --user is-active holodeck-room-host
   unibus-holodeck@export unibus-holodeck@import`, inspect its journal, and run
   `holodeck-probe LAN_IP test-room 2`. The probe follows ordinary LAN discovery,
   joins via ENet, reports welcomes, and gracefully disconnects its test clients.
   Run `holodeck-probe LAN_IP test-room 2 --broadcast` on the LAN host to verify
   the headset's broadcast discovery path as well as unicast.
   Headless success is not proof of Quest UI, rendering or voice behavior.
6. On the Quest, use Holodeck's room list. The client creates a selectable empty
   `lobby` entry for a connected host even before anyone creates a room. Verify
   both peers see membership/pose updates; qualify voice independently.
7. Roll back with `systemctl --user disable --now unibus-holodeck@import
   unibus-holodeck@export holodeck-room-host`. Remove lingering only if no other
   user services depend on it. No firewall or routing changes were made.

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

The deployment uses no blanket multicast forwarding, public listeners, automatic
room sharing or firewall/routing changes.
