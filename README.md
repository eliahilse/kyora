# kyora

Recursive agent runtime: a Rust agent harness whose Python REPL lets the model call itself from code.

Status: early development. Design: docs/design.md.

## Build and test

```sh
cargo build
cargo test
cargo clippy --workspace --all-targets --locked -- -D warnings
```

Requires Rust stable (see rust-toolchain.toml); Python 3.9+ for the REPL tests.

License: Apache-2.0, see LICENSE.
