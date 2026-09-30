# MIDI bridge runbook

`unibus-midi` forwards exact MIDI packets from one local device to one
explicitly addressed remote output:

```text
USB keyboard -> CoreMIDI -> Mac edge -> Unibus -> Pi edge -> ALSA -> Scarlett 18i20 MIDI OUT
```

The bus transport is reliable and ordered. Capture never blocks CoreMIDI or
ALSA: a bounded local queue hands packets to the bus writer. A full queue is a
loud error in the log and drops the new packet. The receiver sends packets
immediately; source timestamps are retained for diagnostics, not scheduling.

## Discover device names

Run on each host:

```sh
unibus-midi --list
```

Port matching is case-insensitive substring matching and must select exactly
one port. Use the shortest stable, unambiguous portion of the displayed name.
The special selector `"*"` selects the device only when exactly one port is
present, which is useful for a dedicated keyboard host.

## Hub configuration

Add the producer-addressed route and grants to the existing hub configuration:

```json
{
  "routes": [
    { "kind": "midi.packet.v1", "target": { "mode": "producer" } }
  ],
  "nodes": [
    {
      "producer_id": "mac-studio-midi",
      "token_file": "mac-studio-midi.token",
      "zones": ["living-room"],
      "capabilities": ["midi.input"],
      "publishes": ["midi.packet.v1"],
      "publish_producers": ["home-pi-midi"]
    },
    {
      "producer_id": "home-pi-midi",
      "token_file": "home-pi-midi.token",
      "zones": ["living-room"],
      "capabilities": ["midi.output"],
      "publishes": []
    }
  ]
}
```

Merge these entries into the existing `routes` and `nodes` arrays rather than
replacing them. The explicit destination and publish grant prevent an input
edge from becoming a global broadcast or feeding itself.

## Mac Studio input

```json
{
  "bus": {
    "installation_id": "home",
    "producer_id": "mac-studio-midi",
    "hub_endpoint": "127.0.0.1:18791",
    "token_env": "UNIBUS_MIDI_TOKEN",
    "zones": ["living-room"],
    "capabilities": ["midi.input"],
    "schema_version": 2,
    "read_timeout_ms": 1000
  },
  "device": {
    "direction": "input",
    "port": "*",
    "destination": "home-pi-midi"
  },
  "queue_capacity": 1024
}
```

## Raspberry Pi Scarlett output

```json
{
  "bus": {
    "installation_id": "home",
    "producer_id": "home-pi-midi",
    "hub_endpoint": "127.0.0.1:18790",
    "token_env": "UNIBUS_MIDI_TOKEN",
    "zones": ["living-room"],
    "capabilities": ["midi.output"],
    "schema_version": 2,
    "read_timeout_ms": 1000
  },
  "device": { "direction": "output", "port": "FLUID Synth" }
}
```

These examples put the hub and output edge on the Pi. The Mac endpoint is a
local SSH forward to the Pi's loopback-only hub:

```sh
ssh -N -L 127.0.0.1:18791:127.0.0.1:18790 ajm@home-pi
```

Install that tunnel as a supervised service rather than opening the plaintext
hub listener on Wi-Fi. `FLUID Synth` is the ALSA sequencer port exposed by the
local piano sampler; use `Scarlett 18i20` instead to drive physical MIDI DIN.

Start the output edge first, then the input edge:

```sh
unibus-midi /etc/unibus/midi-output.json
unibus-midi /etc/unibus/midi-input.json
```

Verify note-on, note-off, sustain pedal, pitch bend, MIDI clock, and SysEx if
the keyboard emits it. Stop the Pi edge and confirm the hub reports delivery
failure rather than silently claiming the destination consumed packets.
