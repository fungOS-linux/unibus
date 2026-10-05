# Modular stream facets

Status: architecture proposal for `stream.descriptor.v2`; no wire behavior is
changed by this document.

Unibus needs one discoverable stream model that works for cameras, audio,
radio spectra, and sensors without pretending those payloads are the same.
The common part is a stable identity and lifecycle. Typed facets describe the
domain, encoding, timing, and available transports independently.

This distinction is load-bearing:

- H.264, H.265, L16, L24, and PCM sample formats are **encodings**.
- RTP, RTSP, SRT, QUIC, UDP, and Unibus messages are **transport or session
  bindings**.
- Isochrone is a **transport/session implementation** that owns RTP framing,
  L16/L24 conversion, timestamp-addressed playout, loss reporting, and audio
  clock-drift correction.
- GStreamer is a trusted local **runtime adapter** that can connect validated
  encodings and transports. It is not a protocol and descriptors never carry
  pipeline strings.

## Ownership and layers

Unibus L1 continues to route opaque envelopes and apply grants, TTL, and QoS.
The stream contract is L2 and belongs in `stream-descriptors`. Domain crates
own their payloads: `unibus-media` owns media events, Radioman owns spectrum
and IQ, and an observation contract owns sensor meaning. Adapters consume the
shared descriptor; they do not redefine it.

```text
domain producer
    -> stable stream descriptor + expiring endpoint lease
    -> authorized transport/session adapter
    -> domain consumer or projection
```

The stable descriptor and endpoint lease remain separate documents. A camera
is still the same stream when its RTSP address changes. A dead endpoint
expires without deleting semantic identity or falsely reporting an empty
sensor value.

## Proposed envelope

The exact Rust types and field bounds require implementation review. The
contract shape is:

```json
{
  "schema_version": 2,
  "stream_id": "camera/front-door/live",
  "source_resource": "camera/front-door",
  "semantic": { "kind": "video.camera", "zone": "porch" },
  "payload": { "contract": "media.video.frame.v1" },
  "encoding": { "kind": "h264", "profile": "high", "level": "4.1" },
  "clock": { "kind": "media", "rate_hz": 90000 },
  "media": { "width": 1920, "height": 1080, "frame_rate_millihz": 30000 },
  "delivery": { "class": "stream", "maximum_age_ms": 1000 },
  "required_consumer_capabilities": ["media.video.decode.h264"],
  "provenance": { "producer": "frigate", "transform": "camera-original" }
}
```

Every descriptor has these shared properties:

- `stream_id` is stable and globally unique within an installation.
- `source_resource` identifies the physical or logical origin independently
  of a particular stream generation.
- `semantic.kind` selects one bounded, typed domain facet.
- `payload.contract` names the schema used by in-band messages, if any.
- `encoding` describes bytes, never how to execute a decoder.
- `clock` names the timestamp domain and rate when the payload has one.
- `delivery` states command, latency-first stream, or latest-value snapshot
  semantics and its freshness bound.
- required capabilities are exact names, not free-form negotiation scripts.
- provenance identifies producers, calibration, and transforms by bounded
  references; it does not contain secrets.

Only applicable facets are present. A temperature observation has units and
aggregation semantics but no video geometry. H.264 video has geometry and a
media clock but no physical measurement unit. Validation rejects
contradictory combinations rather than guessing.

## Typed domain facets

### Media

A media facet describes bounded geometry: dimensions and frame rate for video;
sample rate, channels, channel layout, and sample format for audio. Encoding
profiles are registered data with strict limits. `h264/high/4.1` and
`h265/main/5.0` are examples; arbitrary codec caps are not.

Container and elementary-stream details belong to the encoding or transport
binding that owns them. Consumers match exact supported combinations. A
decoder advertising H.264 support does not thereby claim RTSP, SRT, or a
90-kHz clock.

### Observations and sensors

An observation facet describes scalar, vector, histogram, state, or event
values. It includes physical quantity, UCUM-style unit identifier, value
shape, expected cadence, maximum age, and legal aggregations. It also states
whether a number is a gauge, monotonic counter, or distribution. Dimensions
and labels are bounded at descriptor validation time.

Expiry means **unknown**, never zero, false, or empty. Calibration, algorithm,
and transform provenance travel as references so a raw sensor and a derived
estimate cannot be confused.

RF streams use observation facets for derived quantities and domain contracts
for structured vectors. Radioman's bounded FFT row is a latency-first vector;
peak power, noise floor, frame count, and drop count are scalar observations.
Raw IQ remains on a dedicated UDP/QUIC data plane because ordinary bus routing
must not become an unbounded sample transport. ESP32 CSI follows the same rule:
raw ADR-018 frames are opt-in latency-first streams, while presence or vitals
estimates are expiring snapshots with explicit heuristic provenance.

## Clocks and timing

Clock description is independent of transport:

- a media clock has a rate and timestamp width/wrap behavior;
- a wall-clock observation has capture time and declared synchronization
  quality;
- a monotonic producer clock requires an explicit mapping before correlation;
- clock identity and synchronization method are descriptive facts, not proof
  of authorization.

Isochrone owns the behavior behind its advertised RTP clock: sequence and
timestamp parsing, L16/L24 network byte order, packet geometry, playout,
concealment, restart detection, ASRC control, and saturation telemetry. The
descriptor supplies validated inputs such as 48 kHz, two channels, L24, and 48
frames per packet. It must not duplicate Isochrone's algorithms.

## Endpoint leases and session bindings

An endpoint announcement is an expiring set of alternatives for a stable
`stream_id`. Each binding declares:

- transport/session kind and a versioned profile;
- an opaque, provider-owned `endpoint_reference`;
- connection role (`listener`, `connector`, or `connectionless`) where the
  protocol requires one;
- required adapter capability;
- observed and expiry times;
- provenance and, where needed, the stream generation.

Connection roles are explicit. A TCP-like binding cannot leave both peers
guessing who listens. A datagram binding cannot claim listener semantics that
change its authorization.

Endpoint references are resolved locally by an authorized adapter. They are
not shell commands, GStreamer launch strings, bearer tokens, URL userinfo,
private keys, or inline credentials. The resolver may obtain a short-lived
credential from the local secret owner after grants and identity have been
validated. Discovery proves reachability, not authority.

## Deterministic capability matching

Selection is intersection, not preference inference. A candidate is usable
only when the consumer supports all descriptor requirements and one live
endpoint binding:

1. Validate the descriptor and unexpired endpoint lease independently.
2. Require matching stable stream identity and generation, if generation is
   present.
3. Match the semantic contract and payload schema.
4. Match encoding kind and every declared profile constraint.
5. Match clock, geometry, channel, and packetization limits.
6. Match transport profile and connection role.
7. Require the exact adapter and authorization capabilities.
8. Choose deterministically from remaining candidates using declared policy,
   never by executing producer data.

An unsupported field or ambiguous role is a rejection. Adapters report why a
candidate failed so discovery does not degrade into a silent blank surface.

## Runtimes and ecosystem adapters

Adapters project or realize a stream; none broadens producer authority.

- A GStreamer adapter maps validated facet combinations to compiled,
  allowlisted local graph constructors. The producer supplies data fields,
  never element names, properties, launch syntax, paths, or environment.
- An Isochrone adapter realizes RTP L16/L24 sessions and exports playout,
  loss, drift, and saturation observations.
- Canvas renders supported media, spectrum, and observation contracts.
- MQTT maps approved typed observations or events to explicit topics while
  retaining MQTT broker ownership of sessions and retained state.
- Prometheus/OpenMetrics exports bounded scalar gauges, counters, and
  histograms. It must not create one label or metric per FFT/CSI bin and must
  reject unbounded producer labels.
- MCP exposes agent-readable resources and bounded tools. It is not the
  high-rate data plane.
- DCP exposes state-derived actions legal now and binds execution to catalog
  and state revisions. It does not describe media bytes.
- Recorders and object stores subscribe through explicit capabilities and
  preserve descriptor/provenance snapshots with artifacts.

## Examples

These fragments illustrate facet composition, not a finalized v2 JSON schema.

### H.264 camera over RTSP

```json
{
  "stream_id": "camera/front-door/live",
  "semantic": { "kind": "video.camera", "zone": "porch" },
  "encoding": { "kind": "h264", "profile": "high", "level": "4.1" },
  "clock": { "kind": "media", "rate_hz": 90000 },
  "media": { "width": 1920, "height": 1080, "frame_rate_millihz": 15000 },
  "delivery": { "class": "stream", "maximum_age_ms": 1000 },
  "required_consumer_capabilities": ["media.video.decode.h264"]
}
```

```json
{
  "stream_id": "camera/front-door/live",
  "bindings": [{
    "transport": "rtsp",
    "profile": "rtsp-1.0",
    "role": "connector",
    "endpoint_reference": "frigate/front-door",
    "requires": "media.transport.rtsp"
  }],
  "observed_at_unix_ms": 1000,
  "valid_until_unix_ms": 31000
}
```

The same descriptor can instead receive an SRT binding without redefining
H.264. H.265 changes the encoding requirement, not the transport identity.

### Isochrone RTP L24

```json
{
  "stream_id": "audio/studio/program",
  "semantic": { "kind": "audio.program", "zone": "studio" },
  "encoding": { "kind": "l24", "byte_order": "big_endian" },
  "clock": { "kind": "rtp", "rate_hz": 48000 },
  "media": { "sample_rate_hz": 48000, "channels": 2, "frames_per_packet": 48 },
  "delivery": { "class": "stream", "maximum_age_ms": 40 },
  "required_consumer_capabilities": ["audio.session.isochrone.rtp-l24"]
}
```

Its lease uses an `rtp-udp` connectionless binding resolved by the Isochrone
adapter. L16 is another encoding profile handled by the same implementation.

### Radioman spectrum and health

```json
{
  "stream_id": "radio/rtl-sdr-1/spectrum",
  "semantic": { "kind": "rf.spectrum", "center_frequency_hz": 915000000, "span_hz": 2048000 },
  "payload": { "contract": "radioman.spectrum.v1", "shape": "vector", "maximum_elements": 4096 },
  "encoding": { "kind": "json-f32-dbfs" },
  "delivery": { "class": "stream", "maximum_age_ms": 500 },
  "required_consumer_capabilities": ["rf.spectrum.consume"]
}
```

The FFT vector is coalesced latest work for Canvas. Separate bounded
observations expose `rf.power.noise_floor` in `dBFS` as a gauge and
`rf.frames.dropped` as a monotonic counter. Prometheus receives those scalars,
not a metric per FFT bin. Raw IQ is announced as a direct, expiring data-plane
binding and is not carried as ordinary Unibus frames.

### Temperature and humidity

```json
{
  "stream_id": "sensor/rm4-office/environment",
  "semantic": { "kind": "environment.observation", "zone": "office" },
  "observation": {
    "values": [
      { "name": "temperature", "quantity": "thermodynamic_temperature", "unit": "Cel", "value_kind": "gauge" },
      { "name": "relative_humidity", "quantity": "relative_humidity", "unit": "%", "value_kind": "gauge" }
    ],
    "expected_cadence_ms": 30000,
    "maximum_age_ms": 90000
  },
  "delivery": { "class": "snapshot", "maximum_age_ms": 90000 },
  "required_consumer_capabilities": ["sensor.environment.consume"]
}
```

The snapshot coalesces by stream identity. After 90 seconds it is unknown.

### ESP32 CSI

```json
{
  "stream_id": "rf/esp32-hall/csi",
  "semantic": { "kind": "rf.csi", "zone": "hall" },
  "payload": { "contract": "rf.csi.frame.v1", "maximum_bytes": 4116 },
  "encoding": { "kind": "adr-018-iq-s8", "maximum_antennas": 4, "maximum_subcarriers": 512 },
  "clock": { "kind": "producer_sequence" },
  "delivery": { "class": "stream", "maximum_age_ms": 250 },
  "required_consumer_capabilities": ["rf.csi.raw.consume"],
  "provenance": { "producer": "esp32-hall", "privacy": "raw-csi-opt-in" }
}
```

`spatial.presence.v1` and experimental `spatial.vitals.v1` are separate
latest-value descriptors with calibration/model provenance. They must never
be presented as identity, medical, safety, or ground-truth measurements.

## Security invariants

- Descriptors and leases contain no secrets, credential material, inline
  authentication, executable expressions, filesystem paths, or pipelines.
- Unibus grants separately authorize publishing a descriptor, publishing its
  data kind, resolving an endpoint, and consuming it.
- Endpoint resolution occurs after authentication and capability matching.
- URL-like references reject userinfo. Unknown schemes, profiles, fields, and
  roles fail closed.
- Counts, string lengths, vector geometry, label sets, payload sizes, clock
  rates, and lease durations are bounded before adapter invocation.
- Raw RF, CSI, audio, and video access are distinct capabilities from derived
  observations. A Prometheus grant does not imply raw-media access.
- Adapter errors, expiry, drops, and stale state remain visible.

## v1 compatibility and migration

`stream.descriptor.v1` and `stream.endpoints.v1` remain decodable for at least
one release cycle. Existing `CameraStream` and `VideoStream` announcements in
`unibus-media` also remain accepted during migration.

1. Define v2 types in `stream-descriptors` and add golden fixtures before any
   producer changes.
2. Add a lossless compatibility projection from each valid v1 camera/video
   announcement to a v2 descriptor plus endpoint lease. Reject v1 values that
   cannot be represented safely; do not guess connection roles.
3. Make consumers prefer native v2, otherwise consume the compatibility
   projection. Deduplicate by stable stream identity and provenance.
4. Move new producers to v2, keeping v1 emission behind an explicit
   compatibility option.
5. Remove duplicate descriptor ownership from `unibus-media` only after all
   consumers read v2 and archived v1 fixtures remain covered.

The projection is one-way. V2 domain facets that have no v1 representation
must not be silently flattened into free-form `content` metadata.

## Phased implementation

| Phase | Deliverable | Exit condition |
|---|---|---|
| 1 | v2 envelope, typed facet registries, bounds, fixtures | Pure validation and serialization tests pass; no adapters involved |
| 2 | v1 camera/video projection | Golden v1 documents decode and produce deterministic v2 pairs |
| 3 | capability matcher and endpoint resolver interface | Accept/reject matrix is deterministic and explains failures |
| 4 | Radioman spectrum and observation descriptors | Canvas receives current FFT; scalar health projects to OpenMetrics; IQ stays direct |
| 5 | Isochrone descriptor/lease adapter | L16/L24 sessions preserve current RTP, playout, loss, and clock telemetry |
| 6 | allowlisted GStreamer adapter | Supported H.264/H.265 combinations create only compiled local graph specs |
| 7 | sensor, MQTT, Prometheus, and CSI projections | Expiry, coalescing, bounded labels, privacy, and grants are verified end to end |

## Validation matrix

| Concern | Positive fixtures | Required rejection |
|---|---|---|
| Identity/lifecycle | Stable descriptor with renewed or changed lease | Mismatched stream ID, expired lease, zero/overlong lease |
| Codec | H.264/H.265 bounded profiles; L16/L24 geometry | Unknown profile, impossible geometry, unbounded caps |
| Clock | 90-kHz video, 48-kHz RTP audio, producer sequence | Zero/overflowing rate, missing required mapping, contradictory rates |
| Transport | RTSP connector, SRT role, RTP connectionless, direct QUIC | Ambiguous role, unknown scheme/profile, credential-bearing reference |
| Runtime | Allowlisted local GStreamer graph constructor | Pipeline text, shell metacharacters, producer-selected element/property |
| Observation | Gauge/counter/histogram with unit and bounded dimensions | Unknown aggregation, counter-as-gauge, unbounded labels, stale-as-zero |
| RF | Bounded FFT and CSI; direct IQ lease | Excess bins/subcarriers, raw stream accepted under derived-data grant |
| QoS | Stream drops stale work; snapshot coalesces; command is reliable | Unbounded queue or silent downgrade to weaker delivery |
| Compatibility | Golden v1 camera/video and endpoint documents | Unsafe guessed role or lossy free-form v2-to-v1 flattening |
| Authorization | Exact publish/consume/resolve capabilities | Discovery treated as authority or credentials relayed in descriptors |
