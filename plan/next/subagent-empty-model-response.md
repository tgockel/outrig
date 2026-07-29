# Subagents fail with an empty model response

## Symptom

Observed while dogfooding 0104: three review subagents were launched through
`outrig__subagent`, all three started successfully, and all three eventually failed with an
empty model response. The parent session was otherwise healthy -- its own turns, and every MCP
tool call, kept working through the same provider.

No reproduction yet. It is not known whether the empty response came back from the provider or
whether the subagent loop produced it (a request built without messages, a stream closed before
the first token, a result read after release).

## Why it matters

`outrig__subagent` is the one tool surface that fans work out, so a silent empty result is the
failure mode most likely to be mistaken for "the model had nothing to say" rather than a defect.
It also lands right where 0099-0101 are working, and any of those could either fix it by
accident or bury it further.

## Diagnosis path

- `outrig logs <session> <server>` for the affected session distinguishes the two cases: a
  provider-side empty completion appears in the transcript, a loop-side one does not.
- Whether the failure correlates with concurrency (all three were in flight together) or with
  elapsed time -- "eventually failed" suggests a timeout or a release racing the read, both of
  which 0099's partial-failure work touches.
- Whether the parent surfaces a distinguishable error at all, or collapses "no content" and
  "request failed" into the same empty string.

## See also

- `plan/todo/0099-subagent-release-partial-failure.md` -- release atomicity, same subsystem.
- `plan/todo/0101-subagent-model-selection.md` -- would add a second provider path to this.
