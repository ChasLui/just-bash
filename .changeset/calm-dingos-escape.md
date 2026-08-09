---
"just-bash": patch
---

Fix escaped reserved words being parsed as shell syntax. Unquoted escapes now retain their provenance through lexing and word parsing, including when the word is serialized back to Bash.
