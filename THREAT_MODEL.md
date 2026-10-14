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
- Untrusted: script text, command arguments, redirected file contents, and (only
  when the embedder opts in) network responses.

## Current defenses

1. In-memory filesystem only (`InMemoryFs`), no host filesystem writes/reads.
   Redirection targets resolve inside that filesystem, and descriptor
   duplication (`n>&m`) can only re-point one in-memory sink at another; there is
   no host file descriptor to reach.
2. Explicit parser + executor errors for malformed syntax and invalid file ops.
   A command-leading reserved word the runtime cannot execute is rejected at
   parse time, and a command substitution that fails to parse aborts the
   enclosing command, so unsupported constructs fail closed instead of running
   as external commands or continuing with an empty substitution. The same
   fail-closed rule covers the newest syntax: `select`/`coproc`/`time` and stray
   control-flow keywords are parse errors, unsupported `sed` script commands are
   errors, and `python`/`python3`/`node`/`js`/`sqlite3` exit 127 with an explicit
   "does not embed an interpreter" message.
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
   Functions, arrays, positional parameters and jobs are shell state and are
   reset with it.
6. **Network is off by default.** `curl` has no route to a socket unless the
   embedder sets `Options { allow_network: true, .. }`. The default path is
   `Options::http_responses`, an exact URL → body table served without I/O.
   Plain HTTP honours connect/read timeouts and a response-size cap; HTTPS and
   any non-`http` scheme fail closed with an unsupported-protocol error rather
   than downgrading or retrying.
7. **`awk` and `jq` are in-crate, not delegated.** Both are std-only
   implementations that read their inputs through the same in-memory
   filesystem, so they add no host filesystem, process or network surface.
   Each caps input records, executed steps, nesting depth and accumulated output
   bytes, reports a clean `awk: …` / `jq: …` error when a bound is hit, and
   fails closed on unsupported constructs (`getline`, `system()`, user-defined
   functions, output redirection for `awk`; unsupported filters are compile
   errors for `jq`). `jq` additionally caps the filter token count and nesting
   depth *before* building a tree, so a hostile filter — deeply nested
   parentheses or a flat `1 + 1 + …` chain — exits `3` with a clean message
   instead of aborting the process by exhausting the stack.
8. **The regex engine is bounded.** `regex::Regex` enforces a backtracking step
   budget (`DEFAULT_STEP_BUDGET`) and returns no match once it is exhausted, so
   patterns such as `(a+)+$` cannot pin a CPU. Glob matching uses an iterative
   star-backtracking algorithm and is linear in the pattern, not exponential.
9. **Producers are bounded by consumers.** A downstream `head -n`/`grep -m`
   publishes a line budget that `yes`/`seq` honour, so an unbounded producer is
   stopped at demand instead of filling the output ceiling.

## Residual risks and current scope

1. This runtime is not a full Bash implementation and intentionally supports a
   subset of syntax and builtins.
2. `Options { allow_network: true }` deliberately introduces a real network
   surface: an untrusted script can then reach any host the process can reach
   over plain HTTP, including internal addresses. Leave it disabled (the
   default) when scripts are untrusted, and prefer the deterministic
   `http_responses` table for tests.
3. There is still no host-filesystem or process-spawning surface: unknown
   commands report `command not found` (exit 127) and nothing is executed.
   Process substitution is emulated with temporary files inside `InMemoryFs`,
   not OS pipes.
4. Output size, loop iterations and command count are bounded, and the regex
   engine bounds its own work, but per-command CPU or wall-clock time is not
   globally bounded. Add a time bound before exposing this runtime to hostile,
   high-throughput multi-tenant workloads.
5. Pipeline stages are bounded by a consumer budget but are not concurrent:
   each stage's output is materialised in memory before it is handed on, so a
   pipeline with no limiting consumer still relies on the output ceiling.
6. Descriptor handling is narrowed to file descriptors 0-2: `n>&m` duplication is
   defined only for 1 and 2, an explicit file redirection on a larger descriptor
   is ignored rather than rejected, and `/dev/null` discards output redirection
   only. Scripts that rely on wider descriptor use are silently narrowed.
7. Fail-closed parsing is driven by a fixed list of reserved words and
   constructs. A command-position word outside that list still falls through to
   `command not found` (exit 127), and the rest of the script continues to run.
   No host command is spawned; the impact is scope confusion, not escape.
8. `sed`, `curl`, `awk`, `jq` and the regex engine are subsets of their
   GNU/POSIX counterparts. Unsupported options and script commands are errors, but a
   script that assumes a wider feature set may observe different (not silent)
   results.
