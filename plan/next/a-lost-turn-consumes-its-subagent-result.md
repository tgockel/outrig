# A turn that never lands consumes the subagent result it read

`SubagentRegistry::get_result` (`crates/outrig-cli/src/subagent/mod.rs`)
advances the subagent's delivery watermark when the tool call runs, but the
outcome only reaches the conversation when the turn finishes and its history
is committed. Until then it lives in rig's copy of the turn.

If the turn never lands -- Ctrl-C mid-turn, or a later model call that fails
(`plan/next/partial-turn-history-on-failed-model-call.md`) -- rig's copy is
dropped with the result in it. The watermark stays advanced, so asking
`outrig__get_result` again waits for a *newer* publication. For a subagent
that already published and went idle, none comes, and the call blocks until
the user interrupts again. The result is still in
`<session_dir>/logs/subagent-<name>.log`, so nothing is lost for good, but
the agent cannot see it.

The idle-without-publishing case is level-triggered and stays readable; only
a published outcome is lost this way.

## Sketch

1. Fix the cause: keep completed tool results across a turn that does not
   finish -- the hook-owned history route in
   `partial-turn-history-on-failed-model-call.md`, if it lives outside the
   future the REPL drops.
2. Or narrow the symptom: let `get_result` replay the last delivered outcome
   when asked explicitly, without changing ordinary once-only delivery.

## Acceptance

- A subagent publishes, the primary reads it with `get_result`, and the turn
  is then dropped before committing. A follow-up turn can recover that
  outcome instead of blocking.
- Two concurrent reads of one new version still deliver it once.
