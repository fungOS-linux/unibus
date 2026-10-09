# Canvas resource adapter pilot

`unibus-canvas` calls the installed owner-local `canvasctl` CLI. Canvas remains the
source of truth for its control protocol, private socket checks, workspace state
and revision enforcement. The adapter is a separate crate, not UI logic inside
the router. Dispatch/result schemas live in `unibus::gateway`; the router reexports
them for compatibility.

Only two exact operations are implemented:

- `canvas.inspect`: empty input object; returns the same instance's Canvas snapshot.
- `canvas.workspace.switch`: `{"name":"television","expected_revision":2}`. A revision
  is mandatory; Canvas handles stale revisions and unknown workspaces visibly.

Caller input cannot choose executable paths, sockets, URLs or arbitrary Canvas
commands. CLI output is bounded to 60 KiB, correlated by schema/request ID, and has
a three-second deadline. CLI failures are explicit; Canvas error outcomes are not
reported as successful actions. Oversized inspection documents fail rather than
being silently trimmed. A future retained/chunked projection can handle large ones.

## Runbook

1. Install `canvasctl` and run the adapter as the Canvas owner. Verify that local
   inspection succeeds first. Do not expose Canvas's private socket remotely.
2. Provision an authenticated schema-2 adapter producer on the existing owner-local
   router. Permit only gateway registration/result publication. Keep caller grants
   exact by authenticated subject, resource and operation.
3. Configure matching resource, generation, owner, operations, backend and a fresh
   lease in the router's gateway policy and the adapter config below. Keep token
   material in the private token file, not in inventory advertisements.
4. Run `unibus-canvas /private/canvas-gateway.json`. It registers through the existing
   router and fails on registration rejection, expired lease or rejected results.
   It requires a loopback hub endpoint; remote clients use the router's separately
   authenticated transport, not plaintext adapter TCP across hosts.
5. From an authorized caller, invoke `canvas.inspect`, then use that same resource,
   generation and returned revision for any workspace switch. Verify a different
   resource/operation/subject is denied. Do not infer authorization from discovery.

Adapter config shape (lease is the exact shared `GatewayLease` from the router):

```json
{
  "unibus": {
    "installation_id": "home",
    "producer_id": "canvas-gateway",
    "hub_endpoint": "127.0.0.1:18790",
    "token_file": "canvas-gateway.token",
    "zones": ["default"],
    "schema_version": 2
  },
  "adapter": {
    "resource": {"scope":{"kind":"peer","id":"home-pi"},"key":"canvas"},
    "canvasctl": "/usr/local/bin/canvasctl"
  },
  "lease": "REPLACE_WITH_FRESH_GATEWAY_LEASE_OBJECT"
}
```

This is a finite-lease pilot, not an indefinitely running deployment: no automatic
ACL renewal, reconnect, background privilege issuance or production installation
is implied. Enable router `UNIBUS_GATEWAY_STATE` for durable generation floors.
Managed policy refresh, DCP revision-bound
action discovery and cross-host mTLS/Iroh qualification remain separate work.

## Verification

On Agora, run `cargo check -p unibus-canvas -p unibus-router --tests`, then
`cargo test -p unibus-canvas -p unibus-router`. Cross-build the `gateway` integration
test for Home-Pi using `aarch64-linux-gnu-gcc`. Run the ignored
`live_canvas_inspection_through_authenticated_gateway` test as the Canvas owner
with `UNIBUS_CANVASCTL=/usr/local/bin/canvasctl`.

The live test starts an isolated loopback router with a test-only verifier, registers
its adapter, inspects the real Canvas, verifies reply correlation, denies an
ungranted operation, and denies access after release. It does not modify the TV
workspace, production daemon, trust store or grants. It is not evidence of production
OIDC/mTLS/Iroh or cross-site discovery.
