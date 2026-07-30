# kyora review — GitHub Action

Your own coding-agent subscriptions (Codex, Claude Code, Kimi, Grok, Qwen) reviewing every PR together. Each engine runs *inside the checkout* — it can grep callers and read tests to verify its claims — and findings are merged with consensus ranking: issues flagged by 2+ engines rank first, single-engine findings can be cross-examined by another engine (`verify: true`) before they're allowed into the review.

Nothing is hosted. Your credentials go into your repo's secrets, the engines run on the Actions runner, results land as one PR review.

## Install

`.github/workflows/review.yml`:

```yaml
name: kyora review
on: pull_request

permissions:
  contents: read
  pull-requests: write

concurrency:
  group: kyora-review-${{ github.event.pull_request.number }}
  cancel-in-progress: true

jobs:
  review:
    # secrets are unavailable to fork PRs anyway; this keeps the job green
    if: github.event.pull_request.head.repo.full_name == github.repository
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
        with:
          fetch-depth: 0
      - uses: eliahilse/kyora/action@main
        with:
          verify: "true"
        env:
          CODEX_AUTH_JSON: ${{ secrets.KYORA_CODEX_AUTH }}
          CLAUDE_CODE_OAUTH_TOKEN: ${{ secrets.KYORA_CLAUDE_TOKEN }}
          KIMI_API_KEY: ${{ secrets.KYORA_KIMI_KEY }}
          GROK_API_KEY: ${{ secrets.KYORA_GROK_KEY }}
```

Seed whichever engines you pay for — the action auto-detects which credentials are present and skips the rest. With zero secrets it no-ops with a notice, so you can merge the workflow first and add engines later.

## Seeding credentials

| engine | secret | how to get it |
| --- | --- | --- |
| codex | `KYORA_CODEX_AUTH` | log in locally, then `gh secret set KYORA_CODEX_AUTH < ~/.codex/auth.json` |
| claude | `KYORA_CLAUDE_TOKEN` | `claude setup-token`, paste into `gh secret set KYORA_CLAUDE_TOKEN` |
| kimi | `KYORA_KIMI_KEY` | API key from your Kimi membership (platform.kimi.ai) |
| grok | `KYORA_GROK_KEY` | API key from console.x.ai |

Codex is subscription-OAuth: `auth.json` holds a refresh token, and the CLI rotates the access token on every run. With `persist-auth: true` (default) the refreshed file is cached between runs and preferred over the seeded secret, so you seed **once** and it keeps itself alive. Claude's `setup-token` output is long-lived; Kimi and Grok keys don't rotate.

Every engine uses *your* account through *its vendor's own CLI* — that's what keeps this inside each provider's terms. Don't share one subscription across a team; seed per-repo with credentials of someone who'd be allowed to run that CLI locally.

## Inputs

| input | default | |
| --- | --- | --- |
| `engines` | `auto` | comma-separated ids, or auto-detect from present credentials |
| `verify` | `false` | second engine cross-examines single-engine findings; refuted ones are dropped |
| `post` | `true` | post as PR review (inline comments where the finding anchors to the diff) |
| `fail-on` | `none` | fail the job at/above a severity: `critical`, `major`, `minor`, `nit` |
| `persist-auth` | `true` | cache rotated credentials between runs |
| `version` | `latest` | `@kyora-sh/review` version |

## Security notes

- **Public repos:** Actions caches are readable by workflows in the same repo, including ones triggered from fork PRs. The `if:` guard above stops fork PRs from running this job, but if other workflows in your repo run untrusted code, set `persist-auth: "false"` and rely on the seeded secrets alone (Codex seeds then need re-seeding when the refresh token rotates out).
- The review job needs only `contents: read` + `pull-requests: write`. Engines run with read-only sandboxes/tool allowlists — they review, they don't edit.
