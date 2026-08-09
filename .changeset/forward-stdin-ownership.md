---
"just-bash": patch
---

Restore command groups and subshells after the process-substitution and stdin-ownership changes were combined without forwarding ownership through inner command dispatch.
