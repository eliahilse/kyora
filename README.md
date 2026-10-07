# kyora

kyora is a recursive agent runtime: a Rust harness whose Python REPL lets the model call itself from code, so one agent can fan a task out to many. On kyora vms each of those agents gets its own machine, which takes recursive runs from a laptop to fleet scale.

Status: early development. Design: docs/design.md.

## Build and test

```sh
cargo build
cargo test
cargo clippy --workspace --all-targets --locked -- -D warnings
```

Requires Rust stable (see rust-toolchain.toml); Python 3.9+ for the REPL tests.

Run the TUI demo: `CARGO_BUILD_JOBS=4 cargo run -p kyora-cli -- tui --demo` (see docs/tui.md).

Connect MCP servers through `[mcp.servers.<name>]` in `$KYORA_HOME/config.toml` (see docs/mcp.md).

License: Apache-2.0, see LICENSE.
