# @kyora-sh/usage

Engine quota state and live usage probing, shared by [kyora review](../../review/cli), [kyora council](../../council/mcp) and [kyora switch](../../../apps/switch).

Two halves:

**`state`** — per-engine run history on disk (`~/.local/state/kyora-review/usage.json`, or `KYORA_REVIEW_STATE_DIR`). Records when an engine last ran and how it went, puts a rate-limited engine on a cooldown, and orders engines for rotation. `looksRateLimited` and `resetHintMs` classify a vendor's error text so a 429 becomes a cooldown instead of a hard failure.

**`quota`** — what a vendor says is left, right now. Every vendor reports quota in a different shape, so each parser is deliberately tolerant and returns the same `LiveUsage`: a remaining percentage, a one-line detail, and for Claude the individual windows with their reset times.

| function | source |
| --- | --- |
| `claudeOauthUsage(token)` | `api.anthropic.com/api/oauth/usage` — five-hour and seven-day windows |
| `parseZaiQuota` | Z.ai monitor, per-window percentages |
| `parseQuotaWindows` | Kimi and anything else exposing used/limit pairs |
| `parseTokenPlanUsage` | Bailian token-plan console credits |

`claudeAccessToken(blob)` pulls the OAuth token out of a Claude Code credentials blob, so a caller holding a stored login can ask about that account rather than the live one — which is how `kyora-switch usage` reports every saved account at once.

The tightest window always wins: `remainingPct` is what is left on the most constrained limit, not an average.

```bash
bun test
```
