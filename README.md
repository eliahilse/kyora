# kyora

Tools that make coding agents trustworthy — by grounding them in what actually happens: at runtime, and in review.

[kyora.sh](https://kyora.sh)

## Contents

- [kyora state — queryable temporal runtime state](#kyora-state)
  - [Quick start](#quick-start)
  - [Instrumentation](#instrumentation)
  - [Auto-instrumentation (Bun plugin)](#auto-instrumentation-bun-plugin)
  - [MCP tools](#mcp-tools)
- [kyora review — multi-engine code review](#kyora-review)
  - [Quick start](#quick-start-1)
  - [How findings are ranked](#how-findings-are-ranked)
  - [Engines](#engines)
  - [CI](#ci)
  - [Configuration](#configuration)
- [kyora council — summon other model families](#kyora-council)
- [Repo layout](#repo-layout)
- [Development](#development)
- [License](#license)

## kyora state

Queryable temporal runtime state for coding agents. Records state mutations, function calls, HTTP traffic, and errors over time, then exposes it all via MCP so agents can query what actually happened at runtime — instead of guessing from source. Also indexes dependencies and docs semantically, minimizing hallucinations.

### Quick start

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

### Instrumentation

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

### MCP tools

| Tool | Description |
|------|-------------|
| `kyora_query_state` | query state snapshots over time |
| `kyora_get_recent_errors` | recent errors with stack traces |
| `kyora_get_http_log` | HTTP requests and responses |
| `kyora_search_docs` | semantic search across indexed docs |
| `kyora_list_indexed` | list indexed documentation sources |
| `nora_index_source` | index npm packages, URLs, or local files |
| `kyora_index_status` | check indexing progress |

## kyora review

Multi-engine AI code review on the coding-agent subscriptions you already pay for. Codex, Claude Code, Kimi, Grok, and Qwen review the same diff independently — each vendor's own CLI running headless *inside your checkout*, under your own login, so it can grep callers and read tests instead of guessing from the diff. Findings are merged with consensus ranking and optional adversarial cross-verification, then land as one PR review with inline comments.

No hosting, no accounts, no middleman keys: every engine is the vendor's own CLI authenticated with your subscription.

### Quick start

```bash
bunx @kyora-sh/review doctor    # which engines are ready on this machine
bunx @kyora-sh/review           # review your branch (diff vs main)
```

Review a PR and post the results:

```bash
bunx @kyora-sh/review review --pr 123 --post --verify
```

### How findings are ranked

- **consensus** — two or more engines independently flagged the same issue. Rare and almost always real.
- **verified** — one engine flagged it, a *different* engine was asked to refute it against the actual code, and couldn't (`--verify`).
- **single** — one engine, unchallenged. Refuted findings are dropped entirely.

Findings cluster by file, overlapping lines, and description similarity, so five engines saying the same thing arrive as one finding with five votes — not five comments.

### Engines

| id | runs | auth |
| --- | --- | --- |
| `codex` | `codex exec` (read-only sandbox) | `codex login` (ChatGPT sub) or `OPENAI_API_KEY` |
| `claude` | `claude -p` (probes allowed, writes and CI suites denied) | `claude` login or `CLAUDE_CODE_OAUTH_TOKEN` |
| `kimi` | Claude Code against Kimi's Anthropic-compatible endpoint | `KIMI_API_KEY` |
| `glm` | GLM-5.2 via Claude Code against Z.ai's Anthropic-compatible endpoint | `ZAI_API_KEY`, or picked up from `opencode auth login` |
| `grok` | `grok -p` (grok-4.5, high reasoning effort) | `grok login` (SuperGrok / X Premium+) or `GROK_API_KEY` |
| `qwen` | Qwen (default qwen3.8-max-preview) via Claude Code against the Token Plan Anthropic endpoint | `QWEN_API_KEY` or picked up from `bl config agent` |

Every available engine runs by default; pick explicitly with `--engines codex,kimi`. An engine that's missing, rate-limited, or failing drops out and the review still lands with the rest. If *every* engine fails, the run exits non-zero instead of reporting a clean review.

Engines may execute small targeted probes (snippet evaluation, single-function checks) to verify a suspicion, but never the repo's test suites, builds, or type checks — those are auto-detected from `.github/workflows/*` and declared off-limits, since CI runs them anyway. Writes, installs, and history-mutating git commands are denied.

### CI

One workflow plus your subscription tokens as repo secrets. Rotating credentials (Codex) are persisted via `actions/cache`, so you seed once and they keep themselves alive:

```yaml
- uses: eliahilse/kyora/action@main
  with:
    verify: "true"
  env:
    CODEX_AUTH_JSON: ${{ secrets.KYORA_CODEX_AUTH }}
    CLAUDE_CODE_OAUTH_TOKEN: ${{ secrets.KYORA_CLAUDE_TOKEN }}
    KIMI_API_KEY: ${{ secrets.KYORA_KIMI_KEY }}
```

Full setup, secret-seeding commands, and security notes: [`action/README.md`](action/README.md).

### Configuration

`kyora-review.config.json` at the repo root sets defaults (`engines`, `verify`, `post`, `base`, `failOn`, `maxDiffBytes`, `timeoutMs`), plus per-engine overrides — `bin`, `args` (with `{prompt}`/`{schema}`/`{schemaJson}`/`{out}` tokens), `argsChat`, `argsWrite`, and `env` — so a vendor CLI changing its flags is a config edit, not a code change. The same overrides apply to [kyora council](#kyora-council), which spawns the same CLIs. CLI reference: [`packages/review/cli`](packages/review/cli).

## kyora council

The same engine pool, pointed at your own work instead of a diff. Your agent is one lineage with one set of blind spots; the council MCP server lets it ask the others — before it commits to something expensive to undo — and delegate work to them on separate subscriptions.

```json
{
  "mcpServers": {
    "kyora-council": { "command": "bunx", "args": ["@kyora-sh/council"] }
  }
}
```

`council_convene` puts the same question to several families at once and returns their independent takes; `agent_spawn` hands a task to whichever capable family has the most quota left; `agent_fanout` runs different tasks across different families at once. Engine auth and `kyora-review.config.json` overrides are shared with kyora review, so both halves see the same engines configured the same way.

There is also an optional `PostToolUse` hook that watches for genuinely high-stakes moments — migrations, auth changes, irreversible operations — with a cheap model, and reminds the agent that a second lineage is available. Details: [`packages/council/mcp`](packages/council/mcp).

## Repo layout

```
packages/state/sdk      @kyora/sdk        instrumentation (watch, trace, auto-patching)
packages/state/mcp      @kyora-sh/mcp     MCP server (published)
packages/state/nora     @kyora/nora       semantic doc indexing + search (local embeddings)
packages/state/db       @kyora/db         embedded PostgreSQL (PGLite) + vector search
packages/review/cli     @kyora-sh/review  multi-engine review CLI (published)
packages/council/mcp    @kyora-sh/council cross-family council + subagents over MCP
packages/tooling/*                        shared eslint/tsconfig
action/                                   GitHub Action for kyora review
apps/reckon                               SWE-bench-style eval harness
apps/test                                 demo server
```

## Development

```bash
bun install && bun run dev
```

## License

[Elastic-2.0](LICENSE)
