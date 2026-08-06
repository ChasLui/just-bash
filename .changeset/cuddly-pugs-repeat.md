---
"just-bash": patch
---

Fix `grep -L` exit status to match GNU grep. The status reports whether a line
was selected, not whether a filename was printed, so `grep -L` now exits 0 when
every file matched (printing nothing) and 1 when no file matched (printing every
name) — previously these were inverted.
