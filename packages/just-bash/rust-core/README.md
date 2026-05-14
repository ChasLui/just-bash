# just-bash Rust core

This crate is the Rust rewrite target for the `just-bash` execution core. It
contains a native in-memory filesystem, parser, `Bash` execution facade,
structured execution results, and a small CLI used by the crate tests.

The current Rust milestone intentionally keeps the surface dependency-free while
preserving the public concepts of the TypeScript package:

- `BashOptions` configures initial files, environment, and working directory.
- `parse_script` produces a small command/pipeline/redirection model.
- `Bash::exec` runs shell snippets and returns `BashExecResult`.
- `InMemoryFs` provides deterministic virtual filesystem operations.
- The `just-bash-rs` binary executes a snippet from argv or stdin.

Supported execution features include command sequencing with `;`/newlines,
pipelines with `|`, conditional pipeline connectors with `&&` and `||`, input
redirection with `<`, output redirection with `>` and `>>`, inline `$NAME` and
`${NAME}` variable expansion with single-quote suppression, and assignment-only
environment updates, and syntax errors for missing pipeline/conditional commands.

Supported built-ins in this milestone are `cat`, `cd`, `cp`, `echo`, `env`,
`exit`, `export`, `false`, `grep`, `ls`, `mkdir`, `printf`, `pwd`, `rm`,
`touch`, `true`, and `wc`.
