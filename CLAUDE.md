# CLAUDE.md

This file provides guidance to Claude Code when working with this repository.

## Project Overview

`just-bash` is a **Rust-only** shell runtime. The repository is a single Cargo
workspace; there is no Node/TypeScript build path.

- Active core: `packages/just-bash/rust-core`
- Public crate: `packages/just-bash/rust-core/Cargo.toml` (`name = "just-bash"`)
- Primary binary: `just-bash-rs`

## Command Reference

```bash
# Core
cargo fmt --all
cargo check --workspace --all-targets
cargo clippy --workspace --all-targets
cargo test --workspace
cargo build --workspace

# Example run
cargo run -p just-bash --bin just-bash-rs -- 'echo hello'

# Generate docs (when needed)
cargo doc --workspace --all-features --no-deps
```

If you need to inspect workspace wiring:

```bash
cargo metadata --format-version 1
cargo tree
```

## Local Workspace Behavior

Use `packages/just-bash/rust-core/src` as the active execution source for runtime behavior.

- `src/lib.rs` exposes the library API.
- `src/main.rs` provides the CLI entrypoint.
- `src/parser.rs`, `src/shell.rs`, and `src/fs.rs` contain the parser/shell/filesystem core.

## Development Rules

- All core behavior lives in `packages/just-bash/rust-core`.
- Do not reintroduce Node/TypeScript runtime entrypoints, package manifests, or
  build tooling.
- Run a focused verification command after any runtime change:
  - `cargo fmt --all`
  - `cargo check --workspace --all-targets`
  - `cargo test --workspace`

## Security Notes

- Treat user-provided script input as untrusted.
- Keep parser and execution limits conservative.
- Validate new filesystem path handling carefully for symlink/escape behavior.
- Do not bypass existing guardrails when touching shared filesystem abstractions.
