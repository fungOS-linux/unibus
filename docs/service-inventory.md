# Service inventory and health

Implemented: shared L2 contracts in `stream-descriptors::service`, bounded
consumer projection, source-scoped bus publication/watch, HDHomeRun HTTP,
opt-in ADB publication, and Radioman's authenticated HTTP projection. This is
not an automatic adapter supervisor or an L1 router-owned catalog database.

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

## Bus publication and fleet inspection

`unibus-services publish CONFIG.json` probes one explicitly selected source
and publishes the two snapshot kinds through `unibus-sdk`. HTTP bearer
credentials may be supplied in `bearer_token_file`; tokens never enter a
catalog, health document, or bus payload. The publisher checks expected
service/owner/presenter identities before publishing. It requires router
receipts, caps outstanding requests at 32, and stops on rejected grants,
missing receipts, source failure, disconnect, or SDK overflow. No observers
is an explicitly logged idle-demand state, not successful delivery or a
reason to accumulate retries. A supervisor can restart after genuine failure.

`unibus-services watch CONFIG.json DURATION_MS` reads only the configured
producer/service bindings and emits a retained JSON view. Health received
before catalog is buffered once per configured binding, capped at 256.
Unselected producers are filtered; wrong provenance or malformed selected
evidence fails loudly. Expiry is calculated locally, never renewed by reads.
One service catalog per publisher contains that service's resource collection;
do not multiplex unrelated service catalogs under one producer's snapshot key.

Both bus modes require schema v2 and a loopback connection. Use an authenticated
private tunnel for cross-host reads. The existing router owns routing,
authentication and exact publish grants; consumers own retention. This is not
automatic cross-hub replication or Mycelium topology gossip.

Live acceptance (2026-10-06): `unibus-services` is installed on home-pi and its
HDHomeRun catalog update is active. Activation waited until physical tuners and
proxy stream sockets were idle. The inspector reports healthy with 56 resources;
the stream endpoint lists 55 schema-v2 descriptors. A short native playback
check returned HTTP 200 with MPEG-TS bytes. The publisher is not enabled yet:
the existing router's config/grants remain protected from the default account.
Radioman/ADB code adoption is verified in tests, not a claimed fleet rollout.

Manual setup:

1. Generate separate publisher and observer tokens privately using the existing
   router provisioning workflow. Preserve mode 0600; never put token values in
   examples, shell history, descriptors or logs.
2. Merge `config/services-router-fragment.example.json` into the selected
   router config, substituting local token paths. Validate the complete config
   and preserve all existing nodes/routes. Observer grants permit inventory
   reads only; no ADB/radio/media controls are granted.
3. Copy and customize `services-hdhomerun-publish.example.json` from the current
   `/v1/catalog` response, not guessed addresses or Mycelium names. Install the
   optional user publisher unit after router access is provisioned.
4. Expose the existing private router through an authenticated local tunnel,
   then customize `services-watch.example.json` for that installation and
   source scope. Run `unibus-services watch watch.json 12000`.
5. Verify exact source identities and expiry in the output. Stop the publisher
   and confirm retained evidence becomes stale. This does not tune a radio or
   allocate a TV tuner.

Existing hub configuration on home-pi is owned by another Unix account and
not writable/readable by the default Mycelium account. Live bus provisioning
must wait for authorized router-config access; do not bypass that boundary by
switching accounts or spawning an extra hub.

## Adapter adoption

ADB projects its existing six-second smart-socket inventory lease. A device
serial hashes to a stable resource ID, including network-ADB serials. Native
ADB still owns selecting/authorizing a device and all raw socket semantics.
Unauthorized/offline devices have no advertised action. Unavailable backends
report disconnected; an all-unauthorized device list reports authentication
required. A reachable empty server is healthy, not a claim that a phone exists.

Existing ADB configurations are unchanged unless serve mode sets
`publish_services: true` (requires inventory polling and schema v2). Add the
two exact publish grants and snapshot routes before enabling it. New
`unibus-adb init` output includes those grants, an inventory-only observer,
and `watch.json`. Observer credentials grant no ADB socket access.

Radioman exposes `/v1/catalog` and `/v1/health` on its existing authenticated
provider listener, derived from its registered device index and control socket.
Bearer authentication remains mandatory; DCP-Version remains required for
DCP routes, not generic inventory routes. A failed control socket reports
disconnected and withdraws advertised control operations. Registered devices
are metadata, not claims of measured hardware connectivity or free capacity.
Its legacy spectrum stream is deliberately not advertised as a v2 stream.
Radio actions still go through the native revision-bound DCP catalog; no
generic control executor or new Unibus dependency is added to Radioman.

## Next integration slices

1. Provision the protected live router grants and deploy adapter changes on
   reachable hosts; verify end-to-end under their existing private routing.
2. Add optional health/metrics discovery to declarative configuration using
   these contracts, not a parallel health schema.
3. Implement demand-driven sharing only where protocol semantics permit it,
   with bounded per-consumer queues and cancellation/drop telemetry. No media
   fanout behavior is changed by this slice.
