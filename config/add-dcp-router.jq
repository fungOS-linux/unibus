def without_dcp:
  .routes |= map(select(.kind | startswith("dcp.") | not)) |
  .nodes |= map(select(.producer_id != "home-dcp-provider" and .producer_id != "radioman-dcp-provider" and .producer_id != "jarvis-dcp-client"));

without_dcp |
.routes += [
  {"kind":"dcp.catalog.v1","target":{"mode":"capability","name":"dcp.client"}},
  {"kind":"dcp.execute.v1","target":{"mode":"producer"}},
  {"kind":"dcp.receipt.v1","target":{"mode":"producer"}}
] |
.nodes += [
  {
    "producer_id":"home-dcp-provider",
    "token_file":"tokens/home-dcp-provider.token",
    "zones":["default"],
    "capabilities":["dcp.provider"],
    "publishes":["dcp.catalog.v1","dcp.receipt.v1"],
    "publish_zones":["default"],
    "publish_producers":["jarvis-dcp-client"]
  },
  {
    "producer_id":"radioman-dcp-provider",
    "token_file":"tokens/radioman-dcp-provider.token",
    "zones":["default"],
    "capabilities":["dcp.provider"],
    "publishes":["dcp.catalog.v1","dcp.receipt.v1"],
    "publish_zones":["default"],
    "publish_producers":["jarvis-dcp-client"]
  },
  {
    "producer_id":"jarvis-dcp-client",
    "token_file":"tokens/jarvis-dcp-client.token",
    "zones":["default"],
    "capabilities":["dcp.client"],
    "publishes":["dcp.execute.v1"],
    "publish_zones":["default"],
    "publish_producers":["home-dcp-provider","radioman-dcp-provider"]
  }
]
