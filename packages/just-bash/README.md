# just-bash

This package directory is now backed by the Rust crate in
[`rust-core`](./rust-core). The crate exposes the native shell runtime through
`just_bash` and includes the `just-bash-rs` binary.

## Build and test

From the repository root:

```bash
cargo fmt --all --check
cargo test --workspace
cargo build --workspace
```

Run a snippet:

```bash
cargo run -p just-bash --bin just-bash-rs -- 'printf "alpha\\n" | grep alpha'
```
