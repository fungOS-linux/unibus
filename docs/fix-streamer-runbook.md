# FIX streamer runbook

`unibus-fix` keeps the venue protocol at the edge. It accepts a FIX byte stream,
normalizes market-data snapshot (`35=W`) and incremental refresh (`35=X`)
messages, and publishes two asset-neutral Unibus contracts:

- `market.order_book.v1`: symbol, venue, sequence, timestamp, bids, and asks.
- `market.trade.v1`: symbol, venue, sequence, timestamp, price, size, and side.

Canvas's `canvas-edge` translates those contracts to its local stream socket.
The existing market binders then update order-book and time-and-sales widgets.
Equities and crypto connectors can therefore share the same Canvas surface.

## Hub grants

Grant the streamer its capability and publication kinds, then route both kinds
to all Canvas displays in the destination zone:

```json
{
  "routes": [
    {"kind":"market.order_book.v1","target":{"mode":"capability","name":"canvas.display"}},
    {"kind":"market.trade.v1","target":{"mode":"capability","name":"canvas.display"}}
  ],
  "nodes": [
    {
      "producer_id":"fix-market-edge",
      "token_file":"secrets/fix-market-edge.token",
      "zones":["default"],
      "capabilities":["market.fix"],
      "publishes":["market.order_book.v1","market.trade.v1"],
      "publish_zones":["default"]
    }
  ]
}
```

Merge these entries into the installation's existing arrays; do not replace
the other routes or nodes. The Canvas nodes must claim `canvas.display` (or use
the installation's existing display capability consistently).

## Configure and run

Copy `config/unibus-fix.example.json`, set its hub address and source, and make
the token available without putting it in the file:

```sh
export UNIBUS_FIX_TOKEN="$(cat /secure/path/fix-market-edge.token)"
cargo run -p unibus-fix -- config/unibus-fix.json
```

Source modes are:

- `{"mode":"stdin"}` for captures and pipe-based adapters.
- `{"mode":"file","path":"capture.fix"}` for deterministic replay.
- `{"mode":"tcp","endpoint":"127.0.0.1:9100"}` for a live, already-authenticated
  FIX feed or local session-engine socket.

The TCP mode is deliberately a data-stream adapter, not a FIX initiator. Venue
credentials, Logon fields, resend policy, sequence persistence, and TLS belong
to the counterparty-specific session engine. Point that engine's normalized
drop-copy output at this process. Disconnects and malformed frames terminate
the process loudly so the service manager can restart or alert.

This remains an optional adapter, not part of `unibus-core`. Its initial
normalizer supports book entries (types 0/1), trades (type 2), and entry-ID-based
incremental changes. New/change book groups must supply price and size; sparse
deletes require an entry ID but do not require price, size or entry type. Other
market-data types are ignored. It is not a complete venue-specific FIX parser:
BodyLength, arbitrary binary fields, session sequencing/resends, exchange-time
conversion and price-level aggregation are not implemented. Trade `time` is
currently receive time, not exchange time. Bound session-engine output to this
subset; do not connect this directly to a venue and assume session correctness.
Price/size must be finite, prices positive, sizes nonnegative. Books are isolated
by venue and symbol, with deterministic ordering for equal prices. Replay EOF
must end on a complete frame; TCP EOF is an error rather than a successful exit.

## Verify

Replay a capture containing complete checksum-terminated frames and inspect the
Canvas surface. A book snapshot should sort bids high-to-low and asks low-to-high;
an incremental delete (`279=2`) should remove its `278` entry. Run the automated
checks with:

```sh
cargo test -p unibus-fix
```
