# just-bash Rust core

This crate is the Rust rewrite target for the `just-bash` execution core. It
contains a native in-memory filesystem, parser, `Bash` execution facade,
structured execution results, and a small CLI used by the crate tests.

The Rust milestone keeps the surface dependency-free (standard library only)
while preserving the public runtime concepts of the original project:

- `Options` configures initial files, environment, working directory, exec
  isolation behavior, execution limits, and the (off-by-default) network
  surface.
- `parse_script` / `parse_script_with_limits` produce the statement model.
- `Bash::exec` runs shell snippets and returns `ExecResult`.
- `InMemoryFs` provides deterministic virtual filesystem operations with
  metadata (mode, size, logical mtime).
- The `just-bash-rs` binary executes a snippet from argv or stdin.

## Language surface

- **Sequencing / connectors**: `;`, newlines, `&&`, `||`, `!` negation,
  brace groups `{ …; }`, subshells `( … )`.
- **Pipelines**: `|` with `PIPESTATUS`; stages are materialised one at a time
  but consumers set a line budget that bounds producers (see below).
- **Redirections**: `<`, `>`, `>>`, `n<file`, `n>file`, `n>>file`, `n>&m`,
  `n>&-`, `/dev/null`, here-strings `<<<word`, and heredocs `<<DELIM` /
  `<<-DELIM` (a quoted delimiter suppresses expansion).
- **Control flow**: `if`/`elif`/`else`, `while`, `until`, `for` (including
  `for x; do` over positionals), `case` with `|` alternation and glob
  patterns, `break`/`continue` with levels, and shell functions
  (`f() { …; }`, `function f { …; }`, `return`).
- **Conditionals**: `[[ … ]]` with `&&`/`||`/`!`/parentheses, unary file and
  string operators, numeric comparisons, `==`/`!=` glob matching, `=~`
  regular-expression matching, and `-nt`/`-ot`/`-ef`.
- **Expansion**: `$NAME`, `${NAME}`, `${p-word}`, `${p:-word}`, `${p=word}`,
  `${p:=word}`, `${p?word}`, `${p:?word}`, `${p+word}`, `${p:+word}`,
  `${#name}`, `$#`/`$@`/`$*`/`$?`/`$!`/`$$`, arithmetic `$(( … ))` (including
  `$name`/`$1` operands), command substitution `$( … )`, process substitution
  `<( … )` / `>( … )`, and pathname expansion (globbing).
- **Arrays**: indexed arrays (`a=(1 2 3)`, `a[2]=x`, `a+=(4)`), `${a[i]}`,
  `${a[@]}`/`${a[*]}`, `${#a[@]}`, `${#a[i]}`, `${!a[@]}` and indirect
  `${!name}`. Associative arrays are not implemented.
- **Jobs**: `cmd &` runs in a child shell, records a job, sets `$!`, and
  supports `jobs` and `wait` (see "Determinism" below).
- **Shell options**: `set -e`, `set -u`, `set -o pipefail`, including bundled
  clusters such as `set -euo pipefail`.

## Expansion, quoting and globbing

Single quotes suppress expansion; backslash escapes are preserved as their own
word part; `\<newline>` is a line continuation; inside double quotes only `$`,
`` ` ``, `"` and `\` are escapable. Only unquoted `*`, `?` and `[...]` are glob
metacharacters, so `"$x"` never globs while `$x` may. A pattern that matches
nothing stays literal (no `nullglob`). Hidden files are matched only by an
explicit leading `.`. Matches are sorted, and a relative pattern reports
relative paths just as bash does.

## Built-ins

`:`, `basename`, `break`, `cat`, `cd`, `chmod`, `continue`, `cp`, `curl`, `cut`,
`dirname`, `echo`, `env`, `exit`, `export`, `false`, `grep`, `head`, `jobs`,
`ls`, `mkdir`, `mktemp`, `mv`, `printf`, `pwd`, `read`, `return`, `rm`, `sed`,
`seq`, `set`, `sleep`, `sort`, `stat`, `tail`, `test`/`[`, `touch`, `tr`,
`true`, `unset`, `uniq`, `wait`, `wc`, `yes`.

Several accept GNU-style flags:

- `cat`: `-n`, `-b`, `-E`, `-T`, `-v`, `-A`, `-e`, `-t`, `-s`, `-u` and the
  corresponding long options. Display rendering is byte-wise, so `-v` uses
  GNU `^`/`M-` notation.
- `grep`: `-e` (repeatable), `-f`/`--file`, `-E`/`-P` (regular expressions via
  the built-in engine), `-F` (literal, the default), `-i`, `-v`, `-c`, `-n`,
  `-l`, `-L`, `-q`, `-x`, `-w`, `-h`, `-m N` and `--`, plus `-` as a
  standard-input operand. Exit status is `0` on a selected line, `1` when
  nothing is selected, `2` on a read error or invalid pattern.
- `ls`: `-a`, `-A`, `-1`, `-l`, `-r`, `-t`, `-S`, `-F`, `-d` and multiple
  operands, with `name:` headers when several sources are listed.
- `sed`: `-n`, `-e SCRIPT`, `s/re/repl/flags` (`g`, `p`, `i`), `p`, `d`, `q`,
  and line-number/`$` addresses. Other script commands fail closed.
- `stat`: `-c FORMAT` with `%n`, `%s`, `%a`, `%A`, `%F`, `%Y`.
- `curl`: `-s`, `-i`, `-I`, `-X`, `-H`, `-d`/`--data`/`--data-raw`/
  `--data-binary`/`--data-urlencode`, `-o`, `-f`, `-L`, `--`.

Known interpreter names this core does not embed (`python`, `python3`, `node`,
`js`, `sqlite3`) fail closed with an explicit message and exit status `127`
rather than the generic "command not found".

## Determinism

- There is no wall clock: `InMemoryFs` owns a monotonically increasing logical
  clock, so `ls -t`, `-nt`/`-ot` and `touch` orderings are reproducible.
- There is no concurrency: `cmd &` runs the statement to completion in a child
  shell, emits its output in place, records the job for `wait`, and reports
  status `0` for the `&` itself. `sleep` validates its operand and returns
  immediately.
- There is no process spawning: unknown commands report
  `bash: NAME: command not found` and exit `127`.
- Pipelines are bounded rather than concurrent. A downstream consumer (`head
  -n`, `grep -m`) publishes a line budget that the producer honours, so
  `yes | head -n 3` and `seq 1 1000000 | head -n 3` terminate at demand. Each
  stage's output is still materialised before it is handed on.
- `InMemoryFs` is the only filesystem; `curl` has no network access unless
  `Options::allow_network` is set, and `Options::http_responses` serves
  deterministic URL → body responses without any I/O.

## Limits

Default execution limits:

- `max_script_size_bytes`: `1_048_576`
- `max_command_count`: `10_000`
- `max_command_substitution_depth`: `50`
- `max_output_bytes`: `10_485_760`
- `max_loop_iterations`: `100_000`

The regex engine additionally enforces a backtracking step budget
(`regex::DEFAULT_STEP_BUDGET`) so a pathological pattern is abandoned instead of
hanging.

## Exec isolation

Each `exec()` call starts from the initial `env` + `cwd` (isolated shell state),
while filesystem changes persist across calls. Set
`Options { isolate_exec: false, .. }` to opt into persistent shell state between
calls.
