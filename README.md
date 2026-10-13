# just-bash

`just-bash` is a Rust workspace centered on a native, deterministic shell
runtime. The Rust core lives in [`packages/just-bash/rust-core`](./packages/just-bash/rust-core)
and provides:

- an in-memory filesystem with entry metadata (mode, size, logical mtime),
- a quote-aware shell parser covering heredocs/here-strings, `case`, `until`,
  functions, `[[ … ]]`, arrays and process substitution,
- an execution engine with built-ins, pipelines (with `PIPESTATUS` and
  consumer-bounded producers), background jobs, globbing and process
  substitution,
- a dependency-free POSIX ERE engine used by `grep -E`/`-P`, `sed` and
  `[[ =~ ]]`,
- an offline-by-default `curl` with an injectable response table,
- a native `just-bash-rs` CLI for running snippets from argv or stdin.

Upstream sync status: [`UPSTREAM_SYNC.md`](./UPSTREAM_SYNC.md) records the
upstream fork point, the ported slice, and what is not yet ported.

Security model details: [`THREAT_MODEL.md`](./THREAT_MODEL.md).

## Workspace layout

```text
Cargo.toml                     Rust workspace manifest
packages/just-bash/rust-core/  Rust library and CLI crate
```

The runtime is dependency-free: every module uses only the Rust standard
library.

## Development

```bash
cargo fmt --all --check
cargo check --workspace --all-targets
cargo clippy --workspace --all-targets
cargo test --workspace
cargo build --workspace
```

Run the CLI directly with Cargo:

```bash
cargo run -p just-bash --bin just-bash-rs -- 'echo hello from rust'
```

Cargo is the only build, test, and formatting entry point.
