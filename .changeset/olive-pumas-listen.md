---
"just-bash": patch
---

Stop pipelines from draining the enclosing shell's stdin, so `while read …; do … | …; done < file` runs once per line again.
