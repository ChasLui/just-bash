# Rust-only migration

This repository is Rust-only. Cargo is the single build and test entry point,
and the legacy TypeScript implementation has been removed entirely.

Removed in the final cleanup:

- the archived TypeScript core (`packages/just-bash/legacy-ts`),
- the TypeScript executor companion (`packages/just-bash-executor`),
- all TypeScript examples (`examples/`),
- the vendored CPython/Emscripten WASM assets used only by the TS runtime
  (`packages/just-bash/vendor`),
- Node/JavaScript release plumbing (`.changeset/`, npm package manifests,
  `CHANGELOG.md`, `.npmignore`),
- the TypeScript-specific threat model document.

The Rust core in `packages/just-bash/rust-core` is fully self-contained and does
not depend on any of the removed assets.

Use these commands:

```bash
cargo fmt --all --check
cargo test --workspace
cargo build --workspace
```
