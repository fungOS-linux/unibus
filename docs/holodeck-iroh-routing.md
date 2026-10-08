# Holodeck room route through Mycelium and Iroh

The Quest's ordinary path stays on its own LAN: Quest → home Holodeck room
server over ENet. Remote rooms are explicitly selected. The home host imports
an allowed remote host endpoint onto its LAN, and the Quest uses the existing
room list and `visit`/`dock` choice. The remote Holodeck server remains the
authority for membership, grants, and docking. An imported host is not a merger
of two independent room authorities; the Quest changes its active room when it
joins the remote host.

The current cross-LAN importer already has the bounded UDP/ENet forwarding and
local discovery responder. Its cross-site leg is authenticated Unibus QUIC over
an explicitly routed network. The intended Iroh transport substitutes only
that cross-site leg, reusing the importer/exporter policy and datagram framing.
It must not add another room server or bypass owner admission. Iroh endpoint
keys authenticate the transport peers; they are not user identities or room
grants.

## Discovery boundary

The endpoint-owning exporter will publish an actual bound endpoint in
Mycelium's `iroh-endpoints.json`, using ALPN `fpl-holodeck-room/1`. Mycelium
gossips the short owner lease. `room_directory::selected_hosts` consumes the
existing UniBus `ServiceDirectory`; `MyceliumServiceDirectory` implements it
through Mycelium's public RPC. It admits only fresh, owner-provenance records
whose endpoint ID is pinned by local import policy. It returns direct and relay
hints for Iroh dialing. It does not treat discovery as authorization.

Read-only inspection is available through:

```text
holodeck-room-hosts ~/.mycelium/myceliumd.sock TRUSTED_ENDPOINT_ID...
```

No application has published an Iroh endpoint lease as of 2026-10-08, so this
catalog currently returns no room route. The Agora Iroh relay also needs live
HTTPS and endpoint allowlist qualification. Until both are ready, the existing
LAN and authenticated Unibus bridge continue to serve their current paths.

## Route acceptance checks

1. The Quest joins its home room with remote services absent.
2. A remote export publishes a real, renewing Mycelium lease only while its
   Iroh endpoint and room server are ready. Withdrawal expires in at most 300 s.
3. The importer pins the expected remote endpoint key and room export; a
   discovered but unselected endpoint is never dialed or advertised locally.
4. The Iroh connection succeeds directly across LANs where possible, then
   through the configured private relay when direct routing fails. The
   importer forwards ENet datagrams with existing client and size bounds.
5. The Quest explicitly selects a remote room and chooses `visit` or `dock`.
   The remote owner approves docking. Revocation and disconnect still release
   room permissions and held props.
6. Loss of the remote lease or Iroh path withdraws the imported room while
   the home room remains joinable. A returning endpoint requires a fresh lease
   and a newly authenticated connection.

The direct Quest-to-remote-Iroh path and persistent simultaneous membership in
two room authorities are separate features. The present room client stores one
active membership, and the server's docking placement operates within one
authority. Neither behavior follows from changing transport alone.
