# Reel media over Unibus

Status: transport and Canvas integration implemented; live rollout pending.

`unibus-video` carries compressed MPEG-TS bytes through existing leased stream
fan-out, separately from presentation JSON. It does not add discovery ownership
or require video transcoding. FFmpeg copies H.264 video and normalizes audio to
AAC. Each consumer uses its own authenticated client identity and bounded router
queue.

## Contract

`media.mpegts.chunk.v1` uses a `unibus.stream.event.v1` envelope with binary
payload. Its JSON value binds `generation`, `width`, `height`, and `codec`
(`h264`). Payloads are at most 6016 bytes, aligned to 188-byte TS packets.
Sequence gaps, replay, expired data, or changed geometry terminate the receiver;
the supervisor must restart its decoder instead of retaining damaged state.

## Manual operation

1. Provision separate authorized publisher and receiver client configurations.
   Publisher needs stream-source capability; receivers publish stream interests.
2. Atomically place `current.mp4` in an owner-controlled source directory, then
   replace `current.json` with a matching manifest:
   `{"generation":"123","expires_at_unix_ms":1790000000000}`.
3. Run `unibus-video publish CLIENT.json SOURCE_DIR canvas/tv/tardy/video`.
4. Run `unibus-video receive CLIENT.json canvas/tv/tardy/video PRODUCER GENERATION`.
   Output starts with one JSON geometry header, followed by MPEG-TS bytes.
   `--raw` omits the header for an FFmpeg HLS receiver.
5. Canvas mirror bindings select the stream per surface. The decoder accepts
   `canvas-tv SOURCE_ID - WIDTH HEIGHT`; `canvas-tv describe WIDTH HEIGHT`
   derives the fitted geometry used for decoder output and frame allocation.

The FireTV may retain its native local HLS interface, fed by a nearby Unibus
receiver. Keep that HTTP endpoint private and restrict clients; do not place
Unibus credentials in URLs or the served directory. This is independent
playback of the same generation, not frame-locked multi-screen synchronization.

Do not retire existing delivery until late joins, generation transitions,
receiver restart, and sustained playback are verified on all target screens.
