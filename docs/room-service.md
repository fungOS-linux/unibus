# Room service

Status: **first pass, 2026-11-09.** `crates/unibus-room` implements room
membership, presence and audio-bus declarations as a hub plugin, plus a
two-machine probe. Media transport (isochrone/WebRTC), screen share and the
Holodeck adapter are not part of this cut.

A room is one shared space with two presenters: the flat Canvas view and
Holodeck. Membership, the `space` id, presence and the audio buses are shared;
the presenter is a per-member field. The room is hosted by a member's node: the
binary below is simply a Unibus hub with the room plugin installed, so there is
no central room registry.

## Contract

The plugin handles every `room.*` kind before default routing, so a hub config
needs no `routes` entry for these. A client must hold the `room.member`
capability; the member id is the authenticated Unibus `producer_id`, never a
body field.

| Kind | Direction | Body |
|---|---|---|
| `room.join.v1` | client → service | `{room, space?, display_name, presenter?, presence?, audio?}` |
| `room.leave.v1` | client → service | `{}` |
| `room.self.v1` | client → service | `{presence, mic, speaking, presenter?}` |
| `room.list.v1` | client → service | `{}` |
| `room.welcome.v1` | service → joining client | `{revision, room, space, self, members[], buses[]}` |
| `room.member.v1` | service → room members | `{revision, room, change: joined/left/updated, member}` |
| `room.list.reply.v1` | service → requester | `{rooms[]}` |
| `room.error.v1` | service → requester | `{code, message}` |

`space` defaults to `room`. `presenter` is `canvas` or `holodeck`; a member may
switch presenter with `room.self.v1` without changing membership. `audio` is
`speech` or `program`; a member publishing `program` is how a music bot appears
in the bus publisher count.

Every room command also receives the hub's ordinary `unibus.reply` ack. The ack
means the hub accepted and queued the publication; the `room.*` event is the
domain outcome. Read both.

## Roles and identity

Roles come from the node grant, not from the client body:

| Grant capability | Room role |
|---|---|
| `room.member` | required to join and receive events |
| `room.owner` | `owner` |
| `room.agent` | `agent` |
| neither | `member` |

A room that has no explicit owner is still created by the first joiner; the
service does not invent an owner. Policy beyond that (who may create, invite or
revoke) is not implemented yet.

## Host configuration

`unibus-room` reads the normal `HubConfig` file. The following is a minimal
trusted-LAN example with two members:

```json
{
  "installation_id": "home",
  "listen": "0.0.0.0:6789",
  "allow_insecure_lan": true,
  "nodes": [
    {
      "producer_id": "avery",
      "token_file": "tokens/avery.token",
      "zones": ["default"],
      "capabilities": ["room.member", "room.owner"],
      "publishes": ["room.join.v1", "room.leave.v1", "room.self.v1", "room.list.v1"],
      "publish_zones": ["default"]
    },
    {
      "producer_id": "james",
      "token_file": "tokens/james.token",
      "zones": ["default"],
      "capabilities": ["room.member"],
      "publishes": ["room.join.v1", "room.leave.v1", "room.self.v1", "room.list.v1"],
      "publish_zones": ["default"]
    }
  ]
}
```

`allow_insecure_lan` is a plaintext-TCP escape hatch for a trusted LAN. Use
QUIC/TLS (`quic` in the hub config) or a private tunnel for anything else. Each
node needs its own token file; do not share one token across identities.

A client config is the normal `unibus::client::ClientConfig` shape. It must
claim `room.member` and a zone the grant allows:

```json
{
  "installation_id": "home",
  "producer_id": "avery",
  "hub_endpoint": "192.168.1.20:6789",
  "token_file": "tokens/avery.token",
  "zones": ["default"],
  "capabilities": ["room.member"],
  "schema_version": 2
}
```

## Run

On the host:

```sh
cargo run -p unibus-room -- hub.json
```

On each machine, from the repository (or with the installed binary):

```sh
cargo run -p unibus-room --bin room-probe -- avery.json studio --name Avery
cargo run -p unibus-room --bin room-probe -- james.json studio --name James --presenter holodeck
```

`room-probe` prints the welcome, every member event, and `--list` prints the
room directory. It leaves the room on exit or after `--seconds` (default 30).
The `unibus-room` package's integration test performs the same flow over a real
loopback hub.

A healthy two-client run prints one membership per client:

```text
joined studio (space studio) revision 1
  avery  Avery  Owner/Canvas  Active
  bus Speech  40 ms  1 publishers
  bus Program  250 ms  0 publishers  lossless
member joined james  speaking=false  revision 2
```

The second client's welcome carries both members and the presenter it joined
with. This shows a two-client run on one host; a real two-machine run is the
remaining phase-1 exit.

The binary uses `RoomConfig::default()`: `speech` at 40 ms and `program` at
250 ms, lossless. A host embedding the plugin can pass its own `RoomConfig`
to `RoomPlugin::with_config`.

## Limits and lifecycle

- 64 rooms, 32 members per room, 4 buses; buffer targets 10–2000 ms.
- One room per member. Joining another room leaves the first and emits a `left`
event to that room; rejoining the same room is idempotent and does not bump the
revision.
- A room disappears when its last member leaves.
- Disconnected producers are pruned on the next room event; remaining members
  receive a `left` event. A host restart clears all room state.
- `room.list.v1` returns room names, spaces, member counts and revisions to any
  `room.member` holder. It never returns display names.

## Not implemented

- Media: audio bus bytes stay on isochrone/WebRTC adapters; this service carries
  membership and bus state only.
- Signaling, screen share, `room.addressed.v1`, bus mutation by clients, text
  bodies (use `fungos.conversation` records), persistence and migration when a
  host leaves.
- A separate high-rate speaking kind; `room.self.v1` carries `speaking` today.
- Holodeck room adapter: Holodeck still uses its ENet room server.
