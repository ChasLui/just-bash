---
"just-bash": patch
---

Parse and serialize bare file descriptor variable redirections such as `{output}>output.log` and `{input}<<EOF`. Bare redirects create their target with a command-scoped descriptor, while named command forms keep the allocated descriptor available.
