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
   - loop iteration ceiling,
   - **parser nesting ceiling** (`max_nesting_depth`, default 32) for `( )`,
     `{ }`, `if`, `while`, `for`, `case`, function bodies and `[[ … ]]`
     groups. Recursive-descent parsing spends ~10 KB of stack per level, so
     without this bound ~800 nested `( … )` aborts the whole process — an abort
     an embedder cannot catch. The default is far below upstream's 200 because
     a Rust parser frame costs ~14 KB of stack: measurement puts the
     debug-build maximum at 56 levels on a 1 MiB thread stack, so 32 keeps a
     ~1.75x margin. Embedders that control stack size can raise it,
   - **function call depth ceiling** (`max_call_depth`, default 100) for
     `f(){ f; }` and mutually recursive functions,
   - **expansion depth ceiling** (`max_expansion_depth`, default 100) for
     `${x:-${x:-…}}`, where expansion recurses through the operator word.

   Every one of these reports a clean `bash: …` message and a non-zero exit
   status. `Bash::exec` additionally wraps execution in `catch_unwind` as a
   last-resort containment layer: an unexpected panic becomes
   `bash: internal error: …` with exit status 2 instead of unwinding into the
   embedder. That layer does **not** cover stack overflow (an abort), which is
   why the nesting bounds above are the primary defense.
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
4. Output size, loop iterations, command count and interpreter nesting are
   bounded, and the regex engine bounds its own work, but per-command CPU or
   wall-clock time is not globally bounded. Add a time bound before exposing
   this runtime to hostile, high-throughput multi-tenant workloads.
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
9. The nesting bounds are sized for the smallest stack this runtime targets
   (1 MiB). An embedder that pins a smaller thread stack than that — for
   example a custom executor with a 256 KiB stack — can still overflow before
   the limit is reached, because the cost is per Rust stack frame, not per
   logical level. Raise `ExecutionLimits` only together with the stack budget,
   and prefer running untrusted scripts on a thread whose stack is at least
   1 MiB.
10. A compound command's redirected stdin is shared through one in-memory
    cursor. Commands known to read standard input (`cat`, `grep`, `sort`,
    `uniq`, `cut`, `tr`, `head`, `tail`, `wc`, `sed`, `awk`, `jq`) consume the
    remainder; `read` consumes one line; every other command is assumed not to
    touch stdin. A command outside that list that does read stdin inside a
    redirected compound (`while read l; do myfilter; done < f`) therefore sees
    an empty stream rather than the remaining bytes. This is a narrowing of
    real descriptor semantics, not an escape.
11. Compound commands are still not accepted as a non-first pipeline stage:
    `echo hi | { cat; }` and `cmd | while read l; do …; done` are parse errors,
    not silent misbehaviour.
12. Unquoted expansions are field-split on `IFS` and each resulting field is
    pathname-expanded, as in POSIX. Characters that came from a quoted or
    backslash-escaped part are marked in a parallel mask and are escaped before
    pathname expansion, so `"*.txt"` and `\*.txt` stay literal. A word-level
    consequence worth knowing: `x=""; set -- $x` yields zero fields while
    `set -- "$x"` yields one empty field.
13. Parameter pattern operators (`${v#p}`, `${v##p}`, `${v%p}`, `${v%%p}`,
    `${v/p/r}`, `${v//p/r}`) match with the same bounded glob engine used for
    pathname expansion. Matching is O(n²) in the parameter length per operator,
    which is bounded by `max_string_length`-scale inputs; empty matches are never
    replaced, so `*`-style patterns terminate.
14. Leading `NAME=value` words of a command form a temporary environment that is
    restored after the command runs. The value is expanded without field
    splitting, so quoting cannot be lost.
15. Parser-level expansions are bounded. Brace expansion only runs on a single
    unquoted literal word, recurses at most 16 levels, and caps a numeric range
    at 10 000 items; ANSI-C quoting and backtick substitution produce ordinary
    word parts, so they inherit the same nesting and output budgets as `$(…)`.
16. `eval` and `source`/`.` re-enter the parser in the *current* shell state, so
    they can define functions and change variables. Both are bounded by
    `max_source_depth` (default 100) and report `nesting exceeds limit` rather
    than recursing without bound; `source` reads only from the in-memory
    filesystem, so it cannot reach host files.
17. Text builtins share one option parser (`opts`). Unknown options are refused
    with a non-zero status instead of being treated as file operands, so a
    typo like `sort -z` can no longer silently change which files are read.
18. `alias` expansion is textual and non-recursive: an alias whose replacement
    starts with its own name is not expanded, so `alias ls='ls -l'` cannot loop.
    `ls -R` walks only the in-memory filesystem and inherits the same traversal
    bounds as every other path walk.
19. `tar` reads and writes only inside the in-memory filesystem. Archive bytes
    are Latin-1 encoded on the way in, so they round-trip without loss and
    cannot smuggle a host path. Extraction refuses `..`-style escapes because
    every member is joined onto a resolved in-memory prefix.
20. `find` walks the in-memory tree with the same depth accounting as `ls -R`;
    it never touches the host filesystem.
21. `$((…))` is always arithmetic and its body is never re-read as a command
    substitution. bash behaves the same way for command-shaped bodies (it
    raises a syntax error), and this runtime matches it, so a desync attempt
    such as `$(( (echo LEFT; echo X > marker) || … ))` only produces an
    arithmetic error and creates no file. An experiment that added a subshell
    fallback here turned the `substitution-desync` fixtures from
    `MARKER_ABSENT` to `MARKER_PRESENT`; the regression is pinned by
    `arithmetic_disambiguation_tests`.
22. A compound command used as a pipeline stage (`{ …; } | cat`, `( … ) | wc`,
    `for …; done | wc`) runs inside `child_shell()` with its own stdin cursor, so
    a stage cannot leak state into the parent or skip the pipeline. Before this
    was implemented the stage's output bypassed the pipe *and reported no
    error*, which made a wrong result look successful; the behaviour is now
    pinned by `compound_pipeline_tests`. A compound stage in the *middle* of a
    pipeline initially consumed nothing, because the shared cursor was only
    handed over when the leading stage had a command name; the hand-over now
    also covers a compound lead, pinned by `middle_compound_stage_tests`.
23. `printf` format widths and precisions are parsed with bounded counters; an
    absurd width such as `%999999999s` only pads the output buffer, which is
    itself capped by the output limit, so it cannot allocate without bound.
24. `trap` honours only `EXIT`; every other signal name is accepted and ignored
    rather than simulated, so a script cannot install a handler that would
    fire at an unpredictable point. The EXIT body runs after the script body
    and its output is appended, which keeps draining the output limit.
25. `tee` writes only inside the in-memory filesystem, and `readlink`/`realpath`
    canonicalise against it, so neither can resolve a host path.
    `exec` redirections persist in the same namespace: the descriptor table
    holds only in-memory sinks, so `exec 3>f` cannot name a host file. A
    persisted descriptor is consulted by every later command, which lets one
    `exec` redirect the remainder of a script — the same power bash gives it,
    restricted to the descriptors and commands this runtime already supports.
26. `uname`, `whoami` and `id` report fixed virtual values and never read the
    host environment. `md5sum`/`sha1sum`/`sha256sum` digest only buffers already
    read through the in-memory filesystem, and their output is bounded by the
    output limit like any other command.
    `/dev/zero` is an endless source, so its contribution is capped by the
    downstream line budget and the output limit (at most 1 MiB per read) — the
    same back-pressure that makes `yes | head -n 3` terminate. `/dev/null` reads
    empty and discards writes; neither device can name a host path.
27. `xxd` reads only through the in-memory filesystem and emits at most a
    bounded multiple of its input, so it cannot amplify output past the limit.
    A known consequence of the `String`-based output model is that `printf`
    escapes above `\x7f` become UTF-8 two-byte sequences; only the ASCII range
    round-trips byte-exactly (recorded in `UPSTREAM_GAPS.md` §39).
28. `du` walks only the in-memory tree and reports sizes derived from stored
    contents, so it cannot disclose host disk usage. `df` is deliberately not
    implemented: a fabricated filesystem table would misreport capacity.
29. `pushd`/`popd`/`dirs` keep a directory stack that only ever holds resolved
    in-memory paths, so a stack entry cannot point outside the virtual
    filesystem. `type`'s builtin registry is asserted against the commands that
    actually exist, so it cannot drift into advertising missing commands.
30. The text utilities (`rev`, `fold`, `expand`, `unexpand`, `strings`, `paste`,
    `column`, `split`) operate on buffers already read through the in-memory
    filesystem. `split` is the only one that writes, and it writes only there.
    `strings` treats its input as bytes and never interprets them as paths.
20. `grep` compiles patterns with the in-crate regex engine, which enforces its
    own backtracking step budget, so a pathological pattern cannot pin the CPU.
    `grep -r` walks only the in-memory filesystem and cannot escape it.
21. Arithmetic (`$(( … ))`, `(( … ))`, `let`) runs on a recursive-descent
    evaluator with an explicit recursion cap (64) and a separate cap on
    variable-value re-evaluation (so `x=x` terminates). All arithmetic is
    wrapping `i64`; division by zero and negative exponents are clean errors,
    not panics. The evaluator never allocates beyond the expression text.
