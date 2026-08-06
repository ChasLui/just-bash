---
"just-bash": patch
---

Stop command groups, function bodies and `eval` from rewinding stdin they never replaced, so `{ { read a; }; read b; }` gives `b` the second line instead of replaying the first.
