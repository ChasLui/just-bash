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
  `$name`/`$1` operands and unary signs), command substitution `$( … )`,
  process substitution `<( … )` / `>( … )`, and pathname expansion (globbing).
  A `${ … }` expansion is captured as one lexical unit, so spaces and shell
  operators inside it (`${v:-a b}`, `${v: -2}`) stay literal.
- **Arrays**: indexed arrays (`a=(1 2 3)`, `a[2]=x`, `a+=(4)`) and
  associative arrays (`declare -A m`, `m[key]=v`, `declare -A m=([k]=v)`),
  with `${a[i]}`/`${m[key]}`, `${a[@]}`/`${a[*]}` (one field per element for
  `@`, one joined field for `*`), `${#a[@]}`/`${#m[@]}`, `${#a[i]}`,
  `${!a[@]}` (indices) / `${!m[@]}` (keys), and indirect `${!name}`.
  Subscripts may contain expansions (`${m[$key]}`).
- **Slicing**: `${v:off}`, `${v:off:len}` on strings, `${a[@]:off:len}` on
  arrays and `${@:off:len}` on positional parameters; negative offsets count
  from the end, and a non-positive length yields nothing.
- **Scoping**: `declare`/`typeset`/`local` (`-A` associative, `-a` indexed,
  `-g` force global, `-r`/`-x`/`-i` accepted). Inside a function both `local`
  and `declare` are function-local and restored on return, so recursion and
  nested calls behave as in bash.
- **`shift [n]`** drops leading positional parameters; shifting past the end
  reports status 1 and leaves them unchanged.
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
`declare`, `dirname`, `echo`, `env`, `exit`, `export`, `false`, `grep`, `head`,
`jobs`, `local`, `ls`, `mkdir`, `mktemp`, `mv`, `printf`, `pwd`, `read`,
`return`, `rm`, `sed`, `seq`, `set`, `shift`, `sleep`, `sort`, `stat`, `tail`,
`test`/`[`, `touch`, `tr`, `true`, `typeset`, `unset`, `uniq`, `wait`, `wc`,
`yes`.

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
- `awk`: `-F`, `-v`, `-f`, `--`; `BEGIN`/`END`, `pattern { action }`, `/re/` and
  range patterns, fields and `NF`/`NR`/`FNR`/`FILENAME`, `OFS`/`ORS`, arrays
  with `for (k in a)` (sorted key order), and the usual built-ins (`length`,
  `substr`, `index`, `split`, `sub`, `gsub`, `match`, `sprintf`, `tolower`,
  `toupper`, `int`, `sqrt`, `exp`, `log`, `sin`, `cos`, `atan2`, `rand`,
  `srand`). `getline`, `system()`, user-defined functions and output
  redirection fail closed; records, steps and output bytes are capped.
- `jq`: `-r`, `-j`, `-c`, `-n`, `-s`, `-e`, `-a`, `--arg`, `--argjson`,
  `--tab`, `--indent`, `--`; the documented filter subset (paths, `.[]`,
  slices, pipes, commas, constructors, arithmetic and jq ordering, `//`,
  `if/then/else`, `as $x`, `try`/`catch`, string interpolation and `del` over
  path expressions, plus the documented built-ins). JSON nesting depth, filter
  token count, filter nesting depth and output bytes are capped; unsupported
  syntax is a compile error, never a silent no-op. Two deliberate deviations
  from jq 1.8: numbers print in the shortest plain decimal form (`1e3` prints
  `1000` instead of echoing the source literal) and the `{re, flags}` /
  `[re, flags]` regex argument forms are accepted by every regex built-in.
- `curl`: `-s`, `-i`, `-I`, `-X`, `-H`, `-d`/`--data`/`--data-raw`/
  `--data-binary`/`--data-urlencode`, `-o`, `-f`, `-L`, `--`.

Known interpreter names this core does not embed (`python`, `python3`, `node`,
`js`, `sqlite3`) fail closed with an explicit message and exit status `127`
rather than the generic "command not found". `awk` and `jq` are *not* in that
group: both are implemented in-crate (see `awk.rs` and `jq.rs`) with no external
dependency and no host access beyond the in-memory filesystem.

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
hanging. `awk` and `jq` cap input records, executed steps, nesting depth and
accumulated output bytes, and report a clean error when a bound is hit. `jq`
also caps the filter token count so that a hostile filter cannot build a tree
deep enough to exhaust the stack while parsing, evaluating or dropping it.

## Exec isolation

Each `exec()` call starts from the initial `env` + `cwd` (isolated shell state),
while filesystem changes persist across calls. Set
`Options { isolate_exec: false, .. }` to opt into persistent shell state between
calls.
