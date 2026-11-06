# Consent to Unibus grants

`unibus-consent` is a thin lowering layer from an approved host consent
decision to an ordinary Unibus `NodeGrant`.

```text
ConsentBroker request (kind = unibus_grant)
  -> user allows
  -> unibus-consent checks:
       same request id
       allowed decision
       previewed payload digest still matches
       grant validates
  -> caller installs the NodeGrant into normal Unibus policy/lease state
```

It does not bypass Unibus verification, mint a new grant format, or create a
second ACL. `Scope::Once` shortens the grant to five minutes; session grants are
bounded to one day; persistent grants use the requested lifetime.

The same consent request can also be lowered by an MCP scoped-grant adapter or
a Mycelium scope adapter. The decision is shared; the grant systems stay
separate.
