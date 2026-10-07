# Agent session telemetry

The reusable normalizer lives in `crates/unibus-lua/lua/agent_sessions.lua`.
It publishes `agent.sessions.v1`, not Canvas widget specifications. Provider
networking and hooks belong to a trusted host; UI composition belongs to Canvas.
This is a tested integration foundation, not an installed fleet collector.

## Host input and normalized stream

Supply `provider`, `host_id`, `instance_id`, a monotonically increasing `sequence`,
`mode` (`snapshot` or `event`), and `sessions`. The host supplies the observation
clock separately. Scope identity is the tuple `(host_id, provider, instance_id)`;
session identity adds `id`. Do not deduplicate by title or process name.

A snapshot replaces exactly that scope. An event merges just its supplied
sessions. `ended` removes a session. Reject old sequences within a scope; a
restarted publisher needs a new instance ID. Consumers must bound retained
owners/sessions, age observations to unknown, and resynchronize after SDK drops
or reconnects. The SDK itself is not retained storage.

Output sessions contain only `id` and `state` (`active`, `idle`, `unknown`,
`ended`). No prompts, titles, paths, credentials or terminal text are forwarded.
Unknown includes unsupported states and blocked/retrying sessions; it must not
be silently counted as idle. Active means the provider reports work in progress,
not measured CPU use or a claim that its model is currently generating tokens.

## Sources

- Codex: read-only loaded-thread status from the local app server. Provide each
  thread's `id` and `status`. Missing or nonempty active flags map to unknown.
- Claude Code: lifecycle hook observations with `session_id` and
  `hook_event_name`; use event mode. UserPromptSubmit/tool activity indicates
  active; Stop indicates idle; SessionEnd removes it. Hooks alone cannot prove
  ongoing liveness after a crash: a host collector must provide liveness or mark
  an expired observation unknown. Do not replay old hooks as fresh heartbeats.
- OpenCode: the owner's `/session/status` response supplies `busy`, `idle`, or
  `retry`. Convert the map into sessions with their IDs and status objects.
- Operator: feed its existing owner roster using `id` and explicit `state`.
  The normalizer does not introduce another Operator network protocol.
- Other owners: publish explicit normalized states through the same contract.

References: [Claude hooks](https://code.claude.com/docs/en/hooks),
[OpenCode server](https://dev.opencode.ai/docs/server/).

## Manual publishing host

The existing SDK can embed the Lua module in an owner host. For a bounded stdin
integration harness, use:

```sh
cargo run -p unibus-lua --example agent_sessions -- /private/client-config.json
```

Feed one JSON owner observation per line. The host caps input at 64 KiB, stamps
the clock, and uses the existing authenticated SDK. It waits for the correlated
router acknowledgement and errors on rejection, timeout, disconnect or drops;
queue acceptance is not delivery. Router grants must authorize the producer's
telemetry publication. Never put credentials in an observation or Lua script.
This harness does not configure provider hooks, scrape HTTP endpoints, or start
new agents. Those source connections and the Canvas widget remain to be wired.

## Opt-in attention contract: agent.sessions.v2

V1 is unchanged: IDs and state only. The separate `agent_sessions_v2.lua`
module composes V1 and adds optional `project`, `task`, and
`attention: {reason: input|approval, since_unix_ms, timing: first_observed}`.
Labels come only from explicit `display_project`/`display_task` inputs; callers
must authorize sharing them with the stream's viewers. Prompts, previews,
transcripts, credentials and full working-directory paths are never copied.
IDs remain scoped by host/provider/publisher instance, not human-readable labels.

Codex waiting flags and Operator `pending_approval` are explicit evidence.
Claude `Notification/permission_prompt` is approval evidence; `idle_prompt`,
OpenCode retry, blocked processes and missing flags are not pending questions.
Other owners can supply `attention: input|approval` explicitly. Provider transport
and hook installation remain host responsibilities; this does not install every
provider collector.

Wait timing is first observed by this collector, not an exact provider event time.
It resets on clearing, collector restart or a gap over 30 seconds. Scope and
retained attention budgets are 64 owners / 512 sessions each; replayed sequences,
conflicting duplicates, invalid clocks and control characters fail visibly.

Set runtime config `contract` to `agent.sessions.v2`, use a separate attention
stream, and choose Canvas's `agent-dashboard-v2.lua` consumer. The Codex source's
optional `--display-details` shares only the basename of its working directory
and its explicitly named thread title. Missing titles stay unnamed; previews
are not substituted as task names. Default source mode still omits metadata.

```sh
cargo check -p unibus-lua --example agent_runtime --example agent_source_codex
cargo test -p unibus-lua --no-default-features --features lua54 --test agent_attention --test agent_sessions --example agent_source_codex
cargo build -p unibus-lua --no-default-features --features lua54 --example agent_runtime --example agent_source_codex
```

The manual runbook is: authorize display labels; configure the authenticated
producer/viewer and attention stream; select V2 normalization and presentation;
start the host; verify fresh counts, explicit waiting rows and stale handling.
No response or approval commands are exposed by this read-only stream.

## V1 verification

```sh
cargo check -p unibus-lua --example agent_sessions
cargo test -p unibus-lua --test agent_sessions
```
