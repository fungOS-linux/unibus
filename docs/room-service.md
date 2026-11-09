# Room service

Status: **control path, addressed turns, direct media proof and browser
presenter, 2026-11-09.** `crates/unibus-room` implements room membership,
presence, audio-bus declarations and addressed agent turns as a hub plugin,
plus a two-machine probe, a Canvas host, a headless agent and `rooms-serve`.
The browser presenter carries WebRTC audio between tabs; the room service still
carries no media bytes. Screen share, full-room transcription and the Holodeck
adapter are not part of this cut.

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
| `room.address.v1` | client → service | `{target, text}`; target must be an agent in the same room |
| `room.leave.v1` | client → service | `{}` |
| `room.self.v1` | client → service | `{presence, mic, speaking, presenter?}` |
| `room.list.v1` | client → service | `{}` |
| `room.welcome.v1` | service → joining client | `{revision, room, space, self, members[], buses[]}` |
| `room.addressed.v1` | service → target agent | `{revision, room, target, from, text, turn_id}`; never broadcast |
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

`room.address.v1` is text, not audio. The service checks that the sender and the
target are in the same room and that the target has the agent role. The target
receives one `room.addressed.v1`; other members receive nothing. This is the
agent's addressed-turn trigger, not a transcript and not a media path.

## Roles and identity

Roles come from the node grant, not from the client body:

| Grant capability | Room role |
|---|---|
| `room.member` | required to join and receive events |
| `room.owner` | `owner` |
| `room.agent` | `agent`; required for `room.addressed.v1` delivery |
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
- Signaling, screen share, bus mutation by clients, text bodies (use
  `fungos.conversation` records), persistence and migration when a host leaves.
- Durable room transcripts or recording. `room.address.v1` delivers an
  addressed turn; it does not create a transcript.
- A separate high-rate speaking kind; `room.self.v1` carries `speaking` today.
- Holodeck room adapter: Holodeck still uses its ENet room server.

## Reference host and agent

`room-host` is the Canvas-side reference host. It joins the service, publishes
the Rooms module snapshot over `canvas.widget.snapshot.v1` on `soil/rooms`, and
reads local actions from the retained scene. The room service does not know
Canvas exists.

`room-agent` is a headless participant. A client config needs both
`room.member` and `room.agent`; the node grant must allow the join/leave/self
kinds and `room.address.v1` for the human sender. The agent waits for
`room.addressed.v1`, reports `speaking` and tracks `mic=on` while its TTS child
is publishing through `room.self.v1`, and starts one TTS child per addressed
turn. A newer turn cancels the previous child, so the newest answer wins. TTS
is sent directly as Opus on the speech path; it is never published on the
program bus.

```sh
room-host CLIENT.json --room studio --name Avery --agent jarvis \
  --address-text 'JARVIS, report the room status.'
room-agent JARVIS.json --room studio --name JARVIS \
  --peer 127.0.0.1:48000 --bind 127.0.0.1:48010 \
  --tts-bin soil-opus-call
```

## Browser presenter

`rooms-serve` is the web presenter for the same Rooms view. It joins one room
with the identity in its Unibus client config, renders the Spore's Lua view to
the browser scene, and serves the page at `rooms.myc` through the Mycelium
gateway.

The page owns the browser microphone and one `RTCPeerConnection` per tab. The
server relays only SDP offers/answers and ICE candidates over loopback HTTP; no
media packet enters the room service or the gateway. Each tab has its own call
session and call panel, so one tab muting does not change another tab's state.
Because Unibus identity comes from the producer id, one `rooms-serve` process
is one room member even when two tabs are in the audio mesh. A second person
runs a second server with a second client identity. `room-agent` does not yet
join the browser mesh.

```sh
rooms-serve --config james.json --room studio --name James \
  --spore-root ../spores/spores/rooms \
  --ui ../spores/sdk/ui/ui.lua \
  --bind 127.0.0.1:8793
```

The page is audio-only today (`video: false`). A camera or screen track can be
added to the same peer connection later; adding video does not need a room
service change, but group video will need an SFU rather than a mesh.

The composer writes messages into this presenter's room timeline. That timeline
is host-local: every browser viewing this `rooms-serve` sees it, but another
room member's server does not. Room-wide text still belongs on the
records/conversation path, not in `unibus-room`. `Ask JARVIS` sends the typed
text as the addressed turn.

## Direct Opus proof

`soil-opus-call` in the Soil repository is the media proof for this cut. It
sends one Opus/RTP stream each way over direct UDP at 48 kHz mono, 20 ms frames,
with no SFU or relay. Use ports two apart because ffmpeg allocates RTCP on the
next port.

## Transcript-only mode

The first transcript mode records the **text** of an addressed turn, not audio,
into a host-local JSONL file (`room-host --transcript PATH` or
`rooms-serve --transcript PATH`). It is opt-in and
lives outside the room service. Full-room transcription must subscribe to each
participant's audio track separately and publish through a separate
transcription/records path; it must never record a mixed bus merely to obtain
text.
