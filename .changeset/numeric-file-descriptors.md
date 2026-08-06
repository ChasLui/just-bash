---
"just-bash": minor
---

Support user file descriptors (fd >= 3). `exec 3< file`, `N< file` / `N> file` / `N>> file` on any command, `read -u N`, `read <&N`, `>&N`, `N<&M`, and `N<&-` now go through a real descriptor table: a descriptor carries one shared read position, `exec` keeps it open until it is closed, and every other construct — including `done N< file` on a loop — gets it only for the duration of that command.
