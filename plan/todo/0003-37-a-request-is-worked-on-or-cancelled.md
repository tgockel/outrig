# 0003-37 -- A request is worked on or cancelled

## Context

`0003-30` gave a child a rule for when the host opens a round on it (`agent-classes.md`, "The
child's rounds"; `crates/outrig/src/agent/runner.rs`). When a round ends, the host compares what
the child holds open -- the requests unread on its channels, and those it took and has not
answered; a work child's one submission -- with what its last round opened on, and opens another
round only when they differ. What that leaves when a child does nothing with a request:

- **A request child that took a request** is asked twice: the round its arrival opened, and one
  that names the request by id and how long it has waited. When that round also ends without a
  reply, nothing changed, so no round opens. The child goes idle with the request open. The
  sender's handle is unfinished and its status reads "child idle" until the sender cancels it, the
  instance is released, or a later arrival opens a round that answers it.
- **A request the child never took** is asked once. Its arrival opened a round, that round ended
  with it unread, and nothing changed, so it waits unread on an idle child in the same way.
- **A work child** is asked twice, and when the second round ends without a completion, its
  submission ends `CompletionRejected` ("ended without completing"). The caller learns of it
  (`work.md`, "A request settles once"; `typed-agents.md`, "Validation and repair").

The maintainer's goal for messages is that they are reliable in how they are processed. A request
is either being worked on -- the LLM responsible for it is acting on it -- or it is cancelled, and
the sender is told either way. There is never a case where an LLM is idle while it holds a request
it owes an answer to. The first two endings above break that: the request stays open and its
sender is told nothing. Decided on 2026-10-26.

This task was `potential/one-shot-nudging.md`, which asked whether to prompt a work child more than
once before its call ends, and what an evaluation would show: the completion rate against the
rounds spent. It offered three shapes: further rounds of the same prompt, a round that asks what
blocks completion, and a `fail(message)` that names the blocker. The maintainer decided the rule
below instead of running the evaluation, and it covers request children as well as work children.
The second round changes the text to say what happens next, and `fail` is where a child names its
blocker, so the first two shapes are not built.

### Why a model ends a round without replying

No run against a real model is recorded in the repository, so these come from what a child is told
(`orientation.rs`) and from how models behave in an agent loop, not from measurement. What the task
does about each:

- **It is waiting on something.** A child that has a build or a test run to wait for waits with
  `runtime.wait`, inside the round. A round that ends with text is not a wait: nothing opens a
  round when what the child waited on finishes. The warning round says so.
- **It cannot or will not answer, and says so in prose.** A request child has `d.fail(message)`
  for this, and the preamble names it. The model has to use it, and the warning names it again.
- **It writes the answer as its closing text.** The host does not deliver a child's closing text
  to its sender, so this is the model doing the wrong thing. It is asked twice more and then the
  request is cancelled, and the preamble says that only `d.reply` and `d.fail` answer.
- **It reads the nudge as status.** The first nudge is validly a status line: it says what the
  child took and has not answered. A child that gets it and still ends its round without acting
  gets a second nudge in plainer terms: if this round ends without a `runtime.wait` or a reply,
  the outstanding requests are cancelled.
- **Its instructions describe a different job.** The parent's code writes the first part of a
  child's instructions, and "run the check" or "write the file" can read as work with no answer
  owed. It is never so: a parent that asks for work looks at how it went, and even a function that
  returns `None` has a result the child sets. A cancel gives the parent an exception in its own
  code, from which it can look at the effect -- whether the file exists -- so a cancel is the right
  end even when the work was done.
- **Its turn is empty, or its round used its tool calls.** A turn comes back empty when a model
  spends its output on reasoning, and a round stops early at the tool-call cap. Neither needs
  handling of its own. The round ends like any other, and the warning round that follows is the
  child's chance to fail the request with a message saying it cannot solve it.

## Goal

A request sent to a child is either being worked on or cancelled, and its sender is told which:
no child is idle while it holds a request it has not answered.

## Deliverables

- **Two rounds that change nothing, then a cancel.** `runner.rs` keeps, beside the baseline,
  whether the warning round has been given since the child's summary last changed. When a round
  ends without an error, the host compares the live summary with the one the round opened on:
  - *Nothing in it:* the child is idle. This is the only way a child becomes idle.
  - *Different* -- a reply, a `fail`, a receive, an arrival, the caller's cancel, a release: an
    ordinary round on the live summary, as `0003-30` decided, and the warning is available again.
  - *The same, and holding something* -- the child changed nothing: the first time, a warning
    round; the second time, the host cancels every request the summary holds, by id.

  After a cancel the host computes the summary again. Empty, the child is idle. Anything left -- a
  submission queued behind a work child's, or an arrival that came in as the host decided -- gets
  its round at once, and the child is not marked idle in between. A round that ends in an error is
  not a round that changed nothing: it neither gives nor uses the warning, and the decision after
  the wait is `0003-30`'s, against the baseline from before the round. A host-opened round counts
  no attempt against any request. So a request child that takes a request and never answers is
  asked three times -- the round its arrival opened, the round that names it, and the warning --
  one that never takes a request twice, and a work child three times. `0003-30`'s rule asks two,
  one and two, and leaves the first two open.
- **The warning's text.** The first nudge is `0003-30`'s: a request child's names each request it
  took by id, channel and how long ago, and counts what waits unread; a work child's asks for the
  result by the submission's id, without its closing clause, which says the next round's end fails
  the submission and is no longer true. The warning is a second text. It names what is
  outstanding; says that a round's end is not a wait, since nothing opens a round when what the
  child waited on finishes; gives the ways out -- `await d.reply(value)`, `await d.fail(message)`,
  `runtime.wait`, and for an unread request `receive()` first, since `runtime.wait` raises
  `MessageAvailable` while one waits unread; and says that if this round ends with the request
  still open it is cancelled and its sender gets an exception. Drafts, for the task to tune:

  > [outrig] your last round ended with requests unanswered: you took request 7 from
  > runtime.channels["fizz"] 12s ago; 2 requests are waiting unread on runtime.channels["fizz"].
  > A round's end is not a wait: nothing opens a round when what you waited on finishes. Answer a
  > request with `await d.reply(value)`, end one you cannot answer with `await d.fail(message)`,
  > take an unread one with `await runtime.channels["fizz"].receive()`, or wait for what you need
  > with `runtime.wait`. If this round ends with any of them unanswered, they are cancelled and
  > each sender gets an exception.

  > [outrig] submission 12 is still open: your last two rounds ended without completing it.
  > Complete it with `await runtime.complete(value)`, `value` a `Total`, or wait for what you need
  > with `runtime.wait`. A round's end is not a wait: nothing opens a round when what you waited
  > on finishes. If this round ends with it open, it is cancelled and its caller gets an
  > exception.
- **The preambles say it.** A request child's names the rule: a round that ends with a request
  unread or unanswered is followed by up to two more, the last warning that the requests are
  cancelled if it ends the same way. It says that ending a round is not waiting, and to wait with
  `runtime.wait`; that what the child writes itself is not delivered to the sender, and only
  `d.reply` and `d.fail` answer; and that code it leaves running when it stops writing does not
  keep a request open. A work child's states the same rule where it now says "followed by one
  more, and if that one ends with it open too, the submission fails".
- **The end is a cancel, and the sender is told.** A request the host cancels ends `cancelled`:
  the sender's `await` raises `RequestCancelled`, whose message names the request and says the
  child ended its rounds without answering it, or without reading it; `h.status` reads as after
  `h.cancel()`. An unread request is taken out of the child's queue first, as `h.cancel()` does,
  so the child never takes what was cancelled before it did. A received one stays with the child,
  and a `reply` or `fail` on it afterwards raises `RequestCancelled`, whether it comes from the
  child's next round or from a task the child left running. In `interpreter.py`, `_HOST_ENDS`
  gains `"cancelled"`, and the handler withdraws as `cancel()` does before it ends the request.
  `agent.request.cancelled` says the host cancelled it and why, beside `received`, so the record
  tells this cancel from a sender's; the new payload field is in `crates/outrig/public-api.txt`.
  `RequestCancelled`'s docstring in `_agents.py` says the host cancels with it too.
- **A work child's end is this cancel.** What `0003-30` ends `CompletionRejected` with, "ended
  without completing", ends `cancelled`, with the consequences it had: a decorated call's child is
  released; an explicit work child stays idle, and a submission queued behind the cancelled one
  opens its own round next; the skill parser is a decorated call, so `invoke` raises
  `RequestCancelled` and nothing is called. `CompletionRejected` then means one thing, answers
  that failed their check past the attempt limit, and its docstring drops "or a work child's
  rounds ended without completing".
- **Idle means nothing is outstanding.** The runner marks a child idle only after computing an
  empty summary, which is also the case after a cancel, and `Registry::idle` asserts that the
  child holds nothing open. Ending what a child holds for a terminal model failure, a spent
  budget, a release or the close is unchanged, and so is the retry, after a wait, of a round whose
  model call fails.
- **Known limit.** An arrival is a change, as `0003-30` decided, so requests that keep arriving at
  the pace of the child's rounds keep restarting the count for the older ones. The child is not
  idle while they do, and `requests-max` bounds what accumulates. A count per request would end
  the oldest regardless of arrivals; it is not built.
- **Unchanged.** The primary and its `user` channel: plain messages owe no reply and have no
  sender to notify. The per-request attempt limit, 3, and the `agent.request.invalid` record.
- **Where the rule is written, brought to match.** `agent-classes.md`, "The child's rounds",
  holds the rule in full, with the traces it was decided on and now this one. `work.md` ("A
  request settles once"), `typed-agents.md` ("Validation and repair"), `skills.md` (the parser's
  end) and `messages.md` state it by reference. The pointers `0003-37` left in `work.md`,
  `typed-agents.md` and `agent-classes.md` give way to the rewrite. Also the module comment of
  `runner.rs`, and the `[Unreleased]` paragraph of `crates/outrig/CHANGELOG.md` that says a request
  left unanswered "earns one more" round and a work child "ends its submission
  `CompletionRejected`".

## Acceptance

Each is a mock-driven test unless it says otherwise.

- **A request the child took and never answers is cancelled after three rounds.** The mock takes
  it and ends every round with text. It is called for the round its arrival opened, the round
  whose opening names the request by id and how long it has waited, and the warning round, and for
  nothing after. The handle ends `cancelled`; `await` raises `RequestCancelled`, whose message says
  the child ended its rounds without answering; the child reads idle; `agent.request.cancelled`
  names the host and carries `received`; no `agent.request.invalid` is recorded; and a `reply`
  from the mock's code afterward raises `RequestCancelled`.
- **A request the child never takes is cancelled after two.** The mock ends its rounds with text.
  It is called for the arrival's round and the warning round, whose opening says to take the
  request with `receive()`. The request is out of the child's queue, `pending()` is 0, and the
  mock never saw its body.
- **A reply in the warning round, or a `runtime.wait` it has not reached the end of, is not
  cancelled.** A mock that replies in the warning round has its request answered with the value.
  One that calls `runtime.wait` in it, and replies when the task it waits on finishes, has its
  request answered, since its round has not ended.
- **Anything the child changes restarts the count.** With two requests, a mock that answers one
  and ends the round gets an ordinary round next, as `0003-30` decided. If it then changes
  nothing, it gets the warning and then the cancel for the other, and the first stays answered.
- **A round's end in an error is not an unchanged round.** A model call that fails before the code
  ran opens the round again with the same opening and cancels nothing. One that fails after it ran
  is decided on what is open then, and cancels nothing by itself.
- **A round stopped early counts as ended.** A nudge round stopped at the tool-call cap is
  followed by the warning round, in which the mock ends the request with `await d.fail(...)`; the
  handle ends `failed` with that message, not `cancelled`.
- **A work child is cancelled after three rounds.** The mock never completes. The submission ends
  `cancelled`, not `CompletionRejected`, and `await` raises `RequestCancelled`. A decorated call's
  child is released. An explicit work child reads idle, and a submission queued behind the first
  opens its own round with no idle state seen in between. A skill parser that never completes
  makes `invoke` raise `RequestCancelled` and calls nothing.
- **A host program's handle sees it.** `0003-35`'s `await h.future` raises `RequestCancelled` for
  a request the host cancelled.
- **No child is idle holding a request.** A test that drives every path that ends a round -- the
  cases above, a terminal model failure, a spent budget, a release and the close -- and reads
  `child.status` and each handle as it goes, finds `idle` only when every handle sent to the child
  has ended. `Registry::idle` carries the assertion, so every other test checks it too.
- **The texts.** The first nudge is unchanged for a request child, and a work child's loses only
  its closing clause. The warning for each kind names what is outstanding and each way out, and
  says that ending the round cancels. Both preambles state the rule, and a request child's says
  that its own text is not delivered.
- **The tests that pin the old ending are rewritten**, by name at least
  `a_work_child_that_ends_two_rounds_without_completing_fails`,
  `an_unchanged_state_opens_nothing_and_a_cancel_ends_the_request`,
  `an_unread_request_earns_no_round_past_the_first_that_counts_it` and
  `a_new_arrival_opens_a_round_naming_the_old_requests_too` in `children_tests.rs`, the typed-call
  test in `typed_tests.rs` that ends a submission `CompletionRejected` for ending without
  completing, and the agent-class tests `0003-34` adds that assert an idle child with an open
  request.
- `cargo test`, `cargo clippy --all-targets -- -D warnings` and `cargo fmt -- --check` pass;
  `crates/outrig/public-api.txt` is regenerated and the `public-api` gate passes.
- The design pages and the `[Unreleased]` changelog paragraph say what the code does, checked by
  reading. No `doc/` page changes here; `0003-38` writes them.

## Design forks

1. **Whether a work child gets `runtime.fail(message)` -- Open; recommended: yes.** A request
   child ends a request it cannot answer with `await d.fail(message)`, and its sender gets
   `AgentRequestFailed` carrying the child's reason. A work child has only `runtime.complete`, so
   one that cannot complete has no way to say so: its parent learns only that rounds ended without
   a completion, and the warning offers it `runtime.complete` and `runtime.wait` and no third way
   out. `runtime.fail` would end the submission `AgentRequestFailed`, an end that already exists,
   and add one call to the work child's `runtime`, its `help()` text, its preamble and the
   warning. The kinds still do not mix: a work child gets no request channels and no `reply`. The
   third shape of the entry this task was is this call.

## Dependencies

- **Hard: `0003-30`.** The rule, the handle and the request ends this task changes.
- **Hard: `0003-31`.** A decorated call's child, released when its call ends, and its end, which
  changes from `CompletionRejected` to this cancel.
- **Hard: `0003-34`.** An agent class instance is the request child whose idle ending changes, and
  its acceptance asserts the old ending, which this task rewrites.
- **Hard: `0003-35`.** The cancel must reach a handle a host program holds.
- **Soft: `0003-33`.** `/name`'s parser is a decorated call, so its end changes with `0003-31`'s.

## See also

- `plan/phase/0003-python/agent-classes.md` -- "The child's rounds", where the rule `0003-30`
  shipped is written in full, with the traces it was decided on.
- `plan/phase/0003-python/work.md` -- "A request settles once" and "The handle".
- `plan/phase/0003-python/typed-agents.md` -- "Validation and repair".
- `plan/done/phase/0003-python/tasks/0003-30-a-parent-awaits-its-childs-typed-result.md` -- its
  `## Decisions` for the rule this task replaces.
- `crates/outrig/src/agent/runner.rs`, `orientation.rs`, `children.rs` and `children_tests.rs`;
  `crates/outrig/src/python/interpreter.py`, `_HOST_ENDS`.
