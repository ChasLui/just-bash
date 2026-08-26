---
"just-bash": patch
---

Implement `curl -D` / `--dump-header` (file, `-`, `--dump-header=`, redirect hops, `-f` dumps). `curl -I` header blocks now end with a blank line (`\r\n\r\n`), matching real curl.
