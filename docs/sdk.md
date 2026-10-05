# Embedded Unibus SDK v1

`unibus-sdk` owns a shared Rust control-plane session. `unibus-lua` embeds it
directly through mlua (Luau by default); `unibus-ffi` exposes
the same implementation through a C ABI. There is no extra daemon or protocol.

## Contract

- `connect(config)` uses the existing ClientConfig registration and token-file
  or token-environment authentication. Do not put token values in scripts.
- `publish({destination, kind, body})` validates and queues an object body.
  Returns a request ID for reply correlation. Success means queued, not delivered
  or authorized: consume router replies. C returns that ID as an allocated string.
- `poll()` returns one event, or nil/empty. It never waits for network reads.
  Events contain producer_id, request_id, issued_at_unix_ms, kind and body;
  transport credentials and binary payloads are not exposed.
- `status()` exposes connected and cumulative dropped_events. Disconnection
  requires explicit reconnect. Commands are never automatically replayed.

Both queues are bounded to 128 entries. A full outbound queue errors. Full
inbound queues drop new events and increment dropped_events; bulk or oversized
events also increment this counter. Control bodies are limited to 64 KiB.
This is **not** a retained projection: consumers of state must resynchronize
after drops. Interest leases, catalogs and DCP messages retain their existing
wire contracts and owner-defined semantics; this SDK does not replace them.

Network IO runs on one Rust worker per session. Host polling and Lua callbacks
stay on the host thread. Connect can block up to the existing registration
deadline, and close joins the worker (a blocked send can take up to the existing
five-second write timeout). Do not connect/close inside a rendering frame.
No SDK routing permission bypass exists; router grants remain authoritative.
Token-file paths/config are host inputs, so install this module only in trusted
scripts. For sandboxed scripts, use `unibus_lua::attach(&lua, "bus", session)`
instead of `install`: scripts receive a host-selected session, not connect or
token-source selection. Router grants must still limit that session's authority.

## Lua host

Call `unibus_lua::install(&lua)` in your Rust host. The default backend is
vendored Luau, matching Canvas. LuaJIT is opt-in for VR hosts only:
`default-features = false, features = ["luajit"]`. No system Lua installation
is required. Select exactly one backend across the host's dependency graph.
Lua 5.4 hosts select `default-features = false, features = ["lua54"]`.

```toml
# Canvas/Luau host (default)
unibus-lua = { path = "../unibus/crates/unibus-lua" }

# VR/LuaJIT host: use this instead, not both.
# unibus-lua = { path = "../unibus/crates/unibus-lua", default-features = false, features = ["luajit"] }
```

The publish/poll/status contract is identical on all backends. LuaJIT is not
Luau's optional JIT backend and does not accept Luau-specific syntax. This
SDK change does not migrate Canvas's runtime or existing scripts. Hosts own VM
creation and sandbox policy; do not enable LuaJIT's unsafe FFI library for
untrusted widgets. Network/framing remains Rust-owned; JIT accelerates script
computation, not network latency. Backend support follows [mlua's feature
contract](https://github.com/mlua-rs/mlua#feature-flags).

```lua
local bus = unibus.connect({
  installation_id = "home", producer_id = "canvas-widget",
  hub_endpoint = "127.0.0.1:5000", token_file = "/path/to/host/token",
  zones = {"home"}, capabilities = {"canvas.widget"}, schema_version = 2
})
-- Run from your host update callback; cap work each frame.
for i = 1, 16 do
  local event = bus:poll()
  if not event then break end
  -- Dispatch on event.kind; apply owner-specific validation before use.
end
local health = bus:status()
```

## C hosts

Build `cargo build --release -p unibus-ffi`. Include
`crates/unibus-ffi/include/unibus.h` and link the generated library. Config,
publish and event shapes are UTF-8 JSON and identical to the Rust/Lua contract.
Serialize calls on each opaque session handle. Free returned strings exactly
once using `unibus_sdk_string_free`; close each session exactly once. Never
pass stale pointers. Codes: -1 error, -2 caught panic; poll returns 0 empty or
1 event. Other operations return 0 success. v1 intentionally reports generic
C errors rather than leaking credential-bearing transport diagnostics.

## Verify

`cargo check -p unibus-sdk -p unibus-lua -p unibus-ffi`

`cargo test -p unibus-sdk -p unibus-lua -p unibus-ffi`

The LuaJIT suite checks that JIT is enabled and a hot loop generates a
compiled trace. All backend suites exercise full-duplex messaging through the VM.
Check alternative hosts independently (backend features are mutually exclusive):

`cargo test -p unibus-lua --no-default-features --features luajit`

`cargo test -p unibus-lua --no-default-features --features lua54`

This slice does not yet wire Canvas widgets to the module, add service catalog
caching, or add typed DCP execution helpers. Those should reuse owner crates,
not grow a second discovery/routing implementation here.
