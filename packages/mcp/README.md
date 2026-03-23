# @kyora-sh/mcp

Kyora gives coding agents superhuman debugging capabilities, improving performance by 31.63% on SWE-bench Verified out of the box. It also automatically indexes your dependencies and any online or offline source, giving agents the perfect context to work with.

## Install

```bash
bunx @kyora-sh/mcp
```

## Usage with Claude Code

```json
{
  "mcpServers": {
    "kyora": {
      "command": "bunx",
      "args": ["@kyora-sh/mcp"],
      "env": {
        "KYORA_DATA_DIR": "/path/to/your/app/.kyora"
      }
    }
  }
}
```

## Instrumenting your app

Add `@kyora/sdk` to your app and call `init()`:

```ts
import { init, watch, trace } from "@kyora/sdk"

init({ dataDir: ".kyora" })
```

### watch — track state over time

Wrap any object with `watch()` to record snapshots on every mutation:

```ts
import { watch } from "@kyora/sdk"

const cart = watch({ items: [], total: 0 }, "cart")

cart.items.push({ name: "Widget", price: 9.99 })
cart.total = 9.99
// both mutations are recorded as state snapshots
```

### trace — record function calls

Wrap any function with `trace()` to record args, return values, errors, and timing:

```ts
import { trace } from "@kyora/sdk"

const fetchUsers = trace(async function fetchUsers() {
  const res = await fetch("/api/users")
  return res.json()
}, "fetchUsers")

// every call records: args, return value, duration, errors
await fetchUsers()
```

### automatic instrumentation (bun plugin)

Add kyora as a preload to auto-instrument `// @kyora.watch` and `// @kyora.trace` comments:

```toml
# bunfig.toml
preload = ["@kyora/sdk/plugin"]
```

Then just annotate your code:

```ts
// @kyora.watch
const state = { count: 0, users: [] }

// @kyora.trace
async function loadUsers() {
  const res = await fetch("/api/users")
  state.users = await res.json()
}
```

The plugin transforms this at load time — no manual wrapping needed.

### automatic capture

`init()` also patches `fetch`, `console`, and error handlers automatically:

- all `fetch()` calls are logged with method, url, status, duration, headers, body
- all `console.log/warn/error/info/debug` calls are captured
- unhandled errors and promise rejections are recorded

## MCP Tools

| Tool | Description |
|------|-------------|
| `kyora_query_state` | query state snapshots for a watched variable over time |
| `kyora_get_recent_errors` | get recent errors with stack traces |
| `kyora_get_http_log` | get recent HTTP requests and responses |
| `kyora_search_docs` | semantic search across indexed documentation |
| `kyora_list_indexed` | list all indexed documentation sources |
| `nora_index_source` | index docs from npm packages, URLs, or local files |
| `kyora_index_status` | check indexing status |

## License

[Elastic-2.0](../../LICENSE)
