# MQTT bridge runbook

`unibus-mqtt` connects a local MQTT 3.1.1 broker to every other bridge in the
same namespace. MQTT clients remain ordinary clients of their nearest broker:

```text
client -> broker A -> adapter A -> Unibus -> adapter B -> broker B -> client
```

The adapter preserves the publication topic, QoS, retain flag, and binary
payload. MQTT sessions, retained-message storage, and protocol acknowledgements
terminate at each broker. Successful local MQTT acknowledgement does not assert
that a remote subscriber consumed the publication.

## Hub configuration

Route the opaque bridge contract to every live bridge and grant each adapter
permission to publish it:

```json
{
  "routes": [
    {
      "kind": "mqtt.publish.v1",
      "target": { "mode": "any_holder", "name": "mqtt.bridge" }
    }
  ],
  "nodes": [
    {
      "producer_id": "mqtt-west",
      "token_file": "mqtt-west.token",
      "zones": ["default"],
      "capabilities": ["mqtt.bridge"],
      "publishes": ["mqtt.publish.v1"]
    },
    {
      "producer_id": "mqtt-east",
      "token_file": "mqtt-east.token",
      "zones": ["default"],
      "capabilities": ["mqtt.bridge"],
      "publishes": ["mqtt.publish.v1"]
    }
  ]
}
```

Merge these entries into the installation's existing `routes` and `nodes`
arrays. Each `producer_id`, MQTT `client_id`, and token must be unique.

## Adapter configuration

Create one configuration beside each broker:

```json
{
  "namespace": "building-a",
  "mqtt": {
    "host": "127.0.0.1",
    "port": 1883,
    "client_id": "mqtt-west",
    "tls": false,
    "username_env": "MQTT_USERNAME",
    "password_env": "MQTT_PASSWORD",
    "keep_alive_secs": 30
  },
  "bus": {
    "installation_id": "home",
    "producer_id": "mqtt-west",
    "hub_endpoint": "127.0.0.1:18790",
    "token_env": "UNIBUS_MQTT_TOKEN",
    "zones": ["default"],
    "capabilities": ["mqtt.bridge"],
    "schema_version": 2,
    "read_timeout_ms": 1000
  },
  "subscriptions": [
    { "filter": "sensors/#", "qos": "at_least_once" },
    { "filter": "commands/+/set", "qos": "at_least_once" }
  ],
  "queue_capacity": 128,
  "echo_capacity": 4096,
  "echo_ttl_ms": 10000
}
```

Omit both credential environment fields when the broker does not authenticate.
Set `tls` for server-authenticated TLS using the host's native root store.
Private certificate authorities are not silently accepted; add them to the
host trust store before enabling the adapter.

Start the bridge:

```sh
unibus-mqtt /etc/unibus/mqtt-bridge.json
```

## Verification

With adapters running at two brokers:

1. Subscribe at broker B to `sensors/test`.
2. Publish a binary or text payload to `sensors/test` at broker A.
3. Confirm broker B receives the same payload, QoS, and retain flag.
4. Repeat from B to A.
5. Stop adapter B, publish at A, and confirm no claim of remote delivery is
   made. Restart B and verify behavior follows broker session policy.
6. Publish once and confirm neither broker echoes the publication repeatedly.

MQTT 3.1.1 has no portable origin metadata. Before publishing an inbound
Unibus frame locally, the adapter records a bounded, expiring fingerprint and
consumes the broker echo once. Identical legitimate local publications inside
that short window can collide; use disjoint ingress/egress topic prefixes when
that distinction is critical. The Unibus message also carries immutable
`origin` and `message_id` fields for bridge-level loop prevention and future
MQTT 5 property propagation.
