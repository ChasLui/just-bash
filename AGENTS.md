# AGENTS.md

This file provides execution guidance for Codex (Codex.ai/code) in this repository.

## Project Overview

`just-bash` is a **Rust-only** shell runtime:

- Core runtime: `packages/just-bash/rust-core`
- Crate: `just-bash`
- CLI binary: `just-bash-rs`

There is no Node/TypeScript build path; Cargo is the only entry point.

## Primary Commands

```bash
# Workspace checks
cargo fmt --all
cargo check --workspace --all-targets
cargo clippy --workspace --all-targets

# Tests + build
cargo test --workspace
cargo build --workspace

# Optional run
cargo run -p just-bash --bin just-bash-rs -- 'echo hello from rust'
```

## Execution Rules for Agents

- All core changes go in `packages/just-bash/rust-core`.
- Do not reintroduce Node/TypeScript runtime entrypoints, manifests, or build tooling.
- When adding new behavior, prefer explicit limits and deterministic outputs.
- Avoid introducing runtime dependencies not already approved by core architecture.
