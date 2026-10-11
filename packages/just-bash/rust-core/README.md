# just-bash Rust core

This crate is the Rust rewrite target for the `just-bash` execution core. It
contains a native in-memory filesystem, parser, `Bash` execution facade,
structured execution results, and a small CLI used by the crate tests.

The current Rust milestone intentionally keeps the surface dependency-free while
preserving the public runtime concepts of the original project:

- `Options` configures initial files, environment, working directory, exec
  isolation behavior, and execution limits.
- `parse_script` / `parse_script_with_limits` produce a small
  command/pipeline/redirection model.
- `Bash::exec` runs shell snippets and returns `ExecResult`.
- `InMemoryFs` provides deterministic virtual filesystem operations.
- The `just-bash-rs` binary executes a snippet from argv or stdin.

Supported execution features include command sequencing with `;`/newlines,
pipelines with `|` (stages are buffered in memory), conditional pipeline
connectors with `&&` and `||`, and fd-aware redirection: `<`, `>`, `>>`, the
prefixed forms `n<file`, `n>file` and `n>>file`, descriptor duplication with
`n>&m` (`2>&1`, `1>&2`), descriptor close with `n>&-`, and `/dev/null` as a
discard device for both input and output. Descriptors resolve in written order,
so `2>&1 > f` leaves stderr on the caller while `> f 2>&1` sends both streams to
the file.

Expansion covers `$NAME`, `${NAME}`, the parameter-expansion operators
`${p-word}`, `${p:-word}`, `${p=word}`, `${p:=word}`, `${p?word}`, `${p:?word}`,
`${p+word}` and `${p:+word}`, `${#name}` length, `$#`/`$@` positional
parameters, `set -- a b` positional assignment, arithmetic expansion
`$((expr))`, and command substitution `$(...)`. Single quotes suppress
expansion, backslash escapes are preserved as their own word part,
`\<newline>` is a line continuation, and inside double quotes only `$`,
`` ` ``, `"` and `\` are escapable. Assignment-only commands update the
environment.

Control flow covers `if`/`elif`/`else`, `while` and `for` loops, and
`break`/`continue` with optional level counts. Shell options are `set -e`,
`set -u` and `set -o pipefail`, including bundled clusters such as
`set -euo pipefail`. Escaped and quoted reserved words are ordinary words; a
command-leading reserved word the runtime cannot execute is a parse error rather
than an external command.

Default runtime behavior is intentionally close to upstream: each `exec()` call
starts from the initial `env` + `cwd` (isolated shell state), while filesystem
changes persist across calls. Set `Options { isolate_exec: false, .. }` to
opt into persistent `env`/`cwd` between calls.

Default execution limits:

- `max_script_size_bytes`: `1_048_576`
- `max_command_count`: `10_000`
- `max_command_substitution_depth`: `50`
- `max_output_bytes`: `10_485_760`
- `max_loop_iterations`: `100_000`

Supported built-ins in this milestone are `:`, `basename`, `break`, `cat`, `cd`,
`continue`, `cp`, `cut`, `dirname`, `echo`, `env`, `exit`, `export`, `false`,
`grep`, `head`, `ls`, `mkdir`, `mv`, `printf`, `pwd`, `read`, `rm`, `set`,
`sort`, `tail`, `test`/`[`, `touch`, `tr`, `true`, `unset`, `uniq`, and `wc`.

Two of them accept GNU-style flags:

- `cat`: `-n`, `-b`, `-E`, `-T`, `-v`, `-A`, `-e`, `-t`, `-s`, `-u` and the
  corresponding long options. Display rendering is byte-wise, so `-v` uses
  GNU `^`/`M-` notation.
- `grep`: `-e` (repeatable), `-f`/`--file`, `-i`, `-v`, `-c`, `-n`, `-l`, `-L`,
  `-q`, `-x`, `-w`, `-h`, `-F` and `--`, plus `-` as a standard-input operand.
  Patterns are literal substrings OR-combined (there is no regex engine), exit
  status is `0` on a selected line, `1` when nothing is selected and `2` on a
  read error.
