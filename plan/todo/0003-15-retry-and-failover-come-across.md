# 0003-15 -- Retry and failover come across from the 0.2.x loop

## Context

`0003-04` copied the minimum a round needs and deliberately left retry and failover behind, so the
CLI could run sooner. This finishes the copy. Until it lands, `run-new` is less resilient than
`run`: a rate-limited provider ends a round that the legacy loop would have recovered.

`harness-components.md` lists `llm/retry.rs` and `llm/failover.rs` among the files copied rather
than moved, because the 0.2.x line actively edits them and a copy is what keeps those merges
clean. `llm/mistralrs.rs` and `llm/registry.rs` are explicitly **not** copied: the in-process
backend is deprecated, its removal is `plan/next/remove-deprecated-local-llm.md`, and omitting it
avoids `mistralrs-core`, `hf-hub`, and `candle-core` becoming library dependencies.

Two defects already filed against the originals are worth carrying rather than reproducing.
`plan/next/clamped-ceiling-is-silent.md` records that the effective `max-tokens` never escapes
`build_agent`, so nothing else in the process can see the ceiling that applies. And
`plan/next/chain-attribution-names-the-first-candidate.md` records that `ModelLabel` names
candidate one, so a failover mid-round changes which model answered without changing the label.

## Goal

`run-new` survives a transient provider failure and a failed candidate as well as `run` does, and
knows which model actually answered.

## Deliverables

- `retry.rs` and `failover.rs` copied into `outrig`'s private agent module, with the error type
  converting at the boundary rather than carrying rig's.
- **The in-process backend is not copied.** Neither is the registry it needs.
- **The exported effective ceiling kept correct across a candidate switch.** `0003-04` exports it
  at the point it is resolved and `0003-13` records it; what this task adds is that a failover to
  a different candidate updates it rather than leaving the first candidate's number standing.
- **Attribution that names the model that answered.** `failover.rs`'s private `Abandoned` carries
  the hop; today the chain's state is recovered by prefix-matching the rendered error string,
  which a structured channel replaces.
- The tests come across with the code. A copy without its ~4,900 lines of tests is a copy whose
  behavior nobody can check.

## Acceptance

- A mock provider returning 429 and then succeeding: the round completes, with the retry recorded
  rather than only printed.
- A failover chain whose first candidate is unreachable resolves to the second, and **the
  attribution names the second**, not the first.
- An exhausted chain ends the round with the reason, and the conversation is retained per the
  existing wording, not discarded.
- **The end-to-end checks `0003-12` and `0003-13` could not run**, because this is the task that
  adds failover: a view rebudgeted against a smaller candidate's allowance after a real hop, and
  retry and failover events emitted into the stream whose schema `0003-13` defined.
- The effective `max-tokens` is readable outside `build_agent`, asserted rather than assumed.
- `git diff` shows no change under `crates/outrig-cli/src/llm*`.
- `crates/outrig/public-api.txt` regenerated and clean -- nothing here should widen it.
- `cargo test --workspace`, `cargo clippy --all-targets`, `cargo fmt --check` pass.

## Design forks

1. **Whether to fix the attribution defect here or carry it -- Recommended: fix.** It is cheap in
   a fresh copy and expensive later. If it turns out larger than it looks, leave
   `plan/next/chain-attribution-names-the-first-candidate.md` in place and say so. The ceiling
   defect is not a fork here: `0003-04` owns the export, and this task only keeps it accurate
   across a hop.

## Dependencies

- **Hard: 0003-04.** There is no loop to extend until the minimum is copied.
- **Hard: 0003-13**, and through it `0003-12`. This task's acceptance runs the end-to-end checks
  those two deferred -- rebudgeting after a real hop, and emitting retry and failover events into
  the stream whose schema `0003-13` defined -- so the declared graph should say so rather than
  relying on numeric order to make it true.

## See also

- `plan/phase/0003-python/harness-components.md` -- what is copied and what is deliberately not.
- `plan/next/clamped-ceiling-is-silent.md` and
  `plan/next/chain-attribution-names-the-first-candidate.md` -- the two defects to fix rather than
  reproduce, and `plan/next/remove-deprecated-local-llm.md` for the arm that is never copied.
