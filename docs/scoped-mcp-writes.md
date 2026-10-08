# Operator-authorized MCP writes

Build `unibus-mcp` and `unibus-mcp-authorize` with the
`scoped-authorization` feature. Configure the bridge and publisher with the
same private (0600), regular `authorization_policy_file` and disable
`lan_permissive_mutations` on both. Initially the policy contains
`{"schema_version":1,"grants":[]}`. Never commit this policy: issued grants
are capabilities.

Review publisher tool classification independently. A non-destructive write
can be advertised without `destructiveHint: true`, and therefore remain
`unknown`. Explicit `mutation_overrides` should identify only the reviewed
tools as `mutating`; do not classify every tool as writable.

Store the approved, exact tool arguments in an ordinary JSON file. Issue the
grant and pipe the resulting MCP request directly into the local bridge:

```sh
unibus-mcp-authorize /absolute/private/policy.json PROVIDER TOOL arguments.json |
  curl --fail-with-body --max-time 65 \
    -H 'Content-Type: application/json' --data-binary @- \
    http://127.0.0.1:18791/mcp
```

The helper's stdout includes a secret capability. Never log it, paste it into
chat, capture it in build artifacts, or use shell tracing. The five-minute
grant binds the provider, tool, and exact arguments; both bridge and publisher
verify it. Remove the grant to revoke it immediately. It is reusable within
that interval, not a one-use token: inspect the tool result and read back
external state before retrying a write.

Validate that reads still work and an ungranted mutation fails before issuing
real writes. An MCP client that cannot attach `_meta.unibus_authorization`
still cannot invoke writes directly; this helper supplies that metadata
through the same guarded MCP endpoint, not through vendor REST bypasses.

The Discord bot also requires ordinary writes enabled locally. Invite creation
uses its dedicated guild/channel-checked method. Leave destructive admin
disabled; raw admin mutations remain separately denied.
