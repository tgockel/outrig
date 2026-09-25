# rig's own `INFO` lines reach the terminal in `run` and `run-new`

## Context

The CLI's tracing filter defaults to `info` for every target (`cli/app.rs`, `log_filter_spec`),
so rig's own instrumentation prints between the prompt and the reply:
`rig_core::agent::prompt_request::streaming: Current conversation Turns: 1/52`, an `execute_tool`
span carrying the tool's arguments and result, and `Agent run finished`. Seen while testing
`outrig run-new` in `0003-05`. `outrig run`'s loop makes the same `prompt(..).extended_details()`
call, so it shows the same lines. They read as noise, and they repeat on stderr the source that
`run-new` already shows.

## Shape

A default directive such as `info,rig_core=warn`, still overridable through `OUTRIG_LOG` and
`RUST_LOG`. It changes `run`'s output as well, which is the reason it was not folded into
`0003-05`.

## Acceptance

- At the default filter, neither command's stderr carries a `rig_core` line; `OUTRIG_LOG=debug`
  still shows them.
