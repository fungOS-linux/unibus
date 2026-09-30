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
      "zones": ["studio"],
      "capabilities": ["midi.input"],
      "publishes": ["midi.packet.v1"],
      "publish_producers": ["home-pi-midi"]
    },
    {
      "producer_id": "home-pi-midi",
      "token_file": "home-pi-midi.token",
      "zones": ["studio"],
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
    "hub_endpoint": "127.0.0.1:18790",
    "token_env": "UNIBUS_MIDI_TOKEN",
    "zones": ["studio"],
    "capabilities": ["midi.input"],
    "schema_version": 2,
    "read_timeout_ms": 1000
  },
  "device": {
    "direction": "input",
    "port": "keyboard",
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
    "hub_endpoint": "192.168.10.82:18790",
    "token_env": "UNIBUS_MIDI_TOKEN",
    "zones": ["studio"],
    "capabilities": ["midi.output"],
    "schema_version": 2,
    "read_timeout_ms": 1000
  },
  "device": { "direction": "output", "port": "Scarlett 18i20" }
}
```

The Pi example uses the Mac Studio's current Wi-Fi address. Prefer a stable
DHCP reservation or mesh DNS name before installing this as a service. The hub
must listen on the LAN address with `allow_insecure_lan: true`, or expose its
loopback listener through the installation's existing secure tunnel.

Start the output edge first, then the input edge:

```sh
unibus-midi /etc/unibus/midi-output.json
unibus-midi /etc/unibus/midi-input.json
```

Verify note-on, note-off, sustain pedal, pitch bend, MIDI clock, and SysEx if
the keyboard emits it. Stop the Pi edge and confirm the hub reports delivery
failure rather than silently claiming the destination consumed packets.
