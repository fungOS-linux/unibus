# Cast session boundary

`unibus-cast` is a typed control-plane library, **not an operational AirPlay or
Chromecast receiver**. No protocol listener, pairing implementation, publisher,
FireTV integration or Isochrone audio forwarding is enabled by this slice.
`Backend::GoogleCast` identifies future sessions; it does not claim compatibility.

## Ownership

Reuse the existing AirPlay audio receiver instead of implementing RAOP again.
The adjacent Isochrone deployment references `shairport-sync.service`; Unibus
currently has no AirPlay receiver. The deployed receiver must be inspected before
assuming which PCM/control hooks it exposes. Evaluate an established mirroring
backend separately, preserving H.264 and using FireTV hardware decode when possible.

Protocol backends own pairing, admission and protocol-local discovery (e.g. mDNS).
Mycelium owns fleet capability discovery, not authorization or protocol forwarding.
Unibus carries `media.cast.session.v1` and revision-bound
`media.cast.command.v1`; existing `unibus-media` stream descriptions/endpoints
carry media-plane information. Canvas presents sessions as ordinary tiles.
No credentials, arbitrary GStreamer pipelines or media payloads belong in a
session snapshot. Route references must resolve through an authorized local
catalog; never interpret them as URLs or executable instructions.

Snapshots contain a generation, revision, expiring lease, phase, mute state and
optional audio/video stream IDs and Isochrone route. Commands support stop, mute
and routing. Session validation is deliberately **not authentication**, request
deduplication or execution. A future adapter must authorize each route/control,
atomically compare revisions, persist bounded request-ID deduplication, return
explicit acknowledgements and apply actual backend capability checks. A backend
must not advertise capabilities it cannot perform. Google Cast reception requires
its own feasibility assessment; a LAN advertisement alone is not a Cast receiver.

## Split FireTV video / Isochrone speakers

The intended media paths are source → compressed video → FireTV Canvas and
source → audio → Isochrone → selected speakers. The optional `source_clock`
reuses `stream-descriptors::ClockCorrelationV1`; clock owners calculate it.
Its presence **does not prove lip sync**. Independent buffer-fill/ASRC control
can stabilize audio without aligning its presentation time with FireTV video.

Before advertising synchronized playback, obtain all of:

- A shared producer timeline for audio and video, with generations and bounded
  correlation uncertainty. Source restarts invalidate previous correlations.
- Timestamped actual or qualified presentation observations at both endpoints,
  including decoder/audio-device latency and observation uncertainty.
- Bounded playout queues and an adjustable presentation deadline or delay at
  the audio/video sink; define which endpoint is the presentation master.
- A fresh measured skew plus total uncertainty against an explicit acceptance
  budget, checked across restarts, seeks, loss, clock drift and route changes.

Until those exist, report split playback as **sync unverified**, not synchronized.
Missing/stale measurements must retain that degraded state. Do not invent playout
observations from packet arrival or buffer occupancy. Start with TV-local audio
as the predictable fallback; selecting external speakers should be explicit,
never silently duplicate audio. Isochrone owns RTP/ASRC/loss handling; this crate
does not introduce another clock servo.

## Next runnable slice / manual runbook

1. Read-only inventory of the actual AirPlay audio receiver and FireTV Canvas
   control/media endpoints. Do not restart active playback.
2. Verify supported audio extraction, source timestamps and mirroring backend
   licensing/installation. Keep receiver ports LAN-scoped and pairing enabled.
3. Implement an authenticated session publisher and backend command executor
   using this contract and existing stream projections. Add acknowledgements,
   replay handling and atomic revision tests before enabling remote controls.
4. Feed a local test mirror into an ordinary Canvas tile with TV-local audio;
   measure decode latency, drops and queue age, not merely nominal frame rate.
5. Add explicit Isochrone routing only after its PCM ingress and latency controls
   are verified. Measure split-path skew; keep the unverified indicator until
   presentation evidence meets the selected budget.

No deployments or active services were changed by the contract-only slice.
