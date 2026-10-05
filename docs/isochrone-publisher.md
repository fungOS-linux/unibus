# Isochrone audio publication

Build the local publisher with `cargo build -p unibus-media --features isochrone --bin unibus-isochrone`.
The optional integration uses the sibling Isochrone repository's public Rust control API.

Run `unibus-isochrone /absolute/path/publisher.json` with:

```json
{
  "bus": {
    "installation_id": "home",
    "producer_id": "studio-isochrone",
    "hub_endpoint": "127.0.0.1:18791",
    "token_file": "/absolute/path/unibus.token",
    "zones": ["studio"],
    "capabilities": ["audio.publish"],
    "schema_version": 2
  },
  "agent": {
    "node_id": "mac-studio",
    "address": "127.0.0.1:50100",
    "token_file": "/absolute/path/isochrone.token"
  },
  "state_dir": "/absolute/path/isochrone-state",
  "zone": "studio",
  "destination": "studio"
}
```

The router's producer grant must admit `stream.descriptor.v2`,
`stream.endpoints.v2`, and `media.audio.isochrone.status.v1` and the destination
zone. The agent must listen on an interface reachable at the configured
loopback address. `state_dir` is the same local directory configured on the agent.

The publisher polls once per second; announcements expire after three seconds.
Only running routes receive media endpoints. Missing or stale receiver health
is published as `null`, which means unknown. Stopped routes stop renewing their
endpoint leases. Polling or publication errors exit visibly so a service
supervisor can restart the publisher.

The current agent sender/receiver profile is 48 kHz stereo L24 with 48 frames per
packet. Route generations use the data-plane process ID. Endpoint references
remain logical route references; transport addresses and credentials stay with
the Isochrone agent. Receiving a descriptor does not itself start an audio route.

No physical audio device is opened by the publisher. Use the feature-enabled
media tests to verify loss, restart, wire geometry, and endpoint expiry before
deploying alongside an existing agent.

## Local verification

A loopback trial on 2026-10-04 ran the real Isochrone agent on port 19872,
the Unibus router on port 19871, the publisher, and `unibus-watch` as an
authorized observer. The observer received `media.audio.isochrone.status.v1`
with `node_id=local-audio`, `route_count=0`, a three-second TTL, and an empty
transport token after routing. No audio route was started. Trial processes
were stopped after verification.

Idle agents publish node status every second. Per-route status additionally
contains the route identity, generation, role, phase, and fresh receiver health.
The loopback trial establishes the authenticated control path; actual RTP
playback and process-restart recovery require a separate active-route trial.
