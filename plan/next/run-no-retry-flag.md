# `outrig run --no-retry`, if scripted runs ask for it

`retry-budget-secs` deliberately shipped without a CLI flag. The existing
per-run overrides (`--max-tool-calls`, `--max-tool-result-bytes`) exist because
they are per-*task* decisions; a retry budget is a property of the endpoint,
which is what `[providers.<name>]` is for, and it does not change run to run.

The one case that does not fit is a scripted or CI run that would rather fail
fast than wait ten minutes for a window to reopen. Editing config for that is
awkward when the config is checked in.

If demand appears, the useful shape is the boolean, not the seconds:

    outrig run --no-retry     # equivalent to retry-budget-secs = 0

A `--retry-budget-secs <n>` would widen `RunArgs` -> `RunInnerArgs` -> the
resolution cascade -> the synthetic `ResolvedProvider` in `run.rs` for a knob
nobody tunes per run.

Not scoped, not scheduled. This note exists so the question is answered the same
way next time it comes up.

## Acceptance

- `--no-retry` on `outrig run`, resolving ahead of both the provider and
  top-level values.
- `doc/reference/cli.md` flag table and `doc/usage/run.md`.
- A `run_smoke.rs`-shaped test that the flag reaches the resolved provider.
