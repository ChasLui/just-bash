# Agent instructions

This package is Rust-first.

Active runtime is implemented in:
- `packages/just-bash/rust-core` (source code and CI)
- `just_bash` crate and `just-bash-rs` binary

Legacy TypeScript implementation is preserved only for reference at:
- `packages/just-bash/legacy-ts/`

## Scope and editing rule

- Prefer Rust changes in `rust-core/` for feature work, fixes, or refactors.
- Treat `legacy-ts/` as historical context; only touch it when explicitly maintaining
  migration notes or archival compatibility references.
- Keep `legacy-ts/` out of active build/run paths.

## Reference

- `packages/just-bash/README.md`
- `packages/just-bash/rust-core/README.md`
