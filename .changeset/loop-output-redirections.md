---
"just-bash": patch
---

Apply output redirections attached to `while` and `until` loops. `while true; do echo x; break; done >/dev/null` no longer leaks its output to the caller, and `> file`, `>>`, `2>`, `2>&1`, `&>` and `>|` now behave on loops the way they already did on `for` and `case`. `until` loops also gained the input-redirection handling `while` loops already had, so `until ! read l; do ...; done < file` reads from the file. A loop now only restores stdin it owns, so reading inside a loop no longer rewinds an enclosing group's read position: `printf 'a\nb\n' | { while read x; do break; done; read y; }` sees `y=b`.
