# Service inventory and health

Implemented first slice: shared L2 contracts in `stream-descriptors::service`,
bounded consumer projection, and HDHomeRun read-only HTTP publication. This is
not yet a fleet-wide router catalog or an automatic adapter supervisor.

The `unibus-services` inspector reads one explicitly selected private HTTP
source. It bounds responses/timeouts, disables redirects/environment proxies,
validates catalog/health schemas and expiry, and displays stale health even
when the producer has withdrawn its catalog. It does not discover hosts or
silently switch destinations.

## Boundaries

- Services own protocol implementation, probing, and command payloads.
- `stream-descriptors` owns service inventory, health vocabulary, and existing
  stream v2 descriptors/endpoint leases. No new media protocol is introduced.
- Unibus L1 continues to carry opaque, grant-checked envelopes.
- Mycelium may supply discovery, but the contracts have no Mycelium dependency.
- Cataloging an operation does not grant permission to invoke it. A consumer
  must pass the domain adapter's authorization and session checks.

`unibus.service.catalog.v1` names the service, adapter kind, protocol owner,
presenting proxy, observation/expiry, resources, stream IDs, and operations.
Each operation names its existing domain request contract and required
capability. Channel/device details come from the physical source, not YAML
copies. Declarative configuration can advertise these documents without
embedding secrets, scripts, or media pipelines.

`unibus.service.health.v1` is independent from adapter process lifecycle and
capacity. States are `healthy`, `degraded`, `disconnected`,
`authentication_required`, `busy`, `stale`, and `unknown`. HDHomeRun currently
reports authenticated catalog freshness, **not free tuner count**. Existing
playback admission reports tuner exhaustion with HTTP 503/vendor error 805.
It does not manufacture `busy` from process state or a successful metadata read.

Leases are bounded to five minutes. Consumers derive expiry at read time.
`ServiceInventory` retains at most 256 source-scoped catalogs, rejects source
mismatches, regressing/conflicting evidence and implicit owner changes, and
requires matching catalog provenance before health ingestion. Unreported
health is unknown; expired health or catalog is stale. Caller authentication,
retention/removal, discovery and polling remain explicit responsibilities.
Multiple proxies are separate routes, not independent physical tuners.

## HDHomeRun runbook

The following routes inherit the proxy's existing LAN CIDR, request bounds,
read-only methods and scoped browser CORS policy:

```sh
curl --fail http://192.168.2.74:8088/v1/catalog | jq
curl --fail http://192.168.2.74:8088/v1/health | jq
curl --fail http://192.168.2.74:8088/v1/streams | jq
unibus-services inspect http://192.168.2.74:8088
unibus-services inspect http://192.168.2.74:8088 --json
```

The catalog owner reference is the verified TLS server name, not a guessed
Mycelium node name or the relay IP. The presenter reference identifies the
scoped LAN proxy. DeviceAuth, credential paths and raw upstream errors never
appear in these documents. Protected channels, including CSV `Tags: drm`, are
excluded from consumable resources and descriptors.

`/v1/streams` returns objects containing the existing `StreamDescriptorV2` and
`EndpointAnnouncementV2` types. MPEG-TS geometry/codecs remain unspecified
unless measured; do not infer them from a channel name. Logical endpoint
`hdhomerun.DEVICE.channel.NUMBER` resolves through this adapter to
`http://LAN_PROXY:STREAM_PORT/auto/vNUMBER`. Ports come from proxy configuration;
the consumer must know the selected proxy's native stream base. Arbitrary
URLs and upstream targets cannot be passed through the typed private API.

Polling never extends the producer observation. Catalog/stream endpoints return
503 when expired; health remains readable with `state: stale` and HTTP 503.
Existing stock HDHomeRun discovery and playback endpoints remain unchanged.

## Verification

On the build host, run `cargo check` for both packages, then their tests and
strict Clippy. Tests cover schema/lease bounds, provenance, replay regression,
unknown fields, protected-channel filtering, secret-free projection, matching
stream references, real authenticated QUIC/HTTP catalog reads and stale expiry.
Build artifacts only for deployment. A catalog-only change requires updating
the proxy binary; the owner's private v2 protocol is unchanged.

## Next integration slices

1. Publish/retain these documents through the existing bus grants and expose a
   fleet scope in the inspector; do not invent another discovery daemon.
2. Adapt Radioman and ADB using their existing source-owned probes and commands.
3. Add optional health/metrics discovery to declarative configuration using
   these contracts, not a parallel health schema.
4. Implement demand-driven sharing only where protocol semantics permit it,
   with bounded per-consumer queues and cancellation/drop telemetry. No media
   fanout behavior is changed by this slice.
