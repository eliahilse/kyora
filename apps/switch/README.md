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
cd apps/switch && bun link
```

That puts `kyora-switch` on your PATH. Or run it directly with `bun apps/switch/src/index.ts`.

## Commands

Every provider takes the same verbs, under `kyora-switch claude …` or `kyora-switch codex …`:

| command | what it does |
| --- | --- |
| `save <slot>` | store the account that provider is logged into right now |
| `load <slot>` | log that provider back into a stored account |
| `list` | slots for that provider, with the live one marked |
| `rm <slot>` | delete a slot |
| `rename <old> <new>` | rename a slot |

Two commands span both:

| command | what it does |
| --- | --- |
| `status` | which account each CLI is logged into, and which slot it came from |
| `doctor` | where each CLI keeps its auth on this machine, and what it reads back |

Options: `--json` for `list` and `status`, `-y` to skip the `rm` confirmation.

Slots are namespaced per provider, so `claude/work` and `codex/work` are independent — you can save one without touching the other.

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
