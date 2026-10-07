# Single AirPlay mirror to Canvas

`unibus-cast-mirror CONFIG.json` supervises a separate UxPlay process and a fixed
GStreamer graph. This experimental runtime does not replace Shairport Sync,
implement Google Cast, publish session contracts, or drive Canvas workspaces.

```json
{
  "listen": "192.168.2.74:8099",
  "allowed_clients": ["192.168.2.97"],
  "receiver_name": "fungOS Canvas",
  "receiver_device_id": "02:46:50:4c:74:01",
  "receiver_port": 35100,
  "video_rtp_port": 35200,
  "audio_rtp_port": 35201
}
```

Host dependencies: UxPlay 1.73+ with `-vrtp`/`-artp`, `gst-launch-1.0` and
`rtph264depay`, `h264parse`, `rtpL16depay`, `audioconvert`, `voaacenc`,
`aacparse`, `mpegtsmux`, RTP jitter buffers and queues. The helper does not
download/install dependencies or open firewall rules.

UxPlay listens on TCP/UDP 35100–35102 and advertises locally using its normal
mDNS backend. Forwarded RTP binds loopback only; HTTP binds the configured
specific private LAN address and accepts only explicit viewer IPs. Host firewall
must independently LAN-scope AirPlay/mDNS ports: UxPlay does not use the HTTP
client allowlist to authenticate senders.

New devices require a random PIN (`-pin`); `-reg` retains paired clients in the
receiver owner's `~/.uxplay.register`, so returning devices need not pair again.
An explicit stable receiver device ID and `-key` persistent private key preserve
receiver identity across restarts. Keep the registration and key owner-private.
UxPlay displays the PIN in owner process output; relay it privately to the sender.
The helper ignores implicit UxPlay startup files with `-rc /dev/null`, avoids
static password argv/config, and does not expose PINs over HTTP.

Start the helper under a supervisor with **whole-service process-group cleanup**
(systemd `KillMode=control-group`), then initiate sender screen mirroring. Open
`http://192.168.2.74:8099/live.ts` as an authorized Canvas stream surface.
The requested sender display is 1920×1080 at 30 fps. Mux output is H.264 passthrough plus AAC-encoded stereo audio. Audio RTP is
L16 at 44.1 kHz, not the existing Isochrone 48 kHz/L24 baseline. No GPU video
decode/reencode occurs on this host. Only H.264 screen mirroring is qualified by
this initial graph; H.265 and YouTube native HLS are not handled.

HTTP uses at most four worker/viewer connections and 32 × 1316-byte queued chunks
per viewer, plus a bounded 2 MiB video startup replay. First media bytes wait for
PAT/PMT and a video random-access frame, preventing audio-only player detection.
Late viewers atomically replay the latest complete GOP before live bytes. When
the GOP exceeds the cap, new viewers wait for the next random-access frame.
Lagging viewers disconnect instead of losing arbitrary TS packets;
clients need a new connection to recover. Headers and socket writes time out,
and idle streams wait for a sender without closing. Immediate first frame is not guaranteed.
No media archive or unbounded replay buffer exists.

The MPEG-TS mux currently pads to an 8 Mbit/s baseline to fill stock Android's
byte-based HTTP startup cache promptly on static desktops. Null packets are
excluded from the GOP replay budget. This is a compatibility workaround, not
extra encoded image quality; a bounded Android live-player cache should replace
the padding when that client is updated.

Independent RTP branches use arrival-clock timestamps/jitter buffers; this is
**not verified A/V synchronization**. TV-local decode handles normal muxed
playback, but measure actual skew on hardware. The existing Isochrone split-path
and Holodeck H.264 renderer are separate followups. A live silent audiomixer input
prevents missing sender audio from starving the mux. Sender reconnect lifecycle
and actual playback still need hardware testing; do not claim them ready.

If either child exits or media stdout fails, the helper exits loudly and kills
its remaining child. It does not automatically restart/rearm sessions. External
signals require the supervisor's process-group cleanup described above.
