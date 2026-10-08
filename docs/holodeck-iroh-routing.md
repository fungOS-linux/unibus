# Holodeck room route through Mycelium and Iroh

The Quest's ordinary path stays on its own LAN: Quest → home Holodeck room
server over ENet. Remote rooms are explicitly selected. The home host imports
an allowed remote host endpoint onto its LAN, and the Quest uses the existing
room list and `visit`/`dock` choice. The remote Holodeck server remains the
authority for membership, grants, and docking. An imported host is not a merger
of two independent room authorities; the Quest changes its active room when it
joins the remote host.

The current cross-LAN importer already has the bounded UDP/ENet forwarding and
local discovery responder. Its cross-site leg can use authenticated Unibus QUIC
over an explicitly routed network or Iroh between pinned host endpoint keys.
Both paths use the same importer/exporter policy and datagram framing.
It must not add another room server or bypass owner admission. Iroh endpoint
keys authenticate the transport peers; they are not user identities or room
grants.

## Discovery boundary

The endpoint-owning exporter publishes an actual bound endpoint in
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

The exporter renews its lease every 30 seconds while at least one configured
room answers its host probe, then calls Mycelium's existing scan RPC to gossip
the observation; each lease expires after 120 seconds. It merges its
entry into the shared Mycelium file under a local lock, preserving other fresh
application entries. A process crash leaves an expiring lease, not a permanent
route. The importer requires a fresh matching Mycelium observation on each
connection attempt. The room server's six-second session lease withdraws the
LAN advertisement if the Iroh connection or host becomes unavailable.

## Configure the two room servers

Build `unibus-holodeck` with `--features iroh`. On each host, run
`holodeck-iroh keygen /private/path/room-iroh.key`; this creates a mode-0600
key file and prints the public endpoint ID. Exchange public IDs out of band.
The remote owner must allowlist your home importer's ID. Keep private keys on
their own hosts.

On the remote room server, configure `holodeck-iroh run` with an export file:

```json
{
  "mode": "export",
  "key_file": "/private/path/room-iroh.key",
  "relay_url": "https://relay.example.net/",
  "relay_ca": "/private/path/relay-ca.pem",
  "mycelium_home": "/private/path/.mycelium",
  "mycelium_socket": "/private/path/.mycelium/myceliumd.sock",
  "allowed_importers": ["<home room server's 64-character endpoint ID>"],
  "rooms": [{
    "room_id": "friend/room",
    "label": "Friend's room",
    "target": "127.0.0.1:6780",
    "discovery": "127.0.0.1:6781"
  }]
}
```

On your home room server, use an import file:

```json
{
  "mode": "import",
  "key_file": "/private/path/room-iroh.key",
  "relay_url": "https://relay.example.net/",
  "relay_ca": "/private/path/relay-ca.pem",
  "mycelium_socket": "/private/path/.mycelium/myceliumd.sock",
  "remote_endpoint_id": "<friend's 64-character endpoint ID>",
  "lan_address": "192.168.1.10",
  "discovery_port": 6781,
  "allowed_clients": ["192.168.1.20"],
  "allowed_subnets": [],
  "rooms": [{"room_id": "friend/room", "lan_bind": "192.168.1.10:6790"}]
}
```

Use the actual room host and Quest addresses and ports. The import process owns
the selected LAN discovery port, so run it where the ordinary room responder
does not bind that same address and port. Both peers must configure the private
relay to get relay fallback; omit `relay_url` and `relay_ca` only for a direct
path test. Run `holodeck-iroh check CONFIG.json` before `run`. Confirm the
exported lease appears in `mycelium services --json` and in
`holodeck-room-hosts MYCELIUM_SOCKET REMOTE_ENDPOINT_ID` on the home host.
Direct and relayed Iroh paths still need verification on the two real networks.

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
