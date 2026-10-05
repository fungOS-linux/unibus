# MIDI port discovery

`unibus-midi --list-json` enumerates current OS MIDI inputs and outputs without
opening a MIDI connection. `unibus-midi-discovery <config.json>` periodically
publishes the same inventory to a configured consumer using `midi.ports.v1`.

The configuration contains `bus` (the standard authenticated Unibus client
configuration), `destination` (consumer producer ID), `interval_ms` and `ttl_ms`.
Use interval 2000 ms and TTL 6000 ms to start. Schema version 2 and the
`midi.discovery` capability are required. Router grants must independently
authorize this producer, destination and message kind. Do not reuse a broader
grant just to make discovery work.

Port IDs derive from direction and exact OS name and are scoped by producer.
They are not hardware serial numbers. Duplicate names remain visible with
`matching_ports > 1` and cannot safely be selected by name. Renaming a port
changes its ID. Device-specific drum maps and control profiles belong to the
consumer's Lua profiles, not this discovery publisher.

Consumers use `MidiInventoryRegistry::observe` with the router-authenticated
envelope producer ID. Full snapshots replace prior observations; empty snapshots
withdraw disconnected ports. `current(now)` removes expired publishers. Local
wall clocks must be synchronized: future and stale observations are rejected.
Inventory is availability, not permission to route notes or clock.

The publisher exits on enumeration failures, disconnects and negative router
replies. A service manager may restart it; it does not silently fall back or
claim that an inventory was delivered just because a socket write succeeded.
Synesthesia consumer wiring and fleet deployment are separate integration steps.
