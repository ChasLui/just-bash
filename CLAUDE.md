# CLAUDE.md

This file provides guidance to Claude Code when working with this repository.

## Project Overview

`just-bash` is a **Rust-only** shell runtime. The repository is a Cargo
workspace with the implementation split across focused crates under `crates/`;
there is no Node/TypeScript build path.

- Facade crate (public API + CLI): `crates/just-bash`
- Execution core: `crates/just-bash-shell`
- Public crate: `crates/just-bash/Cargo.toml` (`name = "just-bash"`)
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

The behavior lives in focused crates under `crates/`:

- `crates/just-bash/src/lib.rs` re-exports the implementation crates and the
  public API.
- `crates/just-bash/src/main.rs` provides the CLI entrypoint.
- `crates/just-bash-parser/src/lib.rs` holds the parser (lexer, word model,
  redirection/statement parsing).
- `crates/just-bash-shell/src/lib.rs` holds expansion, execution and built-ins.
- `crates/just-bash-fs/src/lib.rs` holds the in-memory filesystem.

## Development Rules

- Core behavior lives in the owning crate under `crates/`: `just-bash-shell` for
  execution, `just-bash-parser` for parsing, `just-bash-fs` for filesystem, and
  the matching `just-bash-*` crate for each builtin (`awk`, `jq`, `sed`,
  `regex`, `glob`, `net`).
- Keep the `just-bash` facade in sync when adding a crate or changing an export.
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
