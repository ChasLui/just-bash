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
   Redirection targets resolve inside that filesystem, and descriptor
   duplication (`n>&m`) can only re-point one in-memory sink at another; there is
   no host file descriptor to reach.
2. Explicit parser + executor errors for malformed syntax and invalid file ops.
   A command-leading reserved word the runtime cannot execute is rejected at
   parse time, and a command substitution that fails to parse aborts the
   enclosing command, so unsupported constructs fail closed instead of running
   as external commands or continuing with an empty substitution.
3. `/dev/null` is modeled as a discard device: output redirected to it is
   dropped, input redirected from it is empty, the path is never created, and
   nothing is truncated. `n>&-` also routes a descriptor to that discard sink.
4. Execution limits (configurable via `ExecutionLimits`):
   - script size ceiling,
   - total command count ceiling,
   - command substitution nesting ceiling,
   - accumulated output byte ceiling,
   - loop iteration ceiling.
5. Default `exec()` shell-state isolation: environment and cwd reset to
   configured initial values for each call (filesystem remains shared).

## Residual risks and current scope

1. This runtime is not a full Bash implementation and intentionally supports a
   subset of syntax and builtins.
2. There is no real-FS/network/process spawning surface in this Rust core.
3. Output size, loop iterations and command count are bounded, but per-command
   CPU or wall-clock time is not. Add a time bound before exposing this runtime
   to hostile, high-throughput multi-tenant workloads.
4. Descriptor handling is narrowed to file descriptors 0-2: `n>&m` duplication is
   defined only for 1 and 2, an explicit file redirection on a larger descriptor
   is ignored rather than rejected, and `/dev/null` discards output redirection
   only. Scripts that rely on wider descriptor use are silently narrowed.
5. Fail-closed parsing is driven by a fixed list of reserved words and
   constructs. A command-position word outside that list still falls through to
   `command not found` (exit 127), and the rest of the script continues to run.
   No host command is spawned; the impact is scope confusion, not escape.

