# Fix doc width regressions on trunk

`python3 scripts/audit-doc-style.py` fails on trunk today, before any 0080 changes:

- `doc/reference/cli.md:263` -- 109 code points
- `doc/usage/run.md:310` -- 155 code points

Both exceed the 100-code-point width rule. CI runs the same script, so either these landed
while CI was red/skipped or the script gained coverage after they merged. Rewrap the two lines
(tables may need the definition-list bullet conversion described in `.claude/CLAUDE.md`).
