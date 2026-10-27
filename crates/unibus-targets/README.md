# unibus-targets

Bounded index of external HTTP/RPC operations that an Unibus connector may
route to. **This crate does not create ACLs or grants.**

```sh
unibus-targets openapi billing path/to/billing.openapi.yaml
```

Output is a `TargetIndex`:

- one stable target id per operation (`billing.get-invoice`)
- one Unibus message kind (`billing.get-invoice`)
- transport metadata (`http` method/path or `rpc` service/method)
- request/response schema references
- a presentation-only risk hint (`read`, `write`, `destructive`)
- source location and SHA-256 digest

Authority stays in Mycelium. Unibus enforces the grants/capabilities that
reference these target ids. An OpenAPI import must never create authority:
ingestion produces candidates, then a policy/approval step grants them.

## Boundary

```text
OpenAPI/RPC descriptor
        → unibus-targets (this crate)
        → reviewed target index
        → Mycelium grants reference target ids
        → Unibus connector dispatches only indexed targets
```

The connector must not accept arbitrary URLs, paths, methods, or headers.
Unknown targets are rejected before any backend call.
