# Upstream sync ledger

This repository is a Rust-only port of the upstream TypeScript project
[`vercel-labs/just-bash`](https://github.com/vercel-labs/just-bash).

The upstream history *is* part of this repository's history: upstream commit
`d64009a` is an ancestor of this repository's `HEAD`, and the Rust rewrite is
built on top of that exact snapshot. This file records which upstream work made
it into the `crates/` workspace (see
[`crates/just-bash`](./crates/just-bash) for the facade), which crate carries
it, and what is still missing.

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

The two Rust crates that carry the work below are
[`crates/just-bash-parser`](./crates/just-bash-parser) (lexer, word model,
redirection/statement parsing) and
[`crates/just-bash-shell`](./crates/just-bash-shell) (expansion, execution,
built-ins).

## Ported in the current slice

| Upstream commit | PR | Date | Behavior ported | Rust carrier |
| --- | --- | --- | --- | --- |
| `abb904b1` | #347 | 2026-08-09 | Escaped and quoted reserved words are ordinary words (`\if`, `'if'`, `"time"`, partial quoting such as `i"f"`); a backslash escape is preserved as its own word part; `\<newline>` is a line continuation; inside double quotes only `$`, `` ` ``, `"` and `\` are escapable | `just-bash-parser`: `Word::is_bare` / `Word::is_reserved`, `WordPartKind::Escaped`, `push_part`, lexer quote and escape arms |
| `1a7940dd` | #348 | 2026-08-10 | A command-leading reserved word this runtime cannot execute (`case`, `until`, `select`, `coproc`, `function`, `time`, `[[`, `{`, `!`, `in`, `then`, …) is a parse error (exit 2) instead of silently running as an external command; a `$(…)` that fails to parse aborts the enclosing command | `just-bash-parser`: `unsupported_construct`, called from `parse_single_statement` and `parse_pipeline_statement`; `just-bash-shell`: `expansion_failed`, `exec_command_substitution`, `run_invocation` |
| `cb2b583b` | #286 | 2026-07-02 | fd-aware redirection: `n>file`, `n>>file`, `n<file`, `n>&m` duplication (`2>&1`, `1>&2`), `n>&-` close, `/dev/null` discard device; descriptors resolved in written order so `2>&1 > f` differs from `> f 2>&1`; when a dup makes both descriptors target one file, a single sink writes stdout then stderr | `just-bash-parser`: `Redirect`, `RedirectMode`, `take_fd_prefix`, `lex_dup_target`; `just-bash-shell`: `Sink`, `run_invocation`, `deliver`, `write_sink` |
| `c9904dea` | #261 | 2026-06-03 | `-o`/`+o` accepted inside a short-flag cluster (`set -euo pipefail`, `set +euo pipefail`, `set -oo pipefail errexit`); a bare `-o`/`+o` prints an option listing; unknown names report `bash: set: NAME: invalid option name` | `just-bash-shell`: `set` built-in, `set_long_option`, `format_options` |
| `017a911d` | #417 | 2026-09-27 | `break`/`continue` with optional level counts, including `break 2` across nested loops; a loop left via either reports exit status 0 and does not pin `$?` for later iterations | `just-bash-shell`: `LoopSignal`, `LoopDirective`, `loop_directive`, `adopt_loop_status`, `exec_while`, `exec_for` |
| `4af0dc0f` | #416 | 2026-09-30 | `${p-word}`, `${p:-word}`, `${p=word}`, `${p:=word}`, `${p?word}`, `${p:?word}`, `${p+word}`, `${p:+word}`; `${#name}` length; `$#` positional count; `set -- a b` positional assignment | `just-bash-shell`: `expand_braced`, `expand_length`, `expand_operator_word`, `parameter_is_set`, `parameter_value`, `positional_count`, `set` positional handling |
| `701f8e5b` | #520 | 2026-10-01 | `${1:=x}` / `${@:=x}` and other default assignments to positional or special parameters are rejected with `bash: $1: cannot assign in this way` | `just-bash-shell`: `assert_default_assignment_target` |
| `6130334f` | #293 | 2026-07-12 | `cat` display flags `-n`, `-b`, `-E`, `-T`, `-v`, `-A`, `-e`, `-t`, `-s`, `-u` and their long forms; numbering and `-s` run across the whole concatenated stream; `-b` overrides `-n`; `-A` = `-vET` | `just-bash-shell`: `CatOptions`, `parse_cat_options`, `apply_cat_options`, `push_cat_byte` |
| `90df0572` | #314 | 2026-09-29 | Repeated `-e` patterns accumulate and OR-combine; a positional operand supplies the pattern only when no `-e`/`-f` was given, otherwise every operand is a file | `just-bash-shell`: `GrepOptions`, `parse_grep_args` |
| `eaedb5bc` | #327 | 2026-08-06 | `-f`/`--file` reads newline-separated patterns from a file (`-fFILE`, `--file=FILE`, `-vf FILE`); a single trailing newline terminates rather than adding an empty pattern; an empty pattern list short-circuits with status 1 | `just-bash-shell`: `read_pattern_file`, `split_pattern_file`, `run_grep` |
| `66802470` | #331 | 2026-08-06 | `-` as a file operand reads standard input, labelled `(standard input)`; the first `-` consumes the stream and later ones see EOF | `just-bash-shell`: `run_grep` input resolution |
| `6df692f2` | #315 | 2026-07-24 | `--` terminates option parsing, so `--`, `-x` and `--help` after it are patterns or files | `just-bash-shell`: `parse_grep_args` |
| `4f9bdec0` | #332 | 2026-08-06 | `grep -L` exit status follows GNU: status reflects whether a line was selected, never whether a name was printed; an unreadable operand yields 2 | `just-bash-shell`: `run_grep` return value and `had_error` handling |

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
  `run_command` dispatch in `just-bash-shell`.
- `bash --posix` handling, `xtrace`, `PS4`, arrays and `expansionStderr`
  ordering from the same upstream range are not part of this slice.

## Ported in the current slice

This slice closes the language-feature gap the previous revision listed as "not
yet ported". It has no single upstream commit to align with, so the rows record
the upstream behavior family each area corresponds to.

| Area | Behavior ported | Rust carrier |
| --- | --- | --- |
| Heredocs | `<<DELIM`, `<<-DELIM` (tab stripping), quoted delimiters suppressing expansion, bodies handed to a pipeline stage | `just-bash-parser`: `PendingHeredoc`, `consume_heredoc_bodies`, `parse_heredoc_word`, `RedirectMode::Heredoc`; `just-bash-shell`: `expand_heredoc` |
| Here-strings | `<<<word`, trailing newline, no field splitting | `just-bash-parser`: `Token::RedirectHereString`, `RedirectMode::HereString` |
| `case` | `case WORD in PAT\|PAT) … ;; esac`, optional `(`, glob patterns, `*)` default | `just-bash-parser`: `CaseStatement`, `parse_case`; `just-bash-shell`: `exec_case` |
| `until` | `until COND; do … done` (body runs while the condition fails) | `just-bash-parser`: `WhileStatement::until`; `just-bash-shell`: `exec_while` |
| Functions | `f() { …; }`, `function f { …; }`, `f() ( … )`, per-call positional parameters, `return N`, recursion | `just-bash-parser`: `FunctionDef`, `parse_function_keyword`, `parse_function_body`; `just-bash-shell`: `define_function`, `call_function`, `return` |
| `[[ … ]]` | `&&`/`\|\|`/`!`/parentheses, unary file and string tests, numeric comparisons, `==`/`!=` glob matching, `=~` regex, `-nt`/`-ot`/`-ef` | `just-bash-parser`: `CondExpr`, `CondTerm`, `parse_condition`, `lex_condition_words`; `just-bash-shell`: `exec_cond`, `eval_cond_term` |
| Compound commands | `{ …; }` groups, `( … )` subshells (state isolated, filesystem persists), `! cmd` negation | `just-bash-parser`: `StatementKind::Group`/`Subshell`, `Statement::negated`; `just-bash-shell`: `run_subshell`, `exec_statement` |
| Background jobs | `cmd &`, `$!`, `jobs`, `wait`, filesystem visibility, no environment leak | `just-bash-parser`: `Token::Background`, `Statement::background`; `just-bash-shell`: `exec_background`, `JobRecord`, `run_jobs`, `run_wait` |
| Process substitution | `<(cmd)` and `>(cmd)` through deterministic temporary paths, removed after the command | `just-bash-parser`: `WordPartKind::ProcSubIn`/`ProcSubOut`; `just-bash-shell`: `exec_procsub_script`, `flush_procsub_readers`, `cleanup_procsubs` |
| Pathname expansion | Unquoted `*`/`?`/`[...]` with ranges and negation, hidden-file rules, sorted results, relative output shape, trailing-slash directory restriction | `just-bash-glob`: `pattern_matches`, `component_matches`, `expand`; `just-bash-shell`: `expand_word_list`, `glob_fields` |
| Arrays | Indexed arrays: `a=(…)`, `a[i]=v`, `a+=(…)`, `${a[i]}`, `${a[@]}`, `${a[*]}`, `${#a[@]}`, `${#a[i]}`, `${!a[@]}`, indirect `${!name}`, `"${a[@]}"` as one field per element | `just-bash-parser`: `Token::ArrayAssign`, `ArrayAssign`, `WordPart::quoted`; `just-bash-shell`: `exec_array_assign`, `expand_word_list`, array-aware `parameter_value` |
| `PIPESTATUS` | One status per pipeline stage, replaced after each command | `just-bash-shell`: `set_pipestatus`, `exec_pipeline`, `exec_statement_inner` |
| Bounded pipelines | A downstream `head -n`/`grep -m` publishes a line budget that `yes`/`seq` honour, so an unbounded producer stops at demand | `just-bash-shell`: `pipeline_line_budget`, `head_line_limit`, `grep_max_count`, `run_yes`, `run_seq` |
| `yes`, `mktemp`, `seq`, `sleep` | Bounded `yes`; deterministic `mktemp` (`-d`, `-p`, `X`-run templates); `seq FIRST [STEP] LAST`; operand-validating no-op `sleep` | `just-bash-shell`: `run_yes`, `run_mktemp`, `run_seq`, `substitute_x_run` |
| `ls` sorting | `-a`, `-A`, `-1`, `-l`, `-r`, `-t`, `-S`, `-F`, `-d`, multiple operands with `name:` headers | `just-bash-shell`: `LsOptions`, `run_ls`, `sort_ls_entries`, `format_ls_entry` |
| FS metadata | Mode, size and logical mtime per entry; `chmod`, `stat`, `touch` stamping, `test -r/-w/-x/-s` | `just-bash-fs`: `Metadata`, `EntryKind`, `set_mode`, `set_mtime`, logical clock; `just-bash-shell`: `run_chmod`, `run_stat` |
| Regex engine | Dependency-free POSIX ERE engine (`new`/`is_match`/`find`/`captures`/`replace_all`/`split`) with a backtracking step budget, powering `grep -E`/`-P` and `[[ =~ ]]` | `just-bash-regex` (whole module); `just-bash-shell`: `compile_grep_patterns`, `GrepPattern`, `regex_matches`, `regex_matches_word` |
| `sed` | `-n`, `-e`, substitution with `g`/`p`/`i`, `p`, `d`, `q`, line-number/`$` addresses, backed by the regex engine | `just-bash-sed` (whole module); `just-bash-shell`: the `sed` dispatch arm |
| Associative arrays | `declare -A`/`local -A`, `m[key]=v`, `declare -A m=([k]=v)`, `${m[k]}`, `${#m[@]}`, `${!m[@]}` (keys), `${m[@]}`, `unset m[k]`, subscripts with expansions | `just-bash-shell`: `assoc_arrays`, `declare_assoc`, `run_declare`, `assign_element` |
| Slicing | `${v:off}`, `${v:off:len}`, `${a[@]:off:len}`, `${@:off:len}`, negative offsets | `just-bash-shell`: `expand_slice`, `slice_bounds`, `slice_text` |
| Scoping | `declare`/`typeset`/`local` with `-A`/`-a`/`-g`, function-local restored on return, array literals attached to a declaration (`declare -A m=(…)`, `local -a a=(…)`) | `just-bash-shell`: `ScopedVar`, `run_declare`, `restore_scope`, `local_scopes`; `just-bash-parser`: `CommandInvocation::array` |
| `shift` | `shift [n]`, status 1 when shifting past the end | `just-bash-shell`: the `shift` dispatch arm |
| Brace expansion capture | `${ … }` is lexed as one unit, so `${v:-a b}` and `${v: -2}` keep spaces/operators literal; unary signs in `$(( … ))` | `just-bash-parser`: `consume_parameter_expansion`; `just-bash-shell`: `eval_arithmetic_expr` |
| `awk` | Pure-std AWK subset: `-F`/`-v`/`-f`, `BEGIN`/`END`, pattern and range rules, fields/`NF`/`NR`/`FNR`/`FILENAME`, `OFS`/`ORS`, arrays with sorted `for (k in a)`, the documented built-ins; bounded records/steps/output; `getline`/`system()`/user functions/output redirection fail closed | `just-bash-awk` (whole module); `just-bash-shell`: the `awk` dispatch arm |
| `jq` | Pure-std jq subset: `-r`/`-j`/`-c`/`-n`/`-s`/`-e`/`-a`/`--arg`/`--argjson`/`--indent`/`--tab`, paths and `.[]`/slices, pipes/commas/constructors, jq ordering, `//`, `if/then/else`, `as $x`, `try`/`catch`, string interpolation, `del` over path expressions, documented built-ins, regex-backed `test`/`match`/`capture`/`sub`/`gsub`; bounded filter tokens/nesting depth and output so a hostile filter cannot exhaust the stack; unsupported syntax is a compile error | `just-bash-jq` (whole module); `just-bash-shell`: the `jq` dispatch arm |
| Network | `curl` with an offline-fail-closed default, an injectable deterministic URL → body table, and opt-in plain-HTTP fetching; `@file` bodies resolved from the virtual filesystem | `just-bash-net` (whole module); `just-bash-shell`: `run_curl`; `Options::allow_network` / `Options::http_responses` |

Notes:

- `grep`'s default mode remains literal substring matching (`-F` semantics); the
  regex engine is selected explicitly with `-E`/`-P`. Upstream (and GNU grep)
  default to BRE, which this engine does not implement, so the deviation is
  deliberate and documented rather than silent.
- `[[ =~ ]]` uses leftmost-first (PCRE-style) semantics rather than POSIX
  leftmost-longest; boolean tests are unaffected.
- `sed` is an ERE-based subset (`-E` semantics by default); BRE-specific escapes
  are not emulated.
- `curl` speaks plain HTTP only. HTTPS fails closed with an unsupported-protocol
  error rather than silently downgrading.
- `awk` follows GNU awk where GNU and BSD disagree: `-F t`/`-Ft` are a literal
  `t` (BSD reads a bare `t`/`n` as tab/newline), `0x1f` is the hex literal `31`
  (BSD reads `0` then `x1f`), `length(x)` of an uninitialised variable is `0`,
  and `for (k in a)` iterates in sorted key order. Differential runs against
  macOS's BSD `awk` therefore show those specific differences and no others.
- `jq` prints numbers in the shortest plain decimal form, so the literal filter
  `1e3` prints `1000` where jq 1.8 echoes the source text as `1E+3`; computed
  values agree, and the `{re, flags}` / `[re, flags]` regex argument forms are
  accepted by every regex built-in.

## Not yet ported

This list is not exhaustive, and the absence of an item is not a claim of parity.

- `find -exec` and the wider upstream utility surface beyond `awk`/`jq`.
- `select`, `coproc`, `time`, `let`, `eval`, `source`/`.`, `trap`, `getopts`,
  `alias`, `readonly`, `command`, `type`, `hash`, and job control beyond
  `&`/`jobs`/`wait`.
- `${!prefix*}` prefix expansion, `${a[@]@Q}` transformations, `read -a`,
  `mapfile`/`readarray`, and namerefs (`declare -n`).
- Full BRE support and POSIX leftmost-longest ERE semantics.
- True concurrent streaming pipelines: stages are bounded by a consumer-derived
  budget, but each stage's output is still materialised in memory before it is
  handed to the next.
- Real interpreter parity for `python`/`js`/`sqlite3`: the names exist and fail
  closed, but no interpreter is embedded.
- Host filesystem access, process spawning, signals, and host exit-status
  propagation beyond the in-process model.
