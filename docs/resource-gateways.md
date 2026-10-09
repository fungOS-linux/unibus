# Resource gateways

One local Unibus router can serve as an explicit gateway for registered local
applications. It reuses the existing daemon and authenticated adapter sessions;
it does not create another service, scan LANs, or require Iroh. Mycelium catalog
publication uses an optional owner catalog; authenticated adapters can register,
renew and release leases. Live Canvas/device pilots remain follow-up work under
`unibus-resource-gateways`.

## Ownership

Mycelium owns observations, provenance and gossip. A protocol adapter explicitly
claims that it can serve a resource; Unibus owns routing and access enforcement.
A camera without Unibus uses a nearby adapter. The adapter speaks the native LAN
protocol; the remote client speaks authenticated Unibus to the gateway. A secure
gateway does not secure an unauthenticated/plaintext camera against LAN clients.

`ResourceId` is a structured scope plus key. Use `site` scope for legacy devices
without verified global identity, and `peer` scope for machine-owned local apps.
Never identify a device by bare private IP across sites. `GatewayLease` separates
the resource, canonical gateway machine, source service and existing transport
candidate. Multiple gateways may advertise the same resource independently.
Operations describe adapter capabilities, not caller permissions. Native URLs
and backend credentials are not part of the gateway lease.

## Local app routing

The existing router loads an optional `UNIBUS_GATEWAY_CONFIG` JSON file and adds
`GatewayRouter` before ordinary stream routing. No file means this slice adds no
gateway routes. The file contains:

- `gateway_peer_id`: canonical owner of these gateway leases.
- `policy_expires_at_unix_s`: absolute freshness bound, at most 300 seconds from
  startup. There is no indefinite offline allow or automatic renewal.
- `routes`: explicit lease plus authenticated `backend_producer` per resource.
- `grants`: exact caller producer, authenticated subject, resource, operations
  and expiry. Empty grants deny every invocation; no wildcard export exists.

The gateway caller must already pass normal router authentication and publication
checks. An ACL subject is matched to that authenticated producer's existing node
grant, never a subject supplied in a request. Backend publication must separately
allow `unibus.gateway.result.v1`; callers publish `unibus.gateway.invoke.v1`.
Shared-token nodes with no authenticated subject are not admitted by this slice.

Invocation message body (control only, no binary payload):

```json
{
  "schema_version": 1,
  "request_id": "req-001",
  "resource": {"scope": {"kind": "peer", "id": "studio-peer"}, "key": "canvas-tv"},
  "generation": 1,
  "operation": "canvas.control",
  "input": {"action": "list"}
}
```

The router selects only the configured adapter. The forwarded body is a
`GatewayDispatch` containing `caller_producer` and `invocation`; the original
authenticated envelope producer is retained and the bearer token removed.
`input` belongs to the resource's protocol adapter: it is not a generic URL or
TCP proxy. An adapter must independently validate URLs/paths and redirects if
its own protocol supports them.

The backend returns `GatewayResult` under `unibus.gateway.result.v1`, including
`caller_producer`, `request_id`, `resource`, `generation`, and bounded `output`.
The router requires the configured backend identity and matching pending request,
rechecks access and lease freshness, then forwards only to the original caller.
Other producers cannot spoof responses or select arbitrary recipients.

There are at most 128 routes, 512 grants, 256 active requests and 4096 retained
request IDs. Requests time out after five seconds or earlier envelope/lease expiry.
Duplicate IDs, including completed or uncertain deliveries, are rejected until
policy expiry. This is not durable exactly-once execution across daemon restart;
side-effecting adapters must own durable idempotency. Network delivery does not
hold the gateway state mutex. Input and output are limited to 64 KiB.

## Manual runbook

1. Keep the existing router installation; provision a local adapter's normal
   authenticated producer and publication grant. Do not expose its private socket.
2. Choose stable resource scope/key and canonical gateway owner. Record the actual
   native authentication/encryption posture, including `unknown` where unverified.
3. Construct a fresh explicit `GatewayLease` with generation, observed/expiry times,
   supported operations and a validated gateway transport candidate. Its expiry
   cannot exceed the candidate expiry or 300 seconds from observation.
4. Create a bounded gateway config mapping that resource to the adapter producer.
   Add only intended caller subject/resource/operation grants; keep all other access
   denied. Policy and grant expiry are absolute, not stale config auto-renewal.
5. Start the existing router with `UNIBUS_GATEWAY_CONFIG=/private/gateway.json` and
   its existing normal config. Malformed/expired policies fail startup loudly.
6. Send an invocation from an authorized registered caller. Confirm the resource
   generation and adapter result match; verify a different subject/resource/action
   and spoofed backend reply are denied.
7. Let the policy expire and confirm both new calls and outstanding results fail
   closed. To remove this static pilot, unset the variable and restart the router.

## Adapter registration and catalog publication

Configured routes are operator permits, not evidence that an adapter is alive.
The authenticated backend publishes `unibus.gateway.register.v1` with a body
`{"lease": <GatewayLease>}` to register or renew its permitted resource. Its normal
node publication grant must explicitly allow that kind. Registration cannot change
the backend, service identity, gateway owner, or expand the permitted operations.
Lease expiry cannot exceed policy expiry. Endpoint, operation or security-posture
changes require a newer generation; changed renewals require a newer observation.

Publish `unibus.gateway.release.v1` with `{"resource": <ResourceId>, "generation": N}`
to release the current generation. A lower generation cannot resurrect it within
this router process. Optional durable floors also prevent restart resurrection;
see below. Static route invocation remains compatible, but only explicitly registered
leases are exported as live observations.

Build with `--features owner-catalog` and set `UNIBUS_GATEWAY_CATALOG` to an explicit
absolute, dedicated owner file, normally Mycelium's `application-services.json`.
The parent directory must exist. The publisher atomically writes bounded, private
shared-schema observations and refuses to overwrite a different owner's catalog.
Without the feature, setting the variable fails startup. Initial publication is
empty; successful registration/release republishes the current registered leases.

Mycelium reads that file during its existing scan and carries observations through
the existing signed topology path. It preserves original expiry and loopback hints;
it does not proxy traffic or grant access. The generic `GatewayDirectory` consumes
service observations with an explicit trusted owner-to-producer mapping, verifies
outer/inner lease agreement, and rejects conflicting duplicates. Discovery is not
authorization: the destination Unibus router still checks every invocation/result.

A release removes the local route immediately. Previously gossiped observations
may remain until their original TTL expires; clearing the file is not a distributed
revocation mechanism. Policy renewal/revocation is not yet dynamic. No live service
restart/deployment or production mTLS/Iroh gateway qualification has been performed.

## Durable generation floors

Set `UNIBUS_GATEWAY_STATE` to an absolute state-file path in an existing owner-private
directory (mode 0700 on Unix). The router holds an exclusive OS lock for its lifetime,
rejects symlinks/unsafe file permissions and validates the bounded schema and owner.
Atomic state replacement is synced before registration/release changes are applied
or acknowledged. A storage failure rejects the operation without changing live
routes. Startup merges persisted floors monotonically and suppresses retired routes
even if the operator's initial config still contains them. Adapter re-registration
at a new generation is required; availability itself is not persisted.

Retired resource floors are retained even if a resource leaves the config, with a
128-resource bound. Removing/resetting this file deliberately removes rollback
protection; preserve it in backup/restore and don't run competing routers against
different state files for the same owner. This is crash durability, not protection
against an administrator restoring an older disk snapshot. A missing state file
initializes a new store. Without the variable, legacy in-memory behavior remains.
It does not provide durable request idempotency, policy revision tracking, or ACL
renewal. Those remain separate from adapter lease renewal.

## Verification

Run on the build host:

```sh
cargo check -p unibus -p unibus-router --tests
cargo test -p unibus -p unibus-router
cargo test -p unibus -p unibus-router --features owner-catalog
```

Tests cover scoped identity collisions, strict schemas, lease owner/freshness,
exact ACL matching, generation changes, local-app dispatch/result, policy expiry
blocking outstanding results, spoofed adapters and replay. Unit tests use recorded
router sinks. An additional integration test starts the actual router and two
loopback TCP clients, authenticates them through a test-only assertion verifier,
and exercises authorized request/result plus denied operations. This plaintext
local TCP fixture is not evidence of production Canvas, mTLS/Iroh transport or
cross-site connectivity.
