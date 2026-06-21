# just-bash (Rust) threat model

This repository executes untrusted shell scripts in-process. The primary design
goal is deterministic behavior with conservative resource limits.

## Threat actors

1. Untrusted script author: controls script source and attempts sandbox escape,
   denial-of-service, or data exfiltration.
2. Malicious input data: controls stdin/file contents and attempts parser or
   expansion abuse.

## Trust boundary

- Trusted: host process embedding this crate and Rust standard library.
- Untrusted: script text, command arguments, redirected file contents.

## Current defenses

1. In-memory filesystem only (`InMemoryFs`), no host filesystem writes/reads.
2. Explicit parser + executor errors for malformed syntax and invalid file ops.
3. Execution limits (configurable via `BashExecutionLimits`):
   - script size ceiling,
   - total command count ceiling,
   - command substitution nesting ceiling.
4. Default `exec()` shell-state isolation: environment and cwd reset to
   configured initial values for each call (filesystem remains shared).

## Residual risks and current scope

1. This runtime is not a full Bash implementation and intentionally supports a
   subset of syntax and builtins.
2. There is no real-FS/network/process spawning surface in this Rust core.
3. Additional limits (for example output size or per-command CPU time) are not
   yet implemented and should be added before exposing this runtime to hostile,
   high-throughput multi-tenant workloads.
