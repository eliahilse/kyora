# kyora

Tools that make coding agents trustworthy — by grounding them in what actually happens: at runtime, and in review.

Two products, one repo:

## kyora state — queryable temporal runtime state

Records state mutations, function calls, HTTP traffic, and errors over time, then exposes it all via MCP so agents can query what actually happened at runtime. Also indexes dependencies and docs semantically, minimizing hallucinations.

```bash
bunx @kyora-sh/mcp
```

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

Instrument with `@kyora/sdk` (`watch`, `trace`, auto-patching of fetch/console/errors, Bun plugin for annotation-driven instrumentation) and query it back through MCP tools (`kyora_query_state`, `kyora_get_recent_errors`, `kyora_get_http_log`, semantic doc search). Full docs: [`packages/state`](packages/state).

## kyora review — multi-engine code review on your own subscriptions

Codex, Claude Code, Kimi, Grok, and Qwen reviewing the same PR together — each vendor's own CLI, headless, under your login, inside the checkout so it can verify claims against real code. Findings merge with consensus ranking; single-engine findings can be adversarially cross-verified by another engine before they reach you.

```bash
bunx @kyora-sh/review doctor          # which engines are ready
bunx @kyora-sh/review --pr 123 --post # panel-review a PR
```

CI: one workflow + your subscription tokens as repo secrets, auto-refreshed. See [`action/README.md`](action/README.md) and [`packages/review/cli`](packages/review/cli).

## Layout

```
packages/state/sdk      @kyora/sdk        instrumentation (watch, trace, auto-patching)
packages/state/mcp      @kyora-sh/mcp     MCP server (published)
packages/state/nora     @kyora/nora       semantic doc indexing + search (local embeddings)
packages/state/db       @kyora/db         embedded PostgreSQL (PGLite) + vector search
packages/review/cli     @kyora-sh/review  multi-engine review CLI (published)
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
