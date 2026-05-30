# Rust-only migration

This repository now uses Cargo as the only build and test entry point.

Removed legacy package-manager and JavaScript-tooling metadata includes:

- root Node package manifests and workspace locks (`package.json`,
  `pnpm-lock.yaml`, `pnpm-workspace.yaml`),
- example package manifests, pnpm locks, and TypeScript project configs,
- package-level Node manifests and TypeScript/Vitest/Knip configs,
- Biome formatter/linter configuration,
- Rust crate-local lockfile in favor of the workspace `Cargo.lock`.

Use these commands instead:

```bash
cargo fmt --all --check
cargo test --workspace
cargo build --workspace
```
