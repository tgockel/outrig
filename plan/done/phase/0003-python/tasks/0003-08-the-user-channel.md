# 0003-08 -- The user reaches the agent through a channel, not a prompt

## Context

`0003-05` shipped a CLI that passes a typed line to the model as an ordinary prompt. That is the
arrangement this phase exists to replace. `README.md` lists the `user` channel as a deliverable:
messages are announced to the model by name and count, and reading one is an act the generated
code takes.

`messages.md` designs the layer for more than one relationship so the shape does not have to
change when subagents arrive, but only the `user` channel is built here. The endpoint offers three
operations -- `receive`, `send`, `pending` -- and `pending()` is what makes notification possible
without disclosure: the host can tell a model that three messages are waiting without putting any
of them in its context.

The distinction between the model's own text and a channel send is worth keeping straight. Model
text is running commentary. A send is a deliberate message, and it is the only one available to
code running after a round has ended -- which is what makes it more than a second way of printing.

## Goal

A typed line arrives as a message the agent's code reads, and the agent can send to the user from
code that runs after it stopped writing.

## Deliverables

- Endpoints in the interpreter: `receive`, `send`, `pending`, reachable as
  `runtime.channels["user"]`.
- The `msg` and `send` protocol messages, carrying the agent id like everything else.
- **An input pump that reads while a round runs**, which the existing REPL does not do. Its
  `select!` races *reading a line* against an interrupt, and then the round callback runs with
  nothing polling stdin -- so a line typed mid-round is buffered by the terminal and delivered
  only after the round ends. Redirection through `runtime.wait` is unreachable until this changes.
  Reuse the terminal helpers; do not reuse the blocking callback lifecycle.
- **One owned round driver.** Input arriving mid-round enqueues a message on the channel. It must
  not start a second model loop.
- The REPL routing a typed line into the channel rather than into a prompt, and rendering a `send`
  to the terminal, including a send from a background task while a round is running -- two writers
  to one terminal need a stated arrangement.
- **Announcement by name and count.** The model is told what is waiting, not what it says.
- The contract enforced: the `user` channel carries text both ways, and construction validates
  against the serializable subset rather than failing at first send.
- **`receive()`'s shape decided.** `messages.md` marks this as a proposal rather than a settled
  call -- body-only with a separate `receive_delivery()` for attribution, against one method
  always returning an envelope -- and says it should be decided before the first channel task is
  written. This is that task.

## Acceptance

- A typed line reaches the agent as a message its code reads, and the agent's reply reaches the
  terminal through a send.
- **Typed input reaches the channel while a round is running.** Through the CLI test seam: with a
  round in flight, type a second line *without* Ctrl-C and observe it arrive as a message the
  agent can read. A test that posts a message to the interpreter directly passes while the input
  pump is broken, which is why this one drives the seam. The fuller claim -- a wait yields to the
  input, its operation survives, and the same round continues -- needs `runtime.wait`, so it is
  `0003-09`'s acceptance rather than this task's.
- **`pending()` reports a count without the body entering the model's context.** Checked by
  asserting on what the model was actually sent, not by reading the code.
- Receiving consumes; notification does not. Reading twice does not deliver the same message
  twice, and declining to read leaves it queued.
- A send from a background task, after the round that started it ended, reaches the user.
- Ordering holds within the channel.
- A contract declared outside the serializable subset is refused at construction, with the
  offending type named.
- `cargo test --workspace`, `cargo clippy --all-targets`, `cargo fmt --check` pass.

## Design forks

1. **`receive()` versus an always-enveloped receive -- Open, and this task closes it.**
   `messages.md` argues the common case stays readable if the body comes back bare, against
   uniformity. Whichever is chosen goes into the page as a decision rather than a proposal.
2. **What the model is told when a message arrives mid-round -- Recommended: the announcement,
   not the body.** Anything else undoes `pending()`.

## Dependencies

- **Hard: 0003-05.** There is no REPL to route from until the command exists.

## See also

- `plan/phase/0003-python/messages.md` -- channels, endpoints, contracts, delivery rules, and the
  undecided `receive` shape.
- `crates/outrig-cli/src/repl.rs` -- where a typed line currently becomes a prompt.

## Decisions

- **`receive()` always returns a `Delivery` (fork 1, the maintainer's call).**
  - `receive()` returns a frozen dataclass: `.body`, `.sender` (`"user"` on the user
    channel), and `.received_at`, a UTC `datetime`.
  - There is no `receive_delivery()`. `messages.md` recorded a body-only `receive()` beside it as
    a proposal, and now records this as the decision.
  - The envelope was chosen for uniformity: one take with one shape, so code that later needs
    the sender does not change the call it makes. The cost is `.body` on every simple use.

- **Mid-round arrivals are announced, not delivered (fork 2, as recommended).**
  - A round opens on `[outrig] N message(s) is/are waiting on runtime.channels["user"].` as its
    prompt.
  - A message arriving while a call runs is announced as `[N ... waiting ...]` at the head of
    that call's result. The notice sits outside the result's bound, so truncation cannot cut
    it.
  - Every announcement is the total unread, not the new ones, so any later one covers an earlier
    one that went missing.
  - No body enters the model's context unless code the agent wrote prints it.

- **stdout carries only sends; the model's own text goes to stderr (the maintainer's call).**
  - messages.md already treats model text as commentary and a send as the deliberate message.
  - So `run-new > out.txt` keeps exactly what the agent meant the user to have.
  - Commentary is unmarked on stderr, beside the Python source and notes.

- **Where the queue lives, and why its answers come from the reader thread.**
  - Messages from the host are queued in a lock-guarded `deque` on the interpreter's reader
    thread, not through the agent's loop. So `pending()` and the host's `pending` query are exact
    even while the loop is wedged, and a `pending` asked after a `msg` always counts it: the wire
    is FIFO.
  - Receivers are futures woken with `call_soon_threadsafe`, each on its own loop. The wake goes
    to every waiter, and each re-checks, so a receive cancelled after its wake loses nothing.
  - `msg` is always answered: with the new count, or with why it was refused, having queued
    nothing.
  - The append is the last step that can fail, so `_handle`'s retry-on-`MemoryError` never
    queues a message twice. What follows the append (the wakes and the answer) is guarded, never
    re-raised.

- **Bounded both ways, and overload is refused rather than dropped.**
  - A channel holds 256 unread messages (`QUEUE_MAX`), and a message is at most 1 MiB
    (`MESSAGE_MAX`).
  - The host refuses an oversized post before sending it. The interpreter raises `ValueError`
    in the code that sends one, since it bounds everything it writes.

- **Contracts are validated at construction, against the whole serializable subset.**
  - `Endpoint(..., receives=..., sends=...)` refuses anything outside str, bool, int, float,
    None, `list[T]`, `dict[str, T]`, unions, and dataclasses built from those. The refusal names
    the offending type, and the field and class it sits in when nested.
  - Unresolvable annotations are refused the same way. Self-referential dataclasses are
    accepted.
  - Values are checked by exact type (`True` is not an `int`, finite floats only), which runs no
    agent code on the reader thread.
  - Only the host is ever at the other end today, carrying JSON. Encoding a dataclass across a
    channel is left to the first channel that carries one.

- **A send never tears a protocol line.** `_send`'s locked write moved into `_write_line`, which
  joined `_MACHINERY`. An interrupt landing mid-send is then dropped at that frame, not raised,
  since a half line would glue the next reply to it. `Endpoint.send` encodes outside
  `_with_room`, so running out of memory while building the agent's own message is the agent's
  error.

- **What counts as new is the interpreter's to say (`agent/channel.rs`).**
  - Each endpoint counts every message ever delivered to it, and `pending` answers with that
    count beside what waits.
  - `Announcer` keeps only two watermarks over the delivered total: `announced` (by the round in
    flight) and `kept` (by the last round that ended well). A failed or dropped round leaves
    `kept` behind, so its announcement, which the model may never have read, is made again.
  - `round()` asks every time. It returns `Ok(None)` without a model call when nothing was
    delivered past `kept`, or when what was delivered has already been read (a background task
    consuming messages, say). That is what stops a message the model declined to read from
    starting round after round.
  - The tool asks after every call. The answer comes from the reader thread in well under a
    millisecond, beside a model call that takes seconds, and it is bounded at 2 s, as a CPU
    reading is.
  - The first cut counted posts on the host, with a third counter and a drop guard to roll
    back. `/simplify`'s altitude review moved the count into the interpreter, where messages
    land:
    - a refused post is no longer counted as an arrival;
    - `UserChannel` no longer shares the bookkeeping;
    - the rollback, which nothing could observe, is gone;
    - a channel between two co-hosted agents, which never reaches the host, will be counted the
      same way.

- **`UserChannel::send` queues eagerly and returns a future for the answer.**
  - The message is on the wire when `send` returns, so a round started right after it counts it,
    and the CLI never awaits a post inside its loop. The answer comes from the reader thread,
    which native code holding the GIL stops, and a loop stalled there could not relay a Ctrl-C.
  - Answers are collected in a `FuturesOrdered`. A line typed mid-round prints `[outrig] queued
    for the agent (N waiting)` when its answer comes; a refusal prints an error.

- **One round driver, and a `select!` loop in place of `Repl`'s callback.**
  - `converse.rs` owns the only round future (over the agent's `tokio::sync::Mutex`, as before),
    the pump, the agent's sends, the answers, and SIGINT, in one task that writes all output
    in whole lines.
  - A send at the prompt breaks the prompt's line and redraws `> ` after it. A half-typed line
    stays in the terminal's buffer.
  - After a round returns `Some`, another is tried at once, for anything that arrived after its
    last tool result. After an error nothing is retried, since a persistent failure would loop.
    After a Ctrl-C that dropped the round nothing starts until the user types again.
  - `repl.rs` changed only by making `write_stderr_line` and `INTERRUPT_NOTICE` `pub(crate)`, so
    `run`'s harness stays merge-clean with the 0.2.x line.

- **Stdin is read on a `std::thread`, not through tokio.** With a read always in flight, tokio's
  blocking stdin read holds the runtime's drop until the user presses Enter, which is what
  `error.rs` documents for `run`. A second Ctrl-C at the prompt and the interpreter exiting both
  hung. Mutation-checked: moving the loop into `spawn_blocking` fails both exit-path e2e tests.

- **When the interpreter exits, the session ends, with status 1.** The agent's outbox closes the
  moment the interpreter's output does, and once no round runs, `run-new` says so and exits.
  Every line typed after that point would go nowhere.

- **`AgentError`'s texts no longer say "resend the prompt".** With a channel that would queue a
  duplicate. They say what is still waiting and to send another message.

- **The pump is covered only under the e2e feature (the maintainer's call).**
  - The acceptance test drives the binary, in `run_new_e2e.rs`, run by the `live-e2e` CI job, so
    `cargo test --workspace` never runs it.
  - The driver stays concrete over `PythonAgent` rather than growing a trait for a fake.
  - Mutation-checked: a pump that reads only between rounds fails the test in its 30-second
    step.

- **What `/simplify` changed, beyond the move of the counts above, and what it left.**
  - Changed:
    - the contract errors read a class's name without its metaclass (`_type_qualname`, beside
      `_type_name`);
    - the unused `Endpoint.name` is gone, and `receives`/`sends` stay, tested, for the
      reflection `messages.md` describes;
    - `Interpreter::query` takes a kind again;
    - `converse.rs` writes a send through `write_stderr_line` and lets `FuturesOrdered` infer
      its type;
    - the test helpers are shared: `ask`, `refused`, `text_of`, `Running::reached`,
      `wait_until`, and `Session::exit`;
    - the double Ctrl-C check rides on the mid-round e2e test instead of launching a container
      of its own.
  - Left:
    - slash-command and prompt handling shared with `repl.rs`, which would mean editing
      `Repl::run_with` (the merge constraint);
    - caching a dataclass's field types for `_conforms`, since no live contract carries one;
    - making the tool's notice a status line inside `render`, which would give the same bytes
      through a changed signature;
    - a take-once `UserChannel` without the shared receiver;
    - a writer task for the terminal, which does not matter at a person's pace (the unbounded
      outbox it was listed beside did matter, and the review's fix below bounds it);
    - rejecting an oversized `str` before encoding it, a memory spike inside the ceiling;
    - waking one receiver instead of all, since wake-all is what keeps a cancelled receive from
      costing a wakeup.

- **The review rejected the first cut on four findings.** Each was reproduced by a test that
  failed against the code before it was fixed.
  - **A completed call's outcome was lost to a round dropped while asking what waits.**
    - `settle` had consumed the execution, and the tool then awaited the interpreter's
      answer. A Ctrl-C there dropped the round, so the call read "had not returned" and its
      outcome was never reported.
    - The outcome is now held in a guard (`Unhanded`) until the result is handed back. If the
      round is dropped first, the guard keeps it as a late result, which the next call reports
      as it reports any reply nobody waited for.
    - The test withholds the interpreter's next `pending` answer and drops the round when it
      is asked.
  - **Running out of memory after a message was queued queued it twice.**
    - The wake loop's iterator and the `contextlib.suppress(...)` built in its `except` could
      raise past the append, into `_handle`'s whole-message retry.
    - Everything after the append is now a plain `try`/`except BaseException` with the
      iterator taken first, in `_wake_all`, and `_msg`'s answer likewise.
    - The test fails the reader's next `suppress` once, with a receive that cannot be woken.
      Before the fix it got two deliveries for one post.
  - **The host held every message the agent sent, without bound.**
    - `MESSAGE_MAX` bounded one message and the memory ceiling covers only the interpreter,
      so a loop of sends at code speed grew the host's memory as fast as it could.
    - Now each message is acknowledged (`received`) when the user end takes it, and an
      endpoint sends at most `SEND_WINDOW` (16) messages ahead of those acknowledgments.
      Past that, `send` waits in the agent's own code, under its ceiling, so the host holds at
      most 16 MiB per agent.
    - A message the host cannot hand to anyone is acknowledged at once, so the agent does not
      wait on a receive that will never come.
    - The cost is backpressure a library caller has to know about: it must receive while a
      round runs, which `UserChannel::receive` says. It is backpressure rather than refusal
      because a refused send is one the agent would have to retry, and the window already
      makes it wait for the reader.
  - **A steady stream of the agent's messages starved the round and the input.**
    - `select!` was `biased` with the agent's messages above the round and the pump. Behind a
      terminal slower than the agent, one was always waiting, so neither the round nor
      `/quit` was polled.
    - The branches are now picked at random among those ready.
    - The e2e test reads stdout slowly, so a message is always waiting, and needs the round's
      reply and `/quit` to get through. With `biased` restored it fails; with a fast reader it
      passed either way, since the window alone let the queue empty between refills.

- **A second review found two more.** Both were reproduced before the fix.
  - **Input ending dropped what the agent had already sent.**
    - `converse` returned at end of input without taking the messages already waiting on the
      host, and fair selection meant nothing ensured they were taken first.
    - At end of input it now shows what is waiting at that moment, through
      `UserChannel::receive_waiting`: a finite snapshot, each message acknowledged, without
      waiting for a task that goes on sending.
    - `/quit` and a second Ctrl-C still leave at once: the user asked to go.
    - The reported path, a foreground burst still queued as its round ends, would not
      reproduce. The loop polls the round only when it picks it, and behind a slow terminal each
      pick costs a write, so the round ended only after the burst had drained. That slowness is
      filed as `plan/next/run-new-round-moves-at-the-terminals-pace.md`.
    - The same exit path loses a background burst, which reproduces every time: twelve messages
      sent at once, input ended once the first is shown, five of twelve shown before the fix.
  - **Running out of memory while taking in an acknowledgment lost its room for good.**
    - The first fix made `_received` raise nothing, and so swallowed a failure *before* the room
      was made: nothing changed, nothing retried, and each such acknowledgment took one of the 16
      places for good.
    - Now only preparation raises, with nothing changed, and the reader's `_handle` retries it on
      the reserve. After the room is made nothing raises.
    - A send that fails to go out gives its room back through `_with_room`.
    - Tested with a list of waiting sends whose `copy()` fails once, on both paths: before the
      fix the waiting send never went out, and the failed send kept its room.

- **A third review left two comments, both taken.**
  - **End of input could hide a refusal.** The graceful exit showed what the agent had sent
    but not the answers to lines typed before input ended, so a line refused just before --
    one past 1 MiB, say -- could go unreported. Those answers are now shown first, waited
    for at most `ANSWER_GRACE` (2 s): the reader thread gives them at once, and a line refused
    on the host has its answer already. This one could not be reproduced from outside. A
    refused answer is ready at once, and ten oversized lines typed ahead of the end were
    reported every time. The fix rests on the code, the exit that ignored the answers, and
    `lines_refused_as_input_ends_are_still_reported` guards the behavior.
  - **Taking what waits could wait behind a listener.** `receive_waiting` locked the receiver
    that an outstanding `receive` on a clone holds while it waits, so it waited as long as that
    did. It now only tries the lock: with a receive outstanding, that receive takes each message
    as it arrives, so nothing is waiting and it returns nothing. It is no longer `async`. The
    test holds a pending receive on one clone and snapshots from another; before the fix it
    waited out the 20-second step.
