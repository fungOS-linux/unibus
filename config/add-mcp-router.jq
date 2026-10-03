def without_mcp:
  .routes |= map(select(.kind | startswith("mcp.") | not)) |
  .nodes |= map(select(
    .producer_id != "studio-mcp-provider" and
    .producer_id != "neo-mcp-provider" and
    .producer_id != "neo-mcp-bridge"
  ));

without_mcp |
.routes += [
  {"kind":"mcp.provider.v1","target":{"mode":"capability","name":"mcp.bridge"}},
  {"kind":"mcp.invoke.v1","target":{"mode":"producer"}},
  {"kind":"mcp.cancel.v1","target":{"mode":"producer"}},
  {"kind":"mcp.progress.v1","target":{"mode":"producer"}},
  {"kind":"mcp.result.v1","target":{"mode":"producer"}}
] |
.nodes += [
  {
    "producer_id":"studio-mcp-provider",
    "token_file":"tokens/studio-mcp-provider.token",
    "zones":["default"],
    "capabilities":["mcp.provider"],
    "publishes":["mcp.provider.v1","mcp.progress.v1","mcp.result.v1"],
    "publish_zones":["default"],
    "publish_producers":["neo-mcp-bridge"]
  },
  {
    "producer_id":"neo-mcp-provider",
    "token_file":"tokens/neo-mcp-provider.token",
    "zones":["default"],
    "capabilities":["mcp.provider"],
    "publishes":["mcp.provider.v1","mcp.progress.v1","mcp.result.v1"],
    "publish_zones":["default"],
    "publish_producers":["neo-mcp-bridge"]
  },
  {
    "producer_id":"neo-mcp-bridge",
    "token_file":"tokens/neo-mcp-bridge.token",
    "zones":["default"],
    "capabilities":["mcp.bridge"],
    "publishes":["mcp.invoke.v1","mcp.cancel.v1"],
    "publish_producers":["studio-mcp-provider","neo-mcp-provider"]
  }
]
