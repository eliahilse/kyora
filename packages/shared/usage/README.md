# @kyora-sh/usage

Engine quota state and live usage probing, shared by [kyora review](../../review/cli), [kyora council](../../council/mcp) and [kyora switch](../../../apps/switch).

Two halves:

**`state`** — per-engine run history on disk (`~/.local/state/kyora-review/usage.json`, or `KYORA_REVIEW_STATE_DIR`). Records when an engine last ran and how it went, puts a rate-limited engine on a cooldown, and orders engines for rotation. `looksRateLimited` and `resetHintMs` classify a vendor's error text so a 429 becomes a cooldown instead of a hard failure.

**`quota`** — what a vendor says is left, right now. Every vendor reports quota in a different shape, so each parser is deliberately tolerant and returns the same `LiveUsage`: a remaining percentage, a one-line detail, and for Claude the individual windows with their reset times.

| function | source |
| --- | --- |
| `claudeOauthUsage(token)` | `api.anthropic.com/api/oauth/usage` — session, weekly, and per-model weekly windows |
| `codexUsage(token, account)` | `chatgpt.com/backend-api/codex/usage` — plan window plus model-scoped limits |
| `parseZaiQuota` | Z.ai monitor, per-window percentages |
| `parseQuotaWindows` | Kimi and anything else exposing used/limit pairs |
| `parseTokenPlanUsage` | Bailian token-plan console credits |

Claude's payload reports windows twice over: a set of top-level keys, and a `limits` array. Only the array carries the per-model weekly windows, so that is what gets read when present, with the top-level keys as the fallback. A `weekly Fable` window at 62% while the plan-wide weekly sits at 32% is the normal case, not an edge one.

`claudeAccessToken(blob)` and `codexCredentials(blob)` pull the credentials out of a stored login, so a caller can ask about an account it is not currently using — which is how `kyora-switch usage` reports every saved account at once.

The tightest window always wins: `remainingPct` is what is left on the most constrained limit, not an average.

```bash
bun test
```
