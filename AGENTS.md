# AGENTS.md

This file provides execution guidance for Codex (Codex.ai/code) in this repository.

## Project Overview

`just-bash` is a **Rust-only** shell runtime:

- Facade crate (public API + CLI): `crates/just-bash`
- Execution core: `crates/just-bash-shell`
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

- Core changes go in the owning crate under `crates/`: `just-bash-shell` for
  execution, `just-bash-parser` for parsing, `just-bash-fs` for filesystem, and
  the matching `just-bash-*` crate for each builtin.
- Keep the public facade in `crates/just-bash` in sync when you add a new crate
  or change an exported item.
- Do not reintroduce Node/TypeScript runtime entrypoints, manifests, or build tooling.
- When adding new behavior, prefer explicit limits and deterministic outputs.
- Avoid introducing runtime dependencies not already approved by core architecture.
