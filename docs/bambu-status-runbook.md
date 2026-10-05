# Read-only Bambu status over Unibus

The nearby `unibus-bambu` process owns Bambu MQTT and secrets. Mycelium supplies
optional discovery evidence, never authorization. Unibus carries the narrow
`printer.status.v1` contract from `unibus-device::printer`; Canvas consumes that
contract through a subscribe-only observer and renders it with Lua primitives.
No hard dependency on Mycelium; explicit enrollment works without it.

## What ships

- Exact `device/<serial>/report` subscription, MQTT/TLS, username `bblp`.
- No publish API, `/request` subscriptions, push-all requests, heaters, movement,
  pause/resume, file transfer, or camera access. Firmware compatibility still
  needs verification on the actual printers.
- Transactional bounded delta reduction: progress, job/state, temperatures and
  targets, layers, remaining minutes, speed, Wi-Fi. Missing values remain unknown.
- Each field retains its own measurement timestamp. Retained messages and MQTT
  duplicate deliveries do not renew live status. Command replies are not data.
- One addressed snapshot/second, six-second lease, sixty-second report/sample
  freshness. Silence becomes stale; supervisor-owned restart follows disconnect.
- Exact independently enrolled leaf-certificate SHA-256 pin; TLS handshake
  signatures remain verified. Pinning replaces CA/hostname validation, including
  certificate expiry/revocation checks. This is deliberate for printer identity
  over an IP endpoint; rotate pins explicitly, never automatically trust a new
  certificate. This is **not** trust-on-first-use or an insecure TLS fallback.

## Enrollment: all three lab printers

1. Record each printer's serial, IP, model/firmware, and LAN access code from its
   network settings. Usually the LAN code has eight characters; it is not the
   cloud pairing PIN. Existing Bambu Studio enrollment may already hold it.
2. Choose one nearby lab host reachable by all three printers. Run one adapter
   instance per printer, each with a distinct authenticated Unibus producer.
3. Store each code in its own absolute-path regular file with mode `0600` (or
   stricter), never Git, topology advertisements, UI payloads, or command logs.
4. Independently verify each printer's TLS identity using a trusted existing
   enrollment or on-site trusted-network verification. SHA-256 hashes the DER
   leaf certificate. Merely downloading a certificate from a discovered IP is
   not independent verification. No fingerprint automation is shipped here.
5. Copy `config/bambu-status.example.json` three times. Replace every placeholder;
   the example IP is documentation-only and the invalid pin deliberately fails.
6. Add router grants for producers `lab-printer-1..3`: capability
   `printer.status`, `publishes: ["printer.status.v1"]`,
   `publish_producers: ["canvas-printers"]`, and the enrolled `lab` zone.
   Register `canvas-printers` as a non-publishing receiver. Add the explicit route
   `{"kind":"printer.status.v1","target":{"mode":"producer"}}`.
   Use separate private node tokens; do not broaden grants to arbitrary MQTT.
7. Build on Agora/native Linux (cross-build before deploying to Pi):
   `cargo check -p unibus-bambu`, then
   `cargo test -p unibus-bambu -p unibus-mqtt -p unibus-device`, then
   `cargo build --release -p unibus-bambu` when an artifact is needed.
8. Run `unibus-bambu /absolute/path/printer-1.json` (and 2/3) under a supervisor
   with restart backoff. Remote hubs must use an authenticated loopback tunnel;
   the adapter rejects direct remote bus endpoints.
9. Optional discovery: `unibus-bambu discover` reads current Mycelium
   advertisements and expires old evidence. It does not scan, enroll, connect,
   or obtain credentials. An empty list is not proof that printers are absent.

## Put it on Canvas

In the adjacent Dock repository:

1. Install `config/modules/printer-status-v1.{json,lua}` in Canvas's configured
   runtime directory under `modules/`. The Lua module reads typed inputs/theme
   and emits panel/badge/metric/label primitives—no networking or credentials.
2. Copy `config/workspaces/tv/printers.json` into the target Canvas workspace
   catalog using its existing workspace import workflow. Replace the three
   binding stream IDs with `printer-` plus the first 32 lowercase hexadecimal
   characters of SHA-256(`bambu:<serial>`). This derived ID fits Canvas's bounded
   slug vocabulary without exposing credentials or creating case collisions.
   For example, hash the exact device ID with `printf %s 'bambu:SERIAL' | shasum -a 256`.
   Register a `printers` project in the target catalog before importing. The initial
   cards intentionally show awaiting enrollment and no invented telemetry.
3. Copy `config/deploy/printer-observer.example.json`, replace paths/tokens and
   exact device-to-producer mappings. Build `canvas-printer-observer` with
   `--features lua-scenes` on Agora, and run it beside Canvas.
4. The observer consumes authenticated Unibus SDK events, checks enrolled
   producer + serial/device identity, read-only schema, ranges and snapshot
   expiry, then sends bounded local `canvas.widget.snapshot.v1` envelopes to
   Canvas. Configure the hub/tunnel so all three producer streams reach it.
5. Select the `printers` workspace with the target's existing `canvasctl`
   workspace command. Do not edit another node's local catalog by accident.

Multiple displays can have separate observer producer IDs: configure explicit
fanout routes/grants for those destinations rather than assuming the addressed
first-slice configuration broadcasts to every Canvas.

## Verification and limitations

Tests cover real TLS success/mismatched pins, malformed/retained/duplicate
reports, delta freshness, job changes, private file permissions, stale snapshots,
observer source identity and Lua's no-controls/expired-field behavior.
No live printer was enrolled during development: all three still need codes
and identity verification. Do not claim this is a live dashboard until real
reports arrive. Firmware/auth modes can differ; investigate failures visibly,
never disable certificate verification or brute-force codes.

The shared development checkout also has unrelated untracked baseline workspace
members (`unibus-fix`, `service_adoption.rs`); a fresh checkout may need those
owners' pending work before workspace-wide gates pass. This slice does not
silently include or take ownership of that unrelated implementation.

Protocol references: [OpenBambuAPI MQTT](https://github.com/Doridian/OpenBambuAPI/blob/main/mqtt.md),
[Bambu firmware authorization](https://blog.bambulab.com/firmware-update-introducing-new-authorization-control-system-2/).
