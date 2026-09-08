# kyora switch

Hot-swap Claude Code and Codex logins. Save the account you are logged into as a named slot, then load it back later — no logging out, no re-auth, no browser round trip.

Built for the work-account / personal-account shuffle: two subscriptions, one laptop.

```bash
claude                        # /login as account 1
kyora-switch claude save work

claude                        # /login as account 2
kyora-switch claude save private

kyora-switch claude load work        # back to account 1
kyora-switch claude load private     # and back again
```

Codex works the same way, with its own slots:

```bash
codex login
kyora-switch codex save work
kyora-switch codex load private
```

## Install

```bash
ln -s "$PWD/apps/switch/src/index.ts" ~/.local/bin/kyora-switch
```

The entry point carries a `#!/usr/bin/env bun` shebang, so a symlink from anywhere on your PATH is the whole install. `bun link` also works, but only creates the shim once Bun has a global package.json to hang it on.

The link points at the checkout, so the tool runs whatever branch you have out. Or skip the install and run `bun apps/switch/src/index.ts`.

## Commands

Every provider takes the same verbs, under `kyora-switch claude …` or `kyora-switch codex …`:

| command | what it does |
| --- | --- |
| `save <slot>` | store the account that provider is logged into right now |
| `load <slot>` | log that provider back into a stored account |
| `list` | slots for that provider, with the live one marked |
| `usage` | how much quota each stored account has left |
| `rm <slot>` | delete a slot |
| `rename <old> <new>` | rename a slot |

Two commands span both:

| command | what it does |
| --- | --- |
| `status` | which account each CLI is logged into, and which slot it came from |
| `usage` | remaining quota across every stored account, both providers |
| `doctor` | where each CLI keeps its auth on this machine, and what it reads back |

Options: `--json` for `list`, `status` and `usage`, `-y` to skip the `rm` confirmation.

Slots are namespaced per provider, so `claude/work` and `codex/work` are independent — you can save one without touching the other.

## Which account has room left

`usage` answers the question you actually have before switching. It probes each stored account with that slot's own token, so you see every account at once rather than only the one you are logged into:

```
$ kyora-switch usage
claude — Claude Code
* work     you@work.dev · Acme · max
           38% left   session 50% used, resets in 3h 33m · weekly 32% used, resets in 4d · weekly Fable 62% used, resets in 4d
  private  you@home.dev · max
           9% left    session 91% used, resets in 38m · weekly 44% used, resets in 3d

codex — Codex
* work     you@work.dev · pro
           26% left   7d 74% used, resets in 6d · GPT-5.3-Codex-Spark 5h 0% used, resets in 5h
```

`*` marks the account that is live, and the percentage is what is left on the tightest window — including the per-model ones, which are often the binding limit long before the plan-wide window is.

| provider | source |
| --- | --- |
| Claude Code | `api.anthropic.com/api/oauth/usage`, the endpoint `/usage` reads |
| Codex | `chatgpt.com/backend-api/codex/usage`, the endpoint `/status` reads |

Claude's payload carries a `limits` array covering the session window, the plan-wide weekly window, and a weekly window per model — that last one is where a `weekly Fable` or `weekly Opus` limit shows up, and it is easy to be near it while the plan-wide number still looks comfortable. Codex reports its plan window plus any model-scoped limits the account has.

A slot whose access token has gone stale reports nothing until you load it and start the CLI once, which refreshes it.

The probing and cooldown logic is [`@kyora-sh/usage`](../../packages/shared/usage), shared with kyora review and council so all three read quota the same way.

## What actually gets swapped

Only the credentials and the account they belong to. Session history, project settings, and MCP config stay where they are.

**Claude Code**

- the OAuth blob, from the macOS login keychain (service `Claude Code-credentials`) or `~/.claude/.credentials.json` where there is no keychain
- `oauthAccount` in `~/.claude.json`, and nothing else in that file — `userID` and `machineID` identify the install, not the account, so they stay put
- `policy-limits.json` and `remote-settings.json`
- entitlement caches (`modelAccessCache`, `hasAvailableSubscription`, `orgModelDefaultCache` and friends) are dropped so the incoming account refetches its own plan and limits instead of showing the outgoing account's

**Codex**

- `~/.codex/auth.json`. `config.toml` is configuration, not auth, so it stays.

## Verified against the real CLIs

The write paths were read out of the shipped `claude` and `codex` binaries rather than guessed:

- Claude Code reads with `security find-generic-password -a <user> -w -s "Claude Code-credentials"` and writes with `add-generic-password -U -a <user> -s <service> -X <hex>`, piped through `security -i` so the secret never lands in process arguments. `security -i` silently truncates past 4096 characters, so both it and Claude Code fall back to plain argv for larger blobs. This does the same.
- `security -w` returns the value as hex whenever it holds a non-ASCII or control byte, which reads decode.
- The plaintext fallback is `<config dir>/.credentials.json` at mode `0600`, matching the `384` the binary chmods to.
- Codex keeps everything in `auth.json`; `id_token`, `access_token`, `refresh_token`, `account_id`, `last_refresh` and `auth_mode` all live there and nowhere else.

A save-then-load round trip on a real login returns the keychain blob byte for byte, with `~/.claude.json` keeping its projects, counters and install IDs.

## How hot is "hot"

The swap itself is instant, but a CLI already running has its token in memory. Restart `claude` or `codex` after loading a slot — `load` tells you when it finds one running.

## Safety

- every `load` copies the outgoing login to `~/.kyora/switch/backups/<provider>/<timestamp>/` first, so a switch made before you saved the current account is still recoverable — the last 10 per provider are kept
- `load` warns when the login it just replaced was in no slot
- slots and backups are written `0600` under `0700` directories, and restoring a credentials file tightens its mode to `0600` even if it was laxer
- files are replaced atomically, so a running CLI never reads a half-written config

Slots are plaintext credentials on disk, exactly like the files they came from. They are as sensitive as the logins themselves — do not sync `~/.kyora/switch` anywhere.

## Environment

| variable | effect |
| --- | --- |
| `KYORA_SWITCH_DIR` | where slots and backups live (default `~/.kyora/switch`) |
| `KYORA_SWITCH_NO_KEYCHAIN=1` | keep Claude credentials in files only, never the keychain |
| `CLAUDE_CONFIG_DIR` | respected, same as Claude Code reads it |
| `CODEX_HOME` | respected, same as Codex reads it |

## Development

```bash
bun test
bun run check-types
```

Tests never touch the real keychain or your real logins — they run against temp directories, with keychain access disabled by a preload (`src/test-setup.ts`).
