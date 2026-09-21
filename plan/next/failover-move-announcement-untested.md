# The failover move announcement is unpinned

## Problem

0002-36 mitigated its own headline hazard -- one session can span two models -- in two places.
The banner names the fallbacks up front, and `FailoverModel::completion` announces each move as
it happens:

```
[outrig] model <a> failed (<reason>); trying <b>
```

0002-49 pinned the banner half (`crates/outrig-cli/src/cli/run.rs`, `render_banner` and its
tests). The announcement half is still unasserted. A refactor that drops the `eprintln!` leaves
a session silently switching vendors mid-reply with every test green, which is the exact
condition 0002-36 decided was unacceptable.

`plan/next/startup-banner-has-no-tests.md`, which 0002-49 consumed, raised this as a conditional
deliverable -- "if it can be reached without a second mock endpoint". It cannot, cheaply:
the line goes to process stderr from inside an async model call, so asserting on it in-process
needs either a global stderr redirect, which races every other test in the binary, or a writer
injected into `FailoverModel`, which is production surface existing only for a test.

## Sketch

Two options, neither obviously right:

- Extract the message into a `fn move_announcement(from, error, to) -> String` beside
  `render_banner`'s split, test the formatting directly, and accept that nothing proves
  `completion` still calls it. Cheap, and pins the wording but not the behavior.
- Assert at the binary boundary instead. `crates/outrig-cli/tests/anthropic_mock.rs` already
  stands two mock endpoints up in `tools_run_before_a_move_are_not_re_executed`; a sibling that
  drives the same chain through the real binary and reads its stderr would pin the behavior,
  at the cost of a process spawn.

The second is the one that holds the property. Worth doing when something else is already
paying for a binary-level failover fixture.

## Dependencies

- **Landed:** `plan/done/phase/0002-sidecars/tasks/0002-36-model-alias-failover.md`, decision 8.
