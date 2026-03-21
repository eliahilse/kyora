# @kyora-sh/mcp

Kyora MCP server for coding agents. Provides observability tools and semantic doc indexing.

## Install

```bash
bunx @kyora-sh/mcp
```

## Tools

- **kyora_query_state** — query state snapshots for watched variables over time
- **kyora_get_recent_errors** — get recent errors with stack traces
- **kyora_get_http_log** — get recent HTTP requests and responses
- **kyora_search_docs** — semantic search across indexed documentation
- **kyora_list_indexed** — list all indexed documentation sources
- **nora_index_source** — index docs from npm packages, URLs, or local files
- **kyora_index_status** — check indexing status

## Usage with Claude Code

```json
{
  "mcpServers": {
    "kyora": {
      "command": "bunx",
      "args": ["@kyora-sh/mcp"]
    }
  }
}
```

## License

[Elastic-2.0](../../LICENSE)
