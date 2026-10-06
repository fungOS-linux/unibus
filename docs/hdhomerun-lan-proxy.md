# HDHomeRun private LAN republication

`unibus-hdhomerun` has two roles: a device-side **owner** and a client-LAN
**proxy**. HDHomeRun owns tuning, broadcast decoding, and its physical tuner
locks. The adapter owns the private transport and local protocol projection.
Mycelium can supply placement and reachability evidence; neither role links
Mycelium or changes routes. No generic USB, multicast, or LAN bridge is needed.

## Current placement

* Physical device: CONNECT DUO, `10626E55`, `192.168.0.208`, two tuners.
* Owner: Avery, private overlay `100.85.154.96:18504`.
* Proxy: home-pi, `192.168.2.74`, permitted clients `192.168.2.0/24`.
* Temporary packet relay: Agora, `192.168.2.67:18504`. Home-pi's overlay agent
  needs login, so this deployment-only relay forwards encrypted QUIC packets.
  It accepts only home-pi's source IP, with at most two relay children.

The radio Pi is not needed. Moving the proxy means changing explicit LAN
placement, not changing the HDHomeRun contract or coupling it to Radioman.

## Supported endpoints

The local discovery responder listens on **UDP 65001**. It supports wildcard,
targeted DeviceID, and multi-device-type requests, checks framing and CRC, and
returns the real DeviceID, tuner count, and reachable local BaseURL/LineupURL.
Responses are rate-limited and restricted to the configured client CIDR.
Broadcast reception requires a wildcard socket, but HTTP listeners bind only
the configured LAN IP. No requests or broadcasts are forwarded to other LANs.

At `http://192.168.2.74:8088`:

* `/discover.json`: projected device information.
* `/lineup.json`, `/lineup.xml`, `/lineup.m3u`: rewritten channel URLs.
* `/lineup_status.json`: read-only physical scan status; does not start a scan.
* `/healthz`: ready/degraded state and catalog age, no credentials.

Playback: `http://192.168.2.74:5004/auto/v4.1` (KOMO). Explicit
`/tuner0/v4.1` and `/tuner1/v4.1` are supported. The only stream query option is
`duration=SECONDS`, from 1 to 86400. GET and HEAD are accepted; HEAD does not
allocate a tuner. Unsupported methods, path injection, and arbitrary targets
are rejected. This is not an open HTTP proxy.

Protected channels are omitted and rejected; no DRM bypass, transcoding,
device firmware updates, TCP 65001 get/set, DLNA/SSDP emulation, DVR/storage,
channel rescan, or favorites mutation is implemented. **Stock FireTV app
compatibility remains unverified until tested on the FireTV.** Clients needing
legacy control or assuming HTTP port 80 may need further compatibility work.
VLC can open `/lineup.m3u` without relying on discovery.

## Narrow private contract and resource limits

Reuses `unibus-quic` credential loading, mTLS configurations, endpoints, and
certificate binding. Continuous MPEG-TS travels over a reliable QUIC
bidirectional stream, not topic events or MQTT. One stream carries one bounded
request header, a bounded reply header, then raw response bytes through EOF.
Headers use a big-endian u32 byte length, limited to 4096 bytes.

Request contract: `unibus.hdhomerun.request.v1`:

```json
{"contract":"unibus.hdhomerun.request.v1","operation":{"operation":"stream","channel":"4.1","tuner":null,"duration":10}}
```

Other operations are `catalog` and `lineup_status`. No arbitrary URL, control
command, or credentials are accepted in the request. Reply fields are HTTP
`status`, `content_type`, and optional `tuner_error`. Metadata is limited to
1 MiB, at most 1024 channels and 16 physical tuners. Owner concurrency is
bounded to eight connections, 32 requests, and the physical tuner count;
the device also enforces locks shared with other applications.

TLS trust **and explicit certificate allowlists on both ends** are required.
Agora cannot impersonate either peer. The owner binds only an explicit private
address; it only fetches the configured tuner IP/ports, without redirects or
environment proxy configuration. No public listener is needed.

One upstream chunk is written at a time. QUIC send/receive windows bound queued
media; stalled writes and reads time out. Dropping a client HTTP response sends
STOP_SENDING, cancels upstream HTTP, and releases the adapter's tuner permit.
Different viewers use separate tuner sessions; same-channel fanout is **not**
implemented. A third concurrent session receives 503 with HDHomeRun error 805.
Physical tuner failures are propagated rather than hidden.

The proxy probes the owner before advertising. Catalogs refresh every 30
seconds and expire after 75 seconds; expired catalogs stop discovery replies
and return degraded health instead of advertising an unreachable device.
The last snapshot is retained internally, not silently replaced with emptiness.

`DeviceAuth` is never logged or placed in a public descriptor. The proxy
defaults to withholding it. The example explicitly enables `share_device_auth`
for the trusted TV LAN because guide/DVR-related clients may use the device's
native LAN credential. Treat that entire configured client LAN as trusted.

## Build and manual runbook

1. Confirm physical `/discover.json` and `/lineup.json` from the owner host;
   redact `DeviceAuth` from any diagnostic output.
2. Build on Agora: `cargo check -p unibus-hdhomerun`, then
   `cargo test -p unibus-hdhomerun`, then produce Linux release artifacts.
   The Pi build target is `aarch64-unknown-linux-gnu`. For older x86 glibc hosts,
   use the static `x86_64-unknown-linux-musl` target rather than Agora's glibc.
3. Supply deployment-owned CA/certificate/key material with owner certificate
   SAN `hdhomerun-owner.internal`; keep private keys mode 0600 in mode 0700
   directories. A dedicated deployment CA or explicitly trusted peer
   certificates are valid providers; the adapter does not issue fleet identity.
4. Calculate each certificate binding:
   `openssl x509 -in peer.pem -outform DER | openssl dgst -sha256`.
   Put the **other** peer's binding in each allowlist with a `sha256:` prefix.
5. Adapt `config/hdhomerun-{owner,proxy}.example.json`. Keep the proxy TLS name
   set to the **owner** certificate's SAN even when connecting via a relay.
6. Install `deploy/systemd/unibus-hdhomerun-owner.service` on Avery; its
   LoadCredential directives privately expose the TLS material to DynamicUser.
   Configure and enable the Agora relay unit if needed. No route changes.
7. Install the proxy binary in `~/.local/bin`, config/identity in
   `~/.config/unibus-hdhomerun`, and proxy **user** unit in
   `~/.config/systemd/user`. Run `loginctl enable-linger USER` for reboot/logout
   persistence. Validate both configs with `unibus-hdhomerun CONFIG --check`.
8. Run `systemctl daemon-reload; systemctl enable --now UNIT` for the two system
   services; use `systemctl --user daemon-reload` and
   `systemctl --user enable --now unibus-hdhomerun-proxy` for home-pi.
9. From the TV LAN verify `/healthz`, `/discover.json`, all lineup URLs, and a
   short GET stream. Check MPEG-TS sync/packet cadence and release after close.
   Then launch the FireTV HDHomeRun app. Do not mark app compatibility verified
   merely because HTTP or synthetic discovery worked.

Logs: `journalctl -u unibus-hdhomerun-owner`, `journalctl -u
unibus-hdhomerun-relay`, and `journalctl --user -u unibus-hdhomerun-proxy`.
Rollback: stop/disable these three dedicated units. No existing desktop, radio,
router, DHCP, or Tailscale configuration is replaced. Once home-pi has a healthy
private route, point `owner` directly at Avery and disable the relay.

## Verification

Unit and real-socket tests cover protocol corruption/truncation, modern omitted
wildcard IDs, variable-length TLVs, safe metadata rewriting, identity checks,
DRM exclusion, real mTLS QUIC, UDP discovery, HTTP playlist/playback, two-tuner
busy responses, cancellation, LAN denial, certificate pin rejection, and stale
catalog withdrawal. The fixture's MPEG-TS packets are synthetic: these tests do
not substitute for checking real device decoding or the FireTV UI.

Protocol references: [official HTTP API](https://info.hdhomerun.com/info/http_api)
and [official discovery framing](https://github.com/Silicondust/libhdhomerun/blob/master/hdhomerun_pkt.h).
