# kyora

Queryable temporal runtime state for coding agents.

Records state mutations, function calls, HTTP traffic, and errors over time, then exposes it all via MCP so agents can query what actually happened at runtime. Also indexes dependencies and docs semantically, minimizing hallucinations.

## Quick start

```bash
bunx @kyora-sh/mcp
```

Add to `.claude/settings.json`:

```json
{
  "mcpServers": {
    "kyora": {
      "command": "bunx",
      "args": ["@kyora-sh/mcp"],
      "env": { "KYORA_DATA_DIR": "/path/to/your/app/.kyora" }
    }
  }
}
```

## Instrumentation

```ts
import { init, watch, trace } from "@kyora/sdk"

init({ dataDir: ".kyora" })

// track state over time
const cart = watch({ items: [], total: 0 }, "cart")
cart.items.push({ name: "Widget", price: 9.99 })

// record function calls (args, return values, errors, timing)
const fetchUsers = trace(async function fetchUsers() {
  return (await fetch("/api/users")).json()
}, "fetchUsers")
```

`init()` automatically patches `fetch`, `console`, and error handlers.

### Auto-instrumentation (Bun plugin)

```toml
# bunfig.toml
preload = ["@kyora/sdk/plugin"]
```

```ts
// @kyora.watch
const state = { count: 0, users: [] }

// @kyora.trace
async function loadUsers() {
  state.users = await (await fetch("/api/users")).json()
}
```

Transforms at load time, no manual wrapping.

## MCP tools

| Tool | Description |
|------|-------------|
| `kyora_query_state` | query state snapshots over time |
| `kyora_get_recent_errors` | recent errors with stack traces |
| `kyora_get_http_log` | HTTP requests and responses |
| `kyora_search_docs` | semantic search across indexed docs |
| `kyora_list_indexed` | list indexed documentation sources |
| `nora_index_source` | index npm packages, URLs, or local files |
| `kyora_index_status` | check indexing progress |

## Packages

```
packages/mcp   @kyora-sh/mcp   MCP server (published)
packages/sdk   @kyora/sdk      instrumentation (watch, trace, auto-patching)
packages/nora  @kyora/nora     semantic doc indexing + search (local embeddings)
packages/db    @kyora/db       embedded PostgreSQL (PGLite) + vector search
apps/eval                      demo server
```

## Development

```bash
bun install && bun run dev
```

## License

[Elastic-2.0](LICENSE)
