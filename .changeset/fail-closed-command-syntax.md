---
"just-bash": patch
---

Reject unsupported command-leading reserved words in every parser context instead of discarding them or executing them as simple commands. Unknown command AST nodes now fail explicitly instead of returning a successful result.
