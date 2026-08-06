---
"just-bash": minor
---

Treat a `-` FILE operand in `grep` as standard input, matching GNU. `grep PATTERN -` now reads stdin instead of failing with "No such file or directory", stdin is labelled `(standard input)` in the multi-file prefix and in `-l`/`-L`/`-c` output, repeated `-` operands see the stream drained by the first one, and `-` is exempt from `-r` recursion and `--include`/`--exclude` filtering.
