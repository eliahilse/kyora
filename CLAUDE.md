# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

@AGENTS.md

## Commands

The CI gate:

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
```

One crate, one test target, or one test by name:

```bash
cargo test -p kyora-core
cargo test -p kyora-core --test runtime
cargo test -p kyora-core --test runtime first_final_answer_skips_remaining_tools
```

Full builds and test runs are heavy. Run them in CI or a kyora VM (`kyora-vms run -- cargo test --workspace --locked`) rather than on a laptop.

## Architecture

```
crates/protocol    provider-neutral messages, content blocks, tool calls and stream events
crates/providers   the ModelProvider trait, the Anthropic Messages API, stream accumulation, retry policy, a deterministic fake
crates/tools       built-in file, shell and workspace tools
crates/core        the runtime loop, tools and toolsets, ledger and budgets, sessions, traces
crates/cli         `kyora run` and its config (binary `kyora`)
crates/tui         the event-driven TUI with a live recursion tree
```

Tests live in `crates/<crate>/tests`. Provider-facing tests use the fake provider, so they are deterministic and need no API key; live API tests are `#[ignore]`d.
