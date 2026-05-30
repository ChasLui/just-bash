# just-bash

`just-bash` is now a Rust workspace centered on a native, deterministic shell
runtime. The Rust core lives in [`packages/just-bash/rust-core`](./packages/just-bash/rust-core)
and provides:

- an in-memory filesystem,
- a quote-aware shell parser,
- a small execution engine with built-ins, pipelines, redirections, and
  environment expansion,
- a native `just-bash-rs` CLI for running snippets from argv or stdin.

## Workspace layout

```text
Cargo.toml                     Rust workspace manifest
packages/just-bash/rust-core/  Rust library and CLI crate
```

## Development

```bash
cargo fmt --all --check
cargo test --workspace
cargo build --workspace
```

Run the CLI directly with Cargo:

```bash
cargo run -p just-bash --bin just-bash-rs -- 'echo hello from rust'
```

The previous Node package-manager workflow has been removed; use Cargo for all
build, test, and formatting tasks.
