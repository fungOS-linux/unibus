# Bounded JSON stream adapter

`unibus-stream` uses the existing authenticated client, `StreamEvent` and
renewable `StreamInterest` contracts; it does not discover peers or interpret
Canvas/provider state. It does not replace bulk-media transports.

```sh
# Existing one-shot interface remains supported (VALUE_JSON is inline JSON).
unibus-stream CONFIG.json publish STREAM CONTRACT '{"value":1}'
# Each newline-delimited JSON value becomes one event; maximum line is 64 KiB.
producer | unibus-stream publish CONFIG.json STREAM CONTRACT
# Renew 15-second interest every 5 seconds; emit validated JSON values.
unibus-stream subscribe CONFIG.json STREAM CONTRACT EXPECTED_PRODUCER | consumer
```

Client configuration is the existing `ClientConfig`; relative token paths
resolve against that file. Tokens never enter emitted JSON. Publisher grants
allow `unibus.stream.event.v1`; subscriber grants allow
`unibus.stream.interest.v1`. A stream source advertises `unibus.stream.source`.
Consumers reject wrong producer/stream/contract, replayed sequences, samples
older than seven seconds and timestamps more than one second in the future.
Owner observation timestamps inside values remain opaque and unchanged.
Only JSON is accepted: binary content belongs in its existing media transport.

Connection, authorization, malformed input and oversized input failures exit
visibly. Supervisors restart the pipeline; expired interests disappear after
15 seconds, including consumer crashes. Consumers must independently expire
their retained presentation if no event arrives. This is not a durable event
log and does not claim historical replay or cross-router mesh forwarding.

## Live Canvas installation

The existing Home-Pi loopback router is the authorized fan-out point.
Neo publishes the TV's `canvas.presentation.v1` values on
`canvas/tv/tardy/presentation` as `canvas-tv-presentation`. Neo and Studio
use distinct consumer identities and token files; each runs its receiver
locally. SSH forwards local port 19496 to Home-Pi loopback 18790, keeping
bearer credentials off plaintext LAN links. No new router instance, public
listener or expanded media allowlist was introduced.

Manual recovery: check the Home-Pi router service, each private tunnel, the
Neo source publisher, then the two independent subscriber services. Inspect
the router registration log and renderer screenshots. Pause the publisher
to verify retained panels become stale after seven seconds, then resume it
and verify they recover. TV selection remains authoritative and video stays
on its existing private media path. See Canvas `docs/firetv-tardy.md` for
installed launch-agent names and presentation ownership.
