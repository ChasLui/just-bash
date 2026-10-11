# just-bash

`just-bash` is a Rust workspace centered on a native, deterministic shell
runtime. The Rust core lives in [`packages/just-bash/rust-core`](./packages/just-bash/rust-core)
and provides:

- an in-memory filesystem,
- a quote-aware shell parser,
- a small execution engine with built-ins, pipelines, redirections, and
  environment expansion,
- fd-aware redirection (`n>`, `n>>`, `n<`, `n>&m` duplication such as `2>&1`,
  `n>&-` close, `/dev/null` discard),
- parameter expansion with `${p:-word}`-style operators, `${#name}`, `$#`, and
  `set --` positional assignment,
- `break`/`continue`, bundled `set -euo pipefail`, and fail-closed parse errors
  for unsupported command syntax,
- default per-`exec` shell-state isolation (filesystem remains shared),
- configurable execution limits for script size, command count, command
  substitution depth, output size, and loop iterations,
- a native `just-bash-rs` CLI for running snippets from argv or stdin.

Upstream sync status: [`UPSTREAM_SYNC.md`](./UPSTREAM_SYNC.md) records the
upstream fork point, the ported slice, and what is not yet ported.

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
