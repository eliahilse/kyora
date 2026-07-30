# @kyora-sh/review

Multi-engine AI code review on the coding-agent subscriptions you already pay for. Codex, Claude Code, Kimi, Grok, and Qwen review the same diff independently — each running headless *inside your checkout*, so they can grep callers and read tests instead of guessing from the diff — then findings are merged with consensus ranking and optional adversarial cross-verification.

No hosting, no accounts, no API keys of ours: every engine is the vendor's own CLI running under your login.

## Quick start

```bash
bunx @kyora-sh/review doctor    # see which engines are ready on this machine
bunx @kyora-sh/review           # review your branch (diff vs main)
```

Review a PR and post the results as a review with inline comments:

```bash
bunx @kyora-sh/review review --pr 123 --post --verify
```

## How findings are ranked

- **consensus** — two or more engines independently flagged the same issue. Rare and almost always real.
- **verified** — one engine flagged it, a *different* engine was asked to refute it against the actual code, and couldn't (`--verify`).
- **single** — one engine, unchallenged. Refuted findings are dropped entirely.

## Engines

| id | runs | auth |
| --- | --- | --- |
| `codex` | `codex exec` (read-only sandbox) | `codex login` (ChatGPT sub) or `OPENAI_API_KEY` |
| `claude` | `claude -p` (probes allowed, writes and CI suites denied) | `claude` login or `CLAUDE_CODE_OAUTH_TOKEN` |
| `kimi` | Claude Code against Kimi's Anthropic-compatible endpoint | `KIMI_API_KEY` (+ optional `KIMI_BASE_URL`, `KIMI_MODEL`) |
| `glm` | GLM-5.2 via Claude Code against Z.ai's Anthropic-compatible endpoint | `ZAI_API_KEY` or `opencode auth login` |
| `grok` | `grok -p` | `grok` login or `GROK_API_KEY` / `XAI_API_KEY` |
| `qwen` | Qwen (default qwen3.8-max-preview) via Claude Code against the Token Plan Anthropic endpoint | `QWEN_API_KEY` or `bl config agent` |

By default every available engine runs; pick explicitly with `--engines codex,kimi`. An engine that's rate-limited or fails just drops out — the review still lands with the rest.

## Options

```
--pr <number>     review a GitHub PR (resolves base, enables --post)
--base <ref>      base ref for local mode (default: main)
--engines <ids>   comma-separated engine ids (default: all available)
--verify          cross-examine single-engine findings
--post            submit as a PR review (GITHUB_TOKEN / GH_TOKEN / gh auth)
--fail-on <sev>   exit 1 at/above severity — for CI gating
--json            machine-readable output
--out <file>      also write the markdown report to a file
```

`kyora-review.config.json` at the repo root can set the same keys permanently, plus per-engine overrides (`bin`, `args` with `{prompt}`/`{schema}`/`{out}` tokens, `env`) if a vendor CLI changes its flags.

## Quota awareness

Every engine run records its outcome in local state (`~/.local/state/kyora-review/usage.json`). An engine that hits a usage limit is put on cooldown — vendor "try again in N hours" hints are parsed when present, otherwise `cooldownMinutes` (default 60) applies — and skipped on subsequent runs until it expires (`--ignore-quota` forces it). `kyora-review usage` shows the state.

`--max-engines <n>` (or `maxEngines` in config) runs only the n least-recently-used healthy engines per review, rotating load across your subscriptions instead of burning all of them on every PR. In CI the state persists between runs via the action's cache.

Engines with a vendor-side usage API additionally get a **live probe**, consulted before launching — a probed engine at 0% is skipped before spending a request, and any probe that can't run (missing auth, endpoint change) silently falls back to the cooldown mechanism:

| engine | source | auth |
| --- | --- | --- |
| `claude` | `api.anthropic.com/api/oauth/usage` (5h + 7d windows) | Claude Code's own login (file or macOS Keychain) — nothing to configure |
| `glm` | `api.z.ai/api/monitor/usage/quota/limit` (5h + weekly + monthly) | the coding-plan key already used for inference |
| `kimi` | `api.kimi.com/coding/v1/usages` (weekly + 5h) | `KIMI_API_KEY` |
| `qwen` | Bailian console token-plan endpoint | `QWEN_USAGE_COOKIE` — console session cookie, expires after days; optional |
| `codex`, `grok` | no vendor endpoint exists for subscription limits | cooldown fallback only |

## CI

Use the GitHub Action — install once, seed each subscription's token as a repo secret, and tokens that rotate (Codex) are auto-refreshed via cache: see [`action/README.md`](https://github.com/eliahilse/kyora/tree/main/action).

Part of [kyora](https://kyora.sh) · Elastic-2.0
