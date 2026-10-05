# USB ADB over Unibus

Plug an Android device into Studio, enable USB debugging, and approve **Studio's**
ADB key on the device. On Neo:

```sh
adb -H 127.0.0.1 -P 5038 devices -l
adb -H 127.0.0.1 -P 5038 -s SERIAL shell
adb -H 127.0.0.1 -P 5038 -s SERIAL pull /sdcard/example.txt
unibus-adb inventory 127.0.0.1:5038
unibus-adb discover
```

Keep `-H` and `-P` explicit so local USB ADB on Neo is unaffected. Alternatively,
set `ADB_SERVER_SOCKET=tcp:127.0.0.1:5038` for a single command/process environment.
This is an ADB-server bridge, **not generic USB-over-IP**. Shell, exec-out, device
listing, push/pull and install retain normal ADB protocol semantics.
Inventory follows the official [ADB host-service protocol](https://android.googlesource.com/platform/packages/modules/adb/+/4dd4da41e6b004fa0d49575ac87e8db877f3a116/SERVICES.TXT).

## Ownership and trust

```text
Neo adb → loopback :5038 → unibus-adb attach
  → Unibus frames → encrypted SSH tunnel selected by Mycelium
  → Studio's isolated loopback Unibus hub :18793
  → unibus-adb serve → Studio ADB :5037 → USB phone
```

Neo's tunnel listens on `127.0.0.1:19893`. No ADB or Unibus listener is opened on
the LAN. ADB owns USB, Android authentication and hotplug. Unibus owns framing,
producer grants and bounded sessions. Mycelium optionally provides discovered
advertisements and a public SSH plan, including the persisted Unix identity,
certificate and any jump host. Static authenticated tunnels can replace it;
neither USB ownership nor the protocol implementation depends on Mycelium.

An approved Unibus client receives **full Studio ADB-server privileges**, not
read-only access and not a per-phone sandbox. It can access any device attached
to that server, including network devices if the operator connected them, and
issue server-management commands. Give tokens only to trusted operators. The
server separately checks an explicit `allowed_clients` list; discovery never
grants permission. Tokens stay outside the repository in a private directory.

`adb forward` creates a listener on **Studio**, because that is where the ADB
server runs. Those extra sockets are not automatically tunneled to Neo. This
slice does not promise transparent local scrcpy/forwarded-port behavior; run the
capture on Studio or explicitly tunnel the required port. Existing Canvas Android
capture is unaffected.

## Wire contracts and bounds

`adb.socket.v1` carries an addressed JSON header and an opaque binary payload:
`session`, `operation` (`open`, `data`, `ack`, `eof`, `close`), `sequence`.
Session keys include the authenticated producer. Data/EOF are strictly ordered;
gaps, duplicates and data after EOF terminate the session. ACKs follow completed
TCP writes and cannot acknowledge unsent bytes. No automatic command replay.

- 16 simultaneous sessions per adapter; 16 KiB chunks.
- 16 unacknowledged data frames per direction; 32 queued inbound frames/session.
- Half-close preserved independently in both directions.
- 30-second stalled-credit timeout, 5-second TCP write timeout, 300-second idle
  expiry, 10-second envelope freshness.
- Broken bus connections exit visibly; launchd reconnects the adapter. Existing
  ADB sessions fail and must be reissued rather than replayed.

`adb.devices.v1` is a full inventory snapshot every two seconds. It includes
schema version, authenticated producer, generation, observed/expiry timestamps,
`available`/`unavailable` state and bounded serial/state/USB/model/product facts.
The six-second lease expires crashed observers; an available empty list means
no devices, while unavailable means the ADB backend could not be queried.
Unauthorized/offline devices remain explicitly distinguishable. A network ADB
device is never mislabeled USB. Consumers must replace the snapshot, not append
its devices. Repeated snapshots don't flood the attach log; changes are logged.

Discovery publishes an actual `_ssh._tcp` endpoint at port 22, with TXT metadata
`purpose=unibus-adb`, `access=ssh`, `contract=adb.socket.v1`,
`hub=127.0.0.1:18793`, `producer=studio-adb`. It does **not** advertise the private
ADB port as remotely reachable. `discover` reads Mycelium's public topology
advertisements and verifies the exact contract and TTL, rather than fuzzy service
names. The current generic service projection on installed daemons drops useful
TXT metadata, so the advertisement remains the discovery source of truth.

## Manual setup / recovery

1. Build and install `unibus-adb` on both Macs. This crate uses the shared Unibus
   client and router; it does not introduce another router implementation.
2. On Studio, run `unibus-adb init /Users/ajmwagar/.config/unibus/adb-studio`.
   The directory must not exist; initialization never overwrites existing keys.
   This creates a private directory, independent random per-producer tokens,
   an isolated two-node router, and serve/attach configurations.
3. Securely copy only `attach.json` and `neo-adb.token` to the matching private
   directory on Neo. Change paths/producer grants explicitly for another account
   or consumer. Never reuse a producer identity for two concurrent clients.
4. Install the repository's `deploy/launchd/dev.fpl.unibus-adb-*.plist` templates:
   `hub`, `serve`, `advertise` on Studio; `tunnel`, `attach` on Neo. These templates
   currently use `/Users/ajmwagar` and the managed target
   `darwin-averys-mac-studio`. Update paths/target for another deployment.
5. Bootstrap those agents with `launchctl bootstrap gui/501 PATH_TO_PLIST`.
   Use the real `id -u` for other users. These are user-login agents, not pre-login
   system daemons; RunAtLoad/KeepAlive restart them after login or a crash.
6. Check device listing and a read-only command:
   `adb -H 127.0.0.1 -P 5038 -s SERIAL shell getprop ro.product.model`.

Manual equivalents, each in its own terminal:

```sh
# Studio
unibus-adb hub ~/.config/unibus/adb-studio/router.json
unibus-adb ~/.config/unibus/adb-studio/serve.json
# Neo; MYCELIUM_BIN can be an absolute installed CLI path
unibus-adb tunnel darwin-averys-mac-studio
unibus-adb ~/.config/unibus/adb-studio/attach.json
```

Logs live in the private configuration directory. Use `launchctl kickstart -k
gui/501/dev.fpl.unibus-adb-attach` (or the specific failing job) for recovery.
`mycelium scan` refreshes managed observations when discovery is stale; failed
discovery does not stop an already-authorized static connection. An unauthorized
phone needs its USB-debugging approval prompt, not a transport workaround.

Rollback uses `launchctl bootout gui/501/dev.fpl.unibus-adb-JOB` for these five
specific jobs. Existing Canvas/audio/MIDI services and local Neo ADB remain
untouched. No router restart or generic USB driver installation is required.

## Verification

```sh
cargo check -p unibus-adb
cargo test -p unibus-adb
cargo clippy -p unibus-adb --all-targets --no-deps -- -D warnings
```

Tests cover wire bounds/order/half-close, credit validation, inventory identity
and expiry, exact discovery metadata/TTL, and an actual hub with both bridge
processes transferring 4 MiB while rejecting an unapproved client. Full dependency
clippy currently finds an existing `collapsible_match` in the shared router;
adapter-only strict clippy is clean.
