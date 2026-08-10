---
"just-bash": minor
---

Support process substitution `<(cmd)` and `>(cmd)`.

`<(cmd)` runs `cmd` and substitutes a readable `/dev/fd/N` path backed by an
in-memory file; `>(cmd)` substitutes a writable path whose contents are fed to
`cmd` once the outer command finishes. Descriptors are numbered from 63
downwards like bash and released when the command that opened them completes.
Process substitutions retain their surrounding word context in assignments,
conditionals, regular expressions, and heredoc delimiters. Previously any use
raised `Parse error: Expected redirection target`.
