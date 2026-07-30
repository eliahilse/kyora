# @kyora-sh/council

Summon councils of other model families from inside your coding agent.

Your agent is one lineage with one set of blind spots. This gives it a way to ask others — Codex, Claude, GLM, Grok, Kimi, Qwen — before it commits to something expensive to undo, all on the subscriptions you already pay for, all running locally as each vendor's own CLI.

## Tools

| tool | what it does |
| --- | --- |
| `council_convene` | same question to several model families at once, independent takes back |
| `council_convene_async` | same, in the background — collect later with `council_result` |
| `council_ask` | one specific family's perspective, read-only |
| `council_task` | delegate concrete work to one family (`write: true` lets it edit files) |
| `council_result` | collect a background council or task by job id |
| `council_status` | who can be summoned right now, with remaining quota |

Councils are quota-aware: members are seated highest-remaining-quota first, engines that are cooling down or exhausted are never summoned, and `size` caps a council so one question doesn't spend every subscription.

## Install

```json
{
  "mcpServers": {
    "kyora-council": { "command": "bunx", "args": ["@kyora-sh/council"] }
  }
}
```

Engine auth is shared with [`@kyora-sh/review`](../../review/cli) — run `kyora-review doctor` to see who is ready.

## High-stakes watcher (optional)

A `PostToolUse` hook that watches your agent's work with a cheap model and reminds it to consult the council at genuinely consequential moments — schema migrations, auth changes, force pushes, irreversible operations.

```json
{
  "hooks": {
    "PostToolUse": [
      { "matcher": "Write|Edit|MultiEdit|Bash",
        "hooks": [{ "type": "command", "command": "bunx @kyora-sh/council kyora-stakes-hook" }] }
    ]
  }
}
```

It is deliberately cheap and quiet: a keyword/size pre-filter runs first so most tool calls never reach a model at all, classifications are rate-limited (`KYORA_WATCHER_MIN_GAP_MS`, default 2m), nudges are rate-limited harder (`KYORA_WATCHER_NUDGE_GAP_MS`, default 15m), and every failure path stays silent so your agent is never blocked.

| env | default |
| --- | --- |
| `KYORA_WATCHER_KEY` | falls back to `ZAI_API_KEY` / `QWEN_API_KEY` |
| `KYORA_WATCHER_MODEL` | `glm-4.5-air` (z.ai) or `qwen3.6-plus` (Bailian) |
| `KYORA_WATCHER_BASE_URL` | inferred from which key is present |

Any OpenAI-compatible endpoint works — point it at whatever cheap, fast model you like.

Part of [kyora](https://kyora.sh) · Elastic-2.0
