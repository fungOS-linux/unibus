# Small private IPFS swarm

Agora is the primary node; Titan is the reserve. Both use independently
initialized Kubo v0.43.1 repositories and the same random 256-bit swarm key.
The public demo node on Agora is a separate container and repository, unchanged.
Never add private content to the public demo's port 5001.

## Boundaries

- Private adapter RPC: `http://127.0.0.1:5002` on each host.
- Swarm TCP: host's private Tailscale IP, port 4002. No LAN/WAN binding.
- No host gateway, UDP listener, public bootstrap, mDNS, AutoConf, AutoTLS,
  delegated HTTP routing, delegated IPNS publishing, HTTP retrieval or relay.
- `LIBP2P_FORCE_PNET=1` and a startup key check fail closed without a swarm key.
- TCP-only private transport follows Kubo's current supported private-network
  boundary. The swarm key authenticates membership; it is not per-document ACL.
- Members can retrieve available content. Files are not encrypted at rest by
  Kubo; encrypt sensitive data before adding it if storage confidentiality is
  required. A CID is an address, not an authorization token.

Each container has one CPU, 512 MiB RAM, and automatic GC with an **8 GB soft
datastore budget**. Pinned blocks are retained through GC and can exceed that
budget. This is not a filesystem quota or automatic replication policy.

## Deployment and manual runbook

1. Use `deploy/ipfs/private-compose.yaml` and executable `private-init.sh`.
   Stage them together on each host. Validate with `sh -n private-init.sh` and
   `docker compose ... config --quiet` before starting anything.
2. Generate a new shared key once with
   `unibus-ipfs swarm-key /private/path/swarm.key`. This generates a random key,
   refuses to replace an existing file and creates it with mode 0600. Keep its
   parent directory mode 0700. Transfer only over authenticated encrypted SSH;
   never put contents in git, environment variables, Mycelium topology or logs.
3. The official image runs Kubo as UID/GID 1000. Its key bind mount must be
   readable by that identity. On Titan, the source remains owned by `avery`,
   under a mode-0700 directory, with group 1000 and mode 0440. The copied key
   inside the repository is UID/GID 1000, mode 0400. Do not make a key world
   readable to work around this boundary. A failed first init may leave a
   repository config without a key; stop the container and install the key
   into that specific repository before retrying. Do not reinitialize identity.
4. Set `PRIVATE_OVERLAY_IP` to the host's observed overlay IPv4 address and
   `PRIVATE_KEY_FILE` to its protected key path; optionally set
   `PRIVATE_STORAGE_MAX`. Start Compose project `unibus-ipfs-private`.
5. Read each peer ID from `docker exec unibus-kubo-private ipfs id -f '<id>'`.
   Configure each node's `Peering.Peers` through `ipfs config --json`, with the
   other's observed peer ID and `/ip4/OVERLAY_IP/tcp/4002` address. Restart the
   new containers. These persistent peers reconnect without public bootstrap.
6. Add content to the primary with `ipfs add`; explicitly `ipfs pin add CID` on
   the reserve. Joining alone does not replicate future additions. Keep the
   pin set small until a bounded replication owner is implemented.
7. Check health, peer IDs and pin status after restarting both containers.
   Stop the primary briefly and retrieve a pinned document from the reserve;
   validate its hash. Restore the primary and verify reconnection.

Installed locations:

| Host | Operator | Deployment | Key source |
| --- | --- | --- | --- |
| Agora | `ajmwagar` | `/home/ajmwagar/deploy/unibus-ipfs-private` | `/home/ajmwagar/.config/unibus-ipfs/private/swarm.key` |
| Titan | explicitly approved `avery` with sudo | `/home/avery/deploy/unibus-ipfs-private` | `/home/avery/.config/unibus-ipfs/private/swarm.key` |

Container and volume on each host: `unibus-kubo-private`,
`unibus-kubo-private-data`. Private deployment does not expose Kubo's admin API
to Unibus clients. The adapter only allows its bounded CID-range contract;
router grants and a persistent fleet adapter remain a separate deployment.

## Validation

The initial private deployment transferred and pinned both demo PDFs on Titan
through the private peer connection. A temporary same-key peer connected;
a temporary different-key peer was rejected in the security negotiation.
Temporary test repositories are RAM-only and are discarded after validation.
Both documents were retrieved from Titan with Agora's private container stopped;
their SHA256 hashes matched the originals. Agora was restored afterwards.
Unit/integration tests cover secure key creation, non-overwrite, range bounds,
atomic downloads and authenticated bus transfer.

R2 backup and automatic reserve replication are not configured. Back up the
shared key and node identities separately with encryption. Adding a third
node requires the same key, a new identity, private reachability and explicit
persistent peer/pin configuration—not access to the public demo repository.

## Growth ownership

Today enrollment and pins are explicit Kubo operations; neither Mycelium nor
Unibus automatically grows the swarm. Mycelium can own host discovery and
installation/update capability. A storage controller should own approved
enrollment, secure key provisioning, capacity admission and replication intent.
Unibus remains the narrow fetch/status transport. Kubo owns authenticated peer
connections, content addressing and block transfer. Discovery metadata must not
contain a swarm key or implicitly authorize enrollment. This future controller
and discovery advertisement are not implemented in the initial deployment.
