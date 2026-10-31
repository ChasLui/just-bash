# just-bash

`just-bash` is a Rust workspace centered on a native, deterministic shell
runtime. The implementation is split across focused crates under [`crates/`](./crates),
with [`crates/just-bash`](./crates/just-bash) as the facade crate that exposes the
public API and the CLI. Together they provide:

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

### Byte-exact output

The output pipeline preserves raw bytes end to end, so non-UTF-8 / high-byte
data survives command output, pipes, redirections and the in-memory filesystem.
`ExecResult.stdout` and `ExecResult.stderr` are now `Vec<u8>`; use the lossy
`stdout_text()` / `stderr_text()` helpers when you need a `String`. This is a
breaking change in **v4.0.0**. Note that pure text filters (`awk`, `jq`, `sed`,
`grep`, `sort`, …) still decode their input as UTF-8 at their boundary, so bytes
flowing *through* them degrade to U+FFFD.

Upstream sync status: [`UPSTREAM_SYNC.md`](./UPSTREAM_SYNC.md) records the
upstream fork point, the ported slice, and what is not yet ported.

Security model details: [`THREAT_MODEL.md`](./THREAT_MODEL.md).

## Workspace layout

```text
Cargo.toml                 workspace manifest
crates/just-bash/          facade crate: public API (`just_bash`) + `just-bash-rs` CLI
crates/just-bash-shell/    execution engine (built-ins, pipelines, jobs)
crates/just-bash-parser/   quote-aware shell parser
crates/just-bash-fs/       in-memory filesystem
crates/just-bash-glob/     glob matching and pathname expansion
crates/just-bash-regex/    POSIX ERE engine
crates/just-bash-awk/      awk implementation
crates/just-bash-jq/       jq implementation
crates/just-bash-sed/      sed implementation
crates/just-bash-net/      curl transport layer
```

`just-bash` re-exports every implementation crate as a module, so
`just_bash::shell::Bash`, `just_bash::parser::parse_script` and the root
re-exports such as `just_bash::Bash` all remain available.

The runtime is dependency-free: every crate uses only the Rust standard
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
