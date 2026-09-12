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

Requires [Bun](https://bun.sh). The tool runs on Bun's shell and file APIs, so `bun` has to be on your PATH. `npx` will not work.

```bash
bun add -g kyora-switch
kyora-switch status
```

Or run it without installing anything:

```bash
bunx kyora-switch status
```

macOS is the primary target: Claude Code keeps its OAuth blob in the login keychain there, and switch reads and writes it the same way. On Linux both CLIs keep credentials in files, which switch handles natively. Windows is untested.

From a checkout, the entry point carries a `#!/usr/bin/env bun` shebang, so a symlink from anywhere on your PATH is the whole install:

```bash
ln -s "$PWD/packages/switch/cli/src/index.ts" ~/.local/bin/kyora-switch
```

The link points at the checkout, so the tool runs whatever branch you have out. Or skip the install and run `bun packages/switch/cli/src/index.ts`.

## Commands

Every provider takes the same verbs, under `kyora-switch claude …` or `kyora-switch codex …`:

| command | what it does |
| --- | --- |
| `save <slot>` | store the account that provider is logged into right now |
| `load <slot>` | log that provider back into a stored account |
| `list` | slots for that provider, with the live one marked |
| `usage` | how much quota each stored account has left |
| `sync` | copy the live login back into its slot |
| `clear` | sign out locally, without revoking the account |
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

The account that is live is always probed with its live credentials, which the CLI keeps refreshed. Other slots are probed with the access token frozen into them at save time, and Claude's expire after about eight hours, so those go quiet by the next day. `refresh` fixes that:

```bash
kyora-switch claude refresh    # renew stale slots, then show usage
kyora-switch usage --refresh   # same, as part of a normal usage read
```

It makes the call the CLI itself makes when its token ages out: `POST /v1/oauth/token` with `grant_type=refresh_token`. The refresh token rotates, so the new credentials are written straight back into the slot — a spent token is never left behind. Only slots that are *not* live are touched: the live account belongs to the CLI, which refreshes it under its own lock.

A refresh does not extend the session. The refresh token expiry is absolute, roughly four weeks from the login that created it, and comes back unchanged. Once past that, the slot needs a real login.

Codex access tokens last around ten days, so its slots keep reporting without any of this.

The probing and cooldown logic is [`@kyora-sh/usage`](../../packages/shared/usage), shared with kyora review and council so all three read quota the same way.

## Slots never fall behind the CLI

Claude Code rotates its own tokens every few hours and writes them straight to the keychain. A slot saved in the morning holds the morning's token by the evening — still loadable, since the refresh token survives rotation, but stale enough that `usage` had nothing to read and a switch away froze the slot on an old credential.

So every `status`, `list`, `usage` and `load` first copies the live login back into whichever slot holds the same account. `load` does it before switching away, which is the moment that matters: the account you are leaving gets its freshest token saved before its keychain entry is overwritten. `sync` runs that step on its own.

It is a local file copy, nothing more — no network, no keychain write, and a slot that already matches is left untouched so its timestamps do not churn. Slots for other accounts are never involved.

## Signing out without losing the account

`codex logout` and Claude's `/logout` end the account's session server side, which also invalidates the credentials sitting in your saved slots. That is the wrong tool when all you want is a free slot to log a second account into.

```bash
kyora-switch codex clear     # remove the local credentials, nothing else
codex login                  # now log in as someone else
```

`clear` removes only what is on this machine: `~/.codex/auth.json` for Codex, and for Claude Code the OAuth blob plus the `oauthAccount` key. It makes no network call, so every slot you saved earlier still loads. The outgoing login is backed up first, exactly like `load` does.

`config.toml`, project history, settings and MCP config are untouched.

## What actually gets swapped

Only the credentials and the account they belong to. Session history, project settings, and MCP config stay where they are.

**Claude Code**

- the OAuth blob, from the macOS login keychain (service `Claude Code-credentials`) or `~/.claude/.credentials.json` where there is no keychain
- `oauthAccount` in `~/.claude.json`, and nothing else in that file — `userID` and `machineID` identify the install, not the account, so they stay put

Nothing else is touched, which is the same surface `/login` changes when you sign in as a different account. In particular `policy-limits.json` and `remote-settings.json` are left alone: the second one carries your org's plugin and marketplace config, and an earlier version of this tool deleted it on every switch. The entitlement caches in `~/.claude.json` are left alone too — the CLI refetches them for whoever is logged in.

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

## License

[Apache-2.0](https://github.com/eliahilse/kyora/blob/main/packages/switch/cli/LICENSE). The rest of the kyora repo is [Elastic-2.0](https://github.com/eliahilse/kyora/blob/main/LICENSE); this package is the exception.
