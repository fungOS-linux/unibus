# IPFS content retrieval

`unibus-ipfs` delegates IPFS DAG/UnixFS retrieval to a trusted local Kubo node.
It does not expose Kubo's admin API, implement a gateway, pin content, or render
documents. Canvas owns PDF decoding, page navigation and physical-pixel caching.

## Contract

An explicitly authorized producer sends `unibus.ipfs.fetch.v1` with:

```json
{"cid":"QmYwAPJzv5CZsnAzt8auVZRnGi2C5pnPZGoMHfGMhL6VWJ","offset":0,"length":65536}
```

Only a bare valid CID is accepted; no URLs, IPNS or path expressions. Range
length is 1..65536 bytes and the end must not exceed 64 MiB. The adapter sends
`unibus.ipfs.content.v1` to the requesting producer, preserving request ID,
with CID, offset, actual length, EOF flag and binary payload. EOF means a short
range; a full range requires another request. Failures send
`unibus.ipfs.error.v1` with `fetch_failed` and no partial content. Details remain
in server logs. Consumers must validate sender, request ID, CID, offset and
payload length before committing content. These are not SDK JSON blob events.

The router must authorize these kinds and directed routes. The adapter's own
producer allowlist is an additional boundary, not a substitute for router
authentication. The initial implementation serializes requests and bounds each
Kubo operation to twenty seconds; it is not a parallel bulk-transfer engine.
Each of the file-stat and range HTTP requests has a ten-second deadline. The
file-stat size is checked before reading; truncated ranges cannot masquerade
as successful EOF. Unapproved producers terminate the adapter loudly. No grant
is auto-created.

## Manual runbook

1. Run a maintained Kubo daemon on the adapter host with RPC bound to loopback.
2. Provision a dedicated Unibus producer token and explicit fetch/reply routes.
3. Create a private config with `client` (the shared `ClientConfig`),
   `kubo_rpc: "http://127.0.0.1:5001"`, and `allowed_producers`.
4. Run `unibus-ipfs serve CONFIG.json`.
5. For a direct diagnostic/download, run:

   ```sh
   unibus-ipfs fetch http://127.0.0.1:5001 PDF_CID /path/to/new-document.pdf
   ```

   Downloads are staged beside the destination, synced and committed without
   replacing an existing file. Failure removes the temporary partial file.
6. Use Canvas's existing `canvas_open_file` operation on the destination host;
   navigate with `canvas_navigate`, keeping the rendered view and capture at
   the same document revision. Never hand-edit workspace JSON to show a PDF.

The diagnostic command uses local Kubo directly. For a Unibus round trip use:

```sh
unibus-ipfs download CLIENT_CONFIG.json ADAPTER_PRODUCER PDF_CID /path/to/new-document.pdf
```

The downloader validates correlated replies, range metadata and EOF, using one
outstanding range and a twenty-five-second reply deadline. A native Canvas
CID-open command is not yet implemented; use its existing file-open interface
after downloading on the destination host.

Kubo owns DAG-block verification. A UnixFS root CID is not generally the SHA256
of the returned file bytes; do not claim standalone file-hash/CID verification.
See the [official Kubo RPC documentation](https://docs.ipfs.tech/reference/kubo/rpc/).

## Deployment status

Agora runs `unibus-kubo`, Kubo v0.43.1 in Docker, with a persistent
`unibus-kubo-data` volume, restart-unless-stopped, two CPUs and 1 GiB RAM.
Only `127.0.0.1:5001` is published; no host gateway or P2P port is published.
The checked-in Compose definition is `deploy/ipfs/compose.yaml`. It describes
the existing installation; do not run a second copy alongside that container.

This is a **public-network node**, not a private swarm. Loopback RPC protects
administration, not content confidentiality. It can connect outbound to public
peers and advertise/serve stored content. Do not put private documents here.
Use a separate shared-key private swarm and/or encrypt private content first.

Two public PDFs were added, retrieved through the adapter's diagnostic command,
checked against source SHA256 hashes, then transferred through Mycelium and
opened on home-pi using the explicitly approved `ajm` Canvas session. Poppler
was missing and has been installed. The `ipfs-reading` workspace shows both
documents side-by-side; a real 1920x1080 Canvas screenshot was visually checked.

| Document | CID |
| --- | --- |
| [Attention Is All You Need](https://arxiv.org/abs/1706.03762) | `bafybeicrwkcuhyibpnbzadvjw46tl7s5pl5k5rbf4iaega4x3odats642m` |
| [Stanford HMM chapter](https://web.stanford.edu/~jurafsky/slp3/A.pdf) | `bafybeie5nzpwssrzkkou2pdky7wyo6ws4ljdrwtdbestfvpzaxz3wz5g7u` |

The Unibus serve/download round trip is integration-tested with the actual
authenticated router and adapter binaries, using a mock Kubo HTTP backend.
Fleet router grants and a persistent Unibus IPFS publisher are not enabled.
The TV has local copies, not a continuous IPFS dependency.

## Replication and backup boundaries

Additional Kubo nodes can join the public network or a separate private swarm;
joining alone does not replicate pins. Start with explicit pin replication to
a second storage node; use IPFS Cluster only if shared pin-set orchestration is
needed. Mycelium can discover nodes without owning content replication.

R2 is an S3-compatible backup destination, not an IPFS peer. Export pinned DAGs
as CAR archives, upload encrypted archives and a versioned root-CID manifest,
and test restore with CAR import plus pin validation. Back up node identity and
private swarm keys separately under encryption. R2 credentials, bucket choice,
replication and backup automation are not configured by this slice.
