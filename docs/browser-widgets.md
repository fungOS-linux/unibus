# Optional Coherence telemetry views

The router's `viewer` feature can retain bounded
`coherence.widget.snapshot.v1` messages and render them with Coherence's shared
Rust/WASM `gfx` host. UMIE and Shroud reuse the same renderer, loader and metric
contract. Unibus owns transport and authorization, not provider internals or
service discovery.

## Build and configure

1. Build Coherence's optional bundle with `npm run build:widgets`. Copy the four
   runtime files in `dist/widgets` into an application-owned asset directory.
2. Build `cargo build -p unibus-router --features viewer --bins`.
3. Configure the existing authenticated HTTPS viewer with `widget_assets` (an
   absolute directory) and a `widgets` array. Each definition has `id`, `title`,
   `producer_id`, `zone`, and `capability`. Missing assets fail startup.
4. Grant the browser principal `unibus.viewer`, the definition's capability and
   zone. Bind its certificate digest explicitly. Optional
   `bootstrap_viewer_certificates` may pin an existing self-signed human leaf;
   each pin must already have a digest binding. Pins do not confer mesh trust.
5. Grant the source `widget.source`, the exact message kind in `publishes`, and
   the definition's zone in `publish_zones`. Add a normal capability route for
   subscribers; HTTP viewers read retained data without registering a socket.
6. Run `unibus-widget-source CONFIG` with a private JSON configuration. See
   `config/widget-source-agora.example.json`. Update installation identity,
   paths and provider endpoints from verified local evidence before using it.

The collector runs configured read-only commands with a five-second deadline
and bounded stdout. It selects exact scalar series or CSV columns; it is not a
general Prometheus server. Labels, units, divisors, differences and maximums
are explicit configuration. GPU memory capacity is read from the provider,
not maintained as a static machine specification. Secrets stay in the source's
private token file; no command or provider URL enters a browser snapshot.

## Read and verify

Authorized links appear on the index. Views are `/views/umie` and
`/views/shroud`; JSON is `/api/widgets/{id}`. Asset requests also require a
certificate. Anonymous requests fail the TLS handshake. A trusted certificate
without an authorized digest/grant fails access checks. Zone and capability
checks apply to each widget, not just the index.

Snapshots are limited to 32 KiB, 16 metrics and 120 history samples each. Only
configured producer/zone/widget tuples can occupy retention. Invalid,
duplicate and out-of-order updates preserve the last valid frame. Expiry is
explicit (maximum two minutes). The browser retains stale data rather than
fading it out and shows disconnected fetches. Missing metrics remain null,
never a fabricated zero. Collection failure is visible as unavailable metrics.

The GPU host retains its pipeline and buffers, draws only when data, size or
theme changes, skips hidden tabs, and awaits submitted work before drawing
again. Colors resolve from Coherence's CSS semantic tokens. HTML labels remain
selectable and accessible; the canvas is decorative. Unsupported WebGPU shows
a visible degraded message and numeric telemetry instead of claiming GPU use.

For runtime verification, use `curl --cacert SERVER_CERT --cert HUMAN_CERT
--key HUMAN_KEY https://HOST:PORT/api/widgets/umie` (do not use `-k`). Verify a
second observation timestamp advances, check missing-certificate rejection,
and check `application/wasm` and same-origin CSP on assets/pages. Browser visual
QA is separate from HTTP checks and WASM compilation.

Mycelium owner-lease advertisement remains optional. Publishing a lease does
not grant access, establish DNS aliases, or make the local inventory global.
