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
pipelines with `|`, conditional pipeline connectors with `&&` and `||`, input
redirection with `<`, output redirection with `>` and `>>`, inline `$NAME` and
`${NAME}` variable expansion with single-quote suppression, assignment-only
environment updates, and syntax errors for missing pipeline/conditional commands.

Default runtime behavior is intentionally close to upstream: each `exec()` call
starts from the initial `env` + `cwd` (isolated shell state), while filesystem
changes persist across calls. Set `Options { isolate_exec: false, .. }` to
opt into persistent `env`/`cwd` between calls.

Default execution limits:

- `max_script_size_bytes`: `1_048_576`
- `max_command_count`: `10_000`
- `max_command_substitution_depth`: `50`

Supported built-ins in this milestone are `cat`, `cd`, `cp`, `echo`, `env`,
`exit`, `export`, `false`, `grep`, `ls`, `mkdir`, `printf`, `pwd`, `rm`,
`touch`, `true`, and `wc`.
