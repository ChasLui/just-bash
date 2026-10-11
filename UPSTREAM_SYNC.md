# Upstream sync ledger

This repository is a Rust-only port of the upstream TypeScript project
[`vercel-labs/just-bash`](https://github.com/vercel-labs/just-bash).

The upstream history *is* part of this repository's history: upstream commit
`d64009a` is an ancestor of this repository's `HEAD`, and the Rust rewrite is
built on top of that exact snapshot. This file records which upstream work made
it into [`packages/just-bash/rust-core`](./packages/just-bash/rust-core), which
file carries it, and what is still missing.

The port is a **subset** of upstream. Nothing below claims full parity; see
[Not yet ported](#not-yet-ported).

## Reference points

| Item | Value |
| --- | --- |
| Upstream repository | `vercel-labs/just-bash` |
| Fork point (upstream snapshot the port is based on) | `d64009a` — `perf(grep): up to 14.5× speedup via preFilter extensions and matcher reuse (#248)`, 2026-05-26 |
| Upstream reference HEAD used for this sync | `58d1ecf3` — `fix(bash): pass per-exec env to nested sh/bash and stop leaking parent vars (#439)`, 2026-10-06 |
| Commits between fork point and reference HEAD | 107 |

Verify with:

```bash
git merge-base --is-ancestor d64009a HEAD && echo "shared history"
git -C <upstream-clone> rev-list --count d64009a..58d1ecf3   # 107
```

The two Rust source files that carry the work below are
[`src/parser.rs`](./packages/just-bash/rust-core/src/parser.rs) (lexer, word
model, redirection/statement parsing) and
[`src/shell.rs`](./packages/just-bash/rust-core/src/shell.rs) (expansion,
execution, built-ins).

## Ported in the current slice

| Upstream commit | PR | Date | Behavior ported | Rust carrier |
| --- | --- | --- | --- | --- |
| `abb904b1` | #347 | 2026-08-09 | Escaped and quoted reserved words are ordinary words (`\if`, `'if'`, `"time"`, partial quoting such as `i"f"`); a backslash escape is preserved as its own word part; `\<newline>` is a line continuation; inside double quotes only `$`, `` ` ``, `"` and `\` are escapable | `parser.rs`: `Word::is_bare` / `Word::is_reserved`, `WordPartKind::Escaped`, `push_part`, lexer quote and escape arms |
| `1a7940dd` | #348 | 2026-08-10 | A command-leading reserved word this runtime cannot execute (`case`, `until`, `select`, `coproc`, `function`, `time`, `[[`, `{`, `!`, `in`, `then`, …) is a parse error (exit 2) instead of silently running as an external command; a `$(…)` that fails to parse aborts the enclosing command | `parser.rs`: `unsupported_construct`, called from `parse_single_statement` and `parse_pipeline_statement`; `shell.rs`: `expansion_failed`, `exec_command_substitution`, `run_invocation` |
| `cb2b583b` | #286 | 2026-07-02 | fd-aware redirection: `n>file`, `n>>file`, `n<file`, `n>&m` duplication (`2>&1`, `1>&2`), `n>&-` close, `/dev/null` discard device; descriptors resolved in written order so `2>&1 > f` differs from `> f 2>&1`; when a dup makes both descriptors target one file, a single sink writes stdout then stderr | `parser.rs`: `Redirect`, `RedirectMode`, `take_fd_prefix`, `lex_dup_target`; `shell.rs`: `Sink`, `run_invocation`, `deliver`, `write_sink` |
| `c9904dea` | #261 | 2026-06-03 | `-o`/`+o` accepted inside a short-flag cluster (`set -euo pipefail`, `set +euo pipefail`, `set -oo pipefail errexit`); a bare `-o`/`+o` prints an option listing; unknown names report `bash: set: NAME: invalid option name` | `shell.rs`: `set` built-in, `set_long_option`, `format_options` |
| `017a911d` | #417 | 2026-09-27 | `break`/`continue` with optional level counts, including `break 2` across nested loops; a loop left via either reports exit status 0 and does not pin `$?` for later iterations | `shell.rs`: `LoopSignal`, `LoopDirective`, `loop_directive`, `adopt_loop_status`, `exec_while`, `exec_for` |
| `4af0dc0f` | #416 | 2026-09-30 | `${p-word}`, `${p:-word}`, `${p=word}`, `${p:=word}`, `${p?word}`, `${p:?word}`, `${p+word}`, `${p:+word}`; `${#name}` length; `$#` positional count; `set -- a b` positional assignment | `shell.rs`: `expand_braced`, `expand_length`, `expand_operator_word`, `parameter_is_set`, `parameter_value`, `positional_count`, `set` positional handling |
| `701f8e5b` | #520 | 2026-10-01 | `${1:=x}` / `${@:=x}` and other default assignments to positional or special parameters are rejected with `bash: $1: cannot assign in this way` | `shell.rs`: `assert_default_assignment_target` |
| `6130334f` | #293 | 2026-07-12 | `cat` display flags `-n`, `-b`, `-E`, `-T`, `-v`, `-A`, `-e`, `-t`, `-s`, `-u` and their long forms; numbering and `-s` run across the whole concatenated stream; `-b` overrides `-n`; `-A` = `-vET` | `shell.rs`: `CatOptions`, `parse_cat_options`, `apply_cat_options`, `push_cat_byte` |
| `90df0572` | #314 | 2026-09-29 | Repeated `-e` patterns accumulate and OR-combine; a positional operand supplies the pattern only when no `-e`/`-f` was given, otherwise every operand is a file | `shell.rs`: `GrepOptions`, `parse_grep_args` |
| `eaedb5bc` | #327 | 2026-08-06 | `-f`/`--file` reads newline-separated patterns from a file (`-fFILE`, `--file=FILE`, `-vf FILE`); a single trailing newline terminates rather than adding an empty pattern; an empty pattern list short-circuits with status 1 | `shell.rs`: `read_pattern_file`, `split_pattern_file`, `run_grep` |
| `66802470` | #331 | 2026-08-06 | `-` as a file operand reads standard input, labelled `(standard input)`; the first `-` consumes the stream and later ones see EOF | `shell.rs`: `run_grep` input resolution |
| `6df692f2` | #315 | 2026-07-24 | `--` terminates option parsing, so `--`, `-x` and `--help` after it are patterns or files | `shell.rs`: `parse_grep_args` |
| `4f9bdec0` | #332 | 2026-08-06 | `grep -L` exit status follows GNU: status reflects whether a line was selected, never whether a name was printed; an unreadable operand yields 2 | `shell.rs`: `run_grep` return value and `had_error` handling |

Notes:

- `4af0dc0f` is specifically the upstream fix for nounset tripping on a
  whole-word quoted `${var:-default}`. The Rust slice implements the `-`/`=`/`?`/`+`
  operator family, `${#name}` and positional handling at the same time, so it
  covers more than that one fix; the row records the upstream change it aligns
  with, not an exact patch equivalence.
- `grep` patterns are literal substrings here, so the regex-dependent parts of
  the upstream commits (BRE/ERE alternation joining, `-P`, per-pattern syntax
  validation) are deliberately not ported; `-x` and `-w` are implemented
  directly against the matched line instead of by wrapping a combined regex.
- The `:` and `unset` built-ins were added in this slice without a dedicated
  upstream commit in the reference range; both are implemented in the
  `run_command` dispatch in `shell.rs`.
- `bash --posix` handling, `xtrace`, `PS4`, arrays and `expansionStderr`
  ordering from the same upstream range are not part of this slice.

## Not yet ported

The following major upstream themes are absent from the Rust core. This list is
not exhaustive and the absence of an item here is not a claim of parity.

- Heredocs and here-strings (`<<`, `<<-`, `<<<`).
- `case`, `until`, `select`, `coproc`, shell functions, and `[[ ]]` conditionals.
  These currently fail closed at parse time rather than running.
- Background jobs and `&` (a bare `&` is a parse error).
- Process substitution (`<(…)`, `>(…)`).
- Pathname expansion / globbing.
- Arrays and associative arrays.
- `PIPESTATUS`.
- Streaming pipelines: each stage's stdout is buffered in memory before being
  handed to the next stage.
- `yes` and `mktemp`.
- `ls` sorting flags and multi-operand handling (the current `ls` reads at most
  one operand and does not sort).
- mtime and mode metadata in the in-memory filesystem (entries store contents
  only), and therefore `-t`/`-S`/`-F` classification.
- Regex-engine-backed `grep`/`sed`/`awk`/`jq`.
- Network commands and the `python`/`js`/`sqlite` command set.
