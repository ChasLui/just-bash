# just-bash

`just-bash` is a Rust workspace centered on a native, deterministic shell
runtime. The Rust core lives in [`packages/just-bash/rust-core`](./packages/just-bash/rust-core)
and provides:

- an in-memory filesystem,
- a quote-aware shell parser,
- a small execution engine with built-ins, pipelines, redirections, and
  environment expansion,
- default per-`exec` shell-state isolation (filesystem remains shared),
- configurable execution limits for script size, command count, and command
  substitution depth,
- a native `just-bash-rs` CLI for running snippets from argv or stdin.

Security model details: [`THREAT_MODEL.md`](./THREAT_MODEL.md).

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

Cargo is the only build, test, and formatting entry point.
