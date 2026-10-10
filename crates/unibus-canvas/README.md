# Canvas endpoint status

`canvas-fleet` is the local multi-endpoint companion to `canvasctl`. It calls
the existing owner inspection interface; it neither scans for services nor
changes workspaces. Gateway drivers leave remote transport to Unibus.

```sh
canvas-fleet status
canvas-fleet status desktops
canvas-fleet status neo,studio --json
canvas-fleet status --config /path/to/canvas-fleet.json
```

Without a selection, status queries every target registered in
`$XDG_CONFIG_HOME/unibus/canvas-fleet.json`, or
`$HOME/.config/unibus/canvas-fleet.json`. It reports the active workspace,
revision, and Canvas surfaces (not the operating system's entire native window
inventory). Missing fields stay unknown. Each failed endpoint remains visible;
the command exits nonzero if any endpoint fails.

The existing explicit commands remain available:

```sh
canvas-fleet CONFIG GROUP_OR_TARGETS inspect
canvas-fleet CONFIG GROUP_OR_TARGETS switch WORKSPACE
```

Command drivers default to correlated schema-v1 owner replies. For the
ADB-authorized `canvas-tvctl` helper, an operator may explicitly configure
`"reply_contract":"presentation_transaction"`. That helper performs one request
per private-token-authenticated connection and returns a presentation receipt
without wire request IDs. The local adapter validates its boolean outcome and
revision, rejects conflicting IDs/versions, and marks normalization with
`reply_provenance: single_authenticated_transaction`. It does not claim
wire-level correlation. Never select this contract for a multiplexed client;
Unibus gateway adapters always retain strict correlated replies.

TV status reads the active presentation document; desktop status reads the
owner snapshot. Revision always means the endpoint's control revision, not the
document revision.

Registration currently means the local target config;
automatic Mycelium catalog ingestion and live Iroh qualification are separate.
