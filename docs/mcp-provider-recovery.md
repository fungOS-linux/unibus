# MCP publisher recovery

`unibus-mcp publish` keeps one isolated session worker per configured provider.
Initialization, catalog refresh and tool execution run in that worker, not the
Unibus transport/authorization loop. A hung or offline provider must not prevent
other providers from publishing their leases or completing calls.

Failures are logged with the provider ID. That provider stops renewing its lease
and refuses new calls until a valid catalog has been obtained again. Bridges
expire the last advertisement normally; reconnecting does not give a stale
catalog a new observation timestamp.

Retries wait 1, 2, 4, 8, 16, then at most 30 seconds. A successful catalog resets
the backoff. Each worker has a bounded 16-call queue; the publisher's event queue
is bounded at 128. Queue overflow returns an explicit invocation error.

Authorization is checked both before admission and immediately before execution
against the worker's fresh catalog. In configurations using live policy files or
peer ACLs, queued calls must not bypass expiry or revocation. A failed tool call
is returned to the caller and is **never replayed** on reconnect: its effects may
already have happened. Transport failure still stops the publisher; isolation
does not conceal a failed Unibus connection.

## Operator checks

1. Run `unibus-mcp inspect PATH_TO_CONFIG` to check each provider independently
   of Unibus. This diagnostic remains fail-fast.
2. Start `unibus-mcp publish PATH_TO_CONFIG` under the existing service manager.
3. Inspect stderr for `MCP provider ID ready` or an unavailable/retry message.
4. Through the aggregate MCP endpoint, list tools and call a known read-only
   tool. `inspect` alone does not prove routing or authorization works.
5. For an isolation test, use a disposable stdio provider with an absolute
   command such as `/usr/bin/false`. Healthy providers must remain discoverable
   after a full lease interval, and their read-only calls must still work.
6. Restore the test configuration and verify the real provider catalog again.

Tests cover bounded backoff, a failed provider beside a healthy provider,
recovery, slow initialization, non-replayed calls, worker shutdown, and denied
queued calls after catalog expiry or authorization revocation.
