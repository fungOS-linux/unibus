#!/bin/sh
set -eu

# Kubo runs this before starting its daemon. Missing-key startup is fatal.
if ! test -s /data/ipfs/swarm.key; then
    echo 'private IPFS startup refused: missing or unreadable swarm.key' >&2
    exit 1
fi
test -n "${PRIVATE_OVERLAY_IP:?private overlay IP required}"
ipfs config --json AutoConf.Enabled false
ipfs config --json AutoTLS.Enabled false
ipfs config --json DNS.Resolvers '{}'
ipfs config --json Bootstrap '[]'
ipfs config Routing.Type dht
ipfs config --json Routing.DelegatedRouters '[]'
ipfs config --json Ipns.DelegatedPublishers '[]'
ipfs config --json HTTPRetrieval.Enabled false
ipfs config --json Discovery.MDNS.Enabled false
ipfs config --json Swarm.RelayClient.Enabled false
ipfs config --json Swarm.RelayService.Enabled false
ipfs config --json Swarm.DisableNatPortMap true
# The server profile blocks RFC1918/CGNAT addresses; our overlay is private.
# Membership is still enforced by the swarm key and overlay access policy.
ipfs config --json Swarm.AddrFilters '[]'
ipfs config --json Swarm.Transports.Network.Websocket false
ipfs config --json Swarm.Transports.Network.QUIC false
ipfs config --json Swarm.Transports.Network.WebTransport false
ipfs config --json Swarm.Transports.Network.WebRTCDirect false
ipfs config --json Addresses.Swarm '["/ip4/0.0.0.0/tcp/4001"]'
ipfs config --json Addresses.Announce "[\"/ip4/${PRIVATE_OVERLAY_IP}/tcp/4002\"]"
ipfs config Datastore.StorageMax "${PRIVATE_STORAGE_MAX:-8GB}"
