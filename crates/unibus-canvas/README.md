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

Command drivers currently require correlated schema-v1 owner replies. A TV
helper returning an uncorrelated presentation receipt is reported as an error,
not silently accepted. Registration currently means the local target config;
automatic Mycelium catalog ingestion and live Iroh qualification are separate.
