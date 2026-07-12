# Fix pre-existing doc-style audit violations

`python3 scripts/audit-doc-style.py` fails on trunk (exit 1) with two width violations
that predate task 0079:

- `doc/reference/cli.md:263` -- 109 cols (a table row in the `--image` flag table)
- `doc/usage/run.md:310` -- 155 cols (an error-message sample line)

CI runs the same script (`.github/workflows/ci.yml`), so either CI is currently red on
doc-style or the workflow doesn't gate on it; both are worth a look. Fix: wrap the table
row per the "convert to a definition-list bullet pattern" convention, and break the long
error sample across lines.
