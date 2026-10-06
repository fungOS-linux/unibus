# Private, switchable HDHomeRun HLS projection

`unibus-hdhomerun-hls CONFIG.json` owns one FFmpeg/NVENC projection, not the
physical tuner inventory or discovery. It reads the existing adapter's real
`/lineup.json` and reuses its protected-channel and channel-number policy.
The stock HDHomeRun discovery/proxy remains unchanged.

Use `config/hdhomerun-hls-agora.example.json` as the deployment example.
Create its dedicated projection directory beneath the existing HLS HTTP root.
Run the binary on Agora; keep the existing allowlisted static HLS server.

- `GET /v1/channels`: `unibus.hdhomerun.hls.v1`, with channel/name choices.
- `POST /v1/tune`, JSON `{"channel":"5.1"}`: validates against the live lineup,
  reaps the previous owned encoder, and returns channel/name/playback URL only
  after the first HLS segment is ready. Concurrent tuning returns a conflict.
- Unknown/protected channels cannot start a producer. Callers cannot supply
  an input URL, executable, codec options, or tuner index.
- The stable `current/live.m3u8` path survives workspace changes. Playback URLs
  include a generation query to force decoder reconnection; segment filenames
  also contain that generation so caches cannot cross channels.
- Only obsolete directories created during this service run are removed.
  Crash leftovers are not automatically deleted. HLS retention is bounded per
  running producer, and there is at most one producer owned by this service.

The development deployment uses a private literal bind address and explicit
client IP allowlist. This is **not cryptographic pairing**; never expose it
publicly. There is no program guide, DVR, favorites mutation, tuner rescan,
multi-viewer isolation, or automatic playback lease yet. One shared projection
means all consumers of that projection see the selected channel.

## Manual verification

On Agora, fetch `/v1/channels` from the allowed LAN address. POST a listed
channel, fetch the returned playlist, and confirm it advances. POST a different
listed channel and verify the old child was reaped and only one encoder remains.
An unknown channel must fail without replacing the existing encoder.

The current development user service is `canvas-tv-tuner-hls.service`.
It replaced the fixed `canvas-tv-komo4-hls.service`, but still uses
`canvas-tv-hls-http-omni.service`. These transient units are not reboot-persistent.

## Canvas TV presentation

The remote's **Channels** control replaces the active TV tile with a themed
channel browser, not a dialog or separate workspace. Up/Down selects a channel;
Select tunes it; Back returns to the current playback. Other tiles remain visible.
Audio stays local to the TV and the surface mute setting survives tuning.

Configure the control origin with the app's `tuner_endpoint` intent extra.
It is retained in app-private preferences. Rust owns HTTP admission and bounded
responses; Java owns the platform view. The independently built
`android-tuner-control` feature is packaged as `libcanvas_tuner.so` alongside
the renderer library and uses the renderer's existing authenticated loopback
Canvas control contract. It never maintains a second workspace catalog.

Channel application checks workspace/surface provenance again after tuning and
uses revision-bound patches. If the workspace changed, the error is explicit;
the global tuner may already have switched, but another workspace is not edited.
The installed HDHomeRun app's guide inspired navigation only; no proprietary
assets, guide data, or APK code are copied.
