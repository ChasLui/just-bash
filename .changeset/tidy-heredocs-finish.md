---
"just-bash": patch
---

Preserve whether a here-document ended at its delimiter or at end-of-input. Unterminated final body lines now receive Bash's trailing newline, backslash-newline continuations are removed during expansion, and serialization rejects unterminated documents rather than manufacturing a closing delimiter.
