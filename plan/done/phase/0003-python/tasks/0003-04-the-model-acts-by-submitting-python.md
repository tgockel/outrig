# 0003-04 -- The model's only tool submits Python, and `outrig` drives a round

## Context

This is the task where the phase's thesis becomes code: the model is handed one tool, and it
submits Python. It is also the task that starts the crate split.

`crate-split-tradeoffs.md` chose duplication over a move, because every file a move would touch is
one the 0.2.x line is still editing. What it did not settle is *how much* to copy at once. This
task copies the minimum a single round needs -- model resolution, agent construction, the round
call -- and leaves retry, failover, and the local-llm arm to `0003-15`. The exit criterion that
the loop lives in `outrig` is met from the first commit, and nothing has to be unwound later.

Two constraints from the same page bound the surface. rig stays a private dependency, so the
library's error type converts at the boundary rather than carrying `rig::completion::PromptError`.
And the new modules stay private except for one entry point, which exists because Rust requires it
rather than because anyone should build on it.

## Goal

`outrig` can resolve a model, build an agent whose only tool submits Python to the session
interpreter, and drive one round to completion -- reachable from `outrig-cli` through a single
public type. (Amended from "a single public function"; see `## Decisions`.)

## Deliverables

- A private `agent` module in `outrig`: model and provider resolution from the existing config
  types, agent construction, and the round call.
- The tool: one input, the source to run; one output, the execution's result. Bounded by the same
  `tool-result-max-bytes` the existing surface applies, since a traceback or a captured build log
  is exactly the oversized result that ceiling exists for.
- **An error type that names no rig type.** Conversion at the boundary, written once per variant.
- **The effective output ceiling readable outside agent construction.** It is resolved here, and
  `plan/next/clamped-ceiling-is-silent.md` records that it currently never escapes `build_agent`.
  `0003-13` records it as an event and cannot while it is trapped; exporting it at the point it is
  computed is cheaper than retrofitting it into a copy later.
- **One `pub` entry point**, and nothing else: whatever starting a session and driving a round
  requires, in terms that name no rig type. `crates/outrig/public-api.txt` regenerated, growing by
  that entry and nothing more.
- **Explicitly not copied here**: `llm/retry.rs`, `llm/failover.rs`, `llm/mistralrs.rs`,
  `llm/registry.rs`, `builtin_tool.rs`, `self_tool.rs`, `subagent/`. The first two arrive in
  `0003-15`; the in-process backend is deprecated and never arrives; the rest are unqueued.
- `outrig-cli`'s existing harness is untouched. Not "mostly untouched" -- a merge from the 0.2.x
  line must apply without conflict, which is the entire argument for copying.

## Acceptance

- A round driven against a mock provider: the model emits a tool call carrying source, the source
  runs in the interpreter, and the result reaches the model.
  `crates/outrig-cli/tests/anthropic_mock.rs` is the shape -- the mock harness that exists on this
  line. The prototype's
  `openai_mock.rs` is on a branch that never merges, so a second provider fixture is new work and
  should be named as such rather than cited as precedent.
- **The tool's result is bounded**, and a result past the ceiling arrives truncated with the
  marker rather than whole.
- `crates/outrig/public-api.txt` regenerated and clean, and every line the diff adds is under
  `outrig::PythonAgent`. No line of the snapshot names rig, and
  `crates/outrig/tests/public_api_boundary.rs` holds that. The `public-api` CI job is the gate.
  (Amended from "exactly one added entry"; see `## Decisions`.)
- `git diff` shows no change under `crates/outrig-cli/src/llm*`, `rig_tool.rs`, or `subagent/`.
- `cargo test --workspace`, `cargo clippy --all-targets`, `cargo fmt --check` pass.

## Design forks

1. **How much of the loop the minimum actually is -- Resolved; see `## Decisions`.** Model
   resolution drags in alias handling and per-model ceilings. If a piece turns out to be
   load-bearing for one round, copy it and say so here; if it is only load-bearing for retry, it
   belongs in `0003-15`.
2. **Whether the entry point takes a built session or builds one -- Takes a launched `Outrig`;
   see `## Decisions`.** The narrower it is, the less it commits to. It will move, and the task
   should record what it chose and why.

## Dependencies

- **Hard: 0003-03.** The tool has nothing to submit to until the host can drive the interpreter.

## See also

- `plan/phase/0003-python/harness-components.md` -- what moves, what is copied, what stays.
- `plan/phase/0003-python/crate-split-tradeoffs.md` -- why duplication, why rig stays private, why
  the surface is one entry.
- `crates/outrig-cli/src/llm.rs` -- the loop being copied from, and `rig_tool.rs` for how a
  discovered tool becomes something an agent can call.

## Decisions

- **The surface is one type, not one function.** This was the maintainer's call, and it amends the
  Goal and the public-api acceptance bullet.
  - `PythonAgent` has two methods. `start` resolves, starts the interpreter, and builds the agent;
    `round` drives one prompt. The snapshot grows by four lines: the struct, its `impl`, and the
    two functions.
  - The literal one-line rule would have forced a single `pub async fn` that owns the whole loop
    and trades prompts and replies over channels. The CLI's `Repl::run` wants a per-prompt
    callback, which `round` is.
  - The line count stood in for "the copied shape does not leak". Two checks hold that directly:
    the snapshot diff adds only `outrig::PythonAgent` lines, and
    `crates/outrig/tests/public_api_boundary.rs` allows only the crates in `PUBLIC_CRATES` to
    appear in the snapshot. The allowlist holds rig and reqwest private, and any later private
    dependency with them. The review of the first cut preferred it to a rig-only matcher.

- **It takes a launched `Outrig` (fork 2).**
  - Only `Outrig::launch` mounts the payload. The CLI's `session_setup` builds its containers
    without it, and it cannot add the mount without new public surface.
  - So `run-new` in `0003-05` launches through the facade. That brings `LaunchSpec::from_config`'s
    documented limits: no `--network` override, `start = "manual"` sidecars skipped, and
    `on-failure = "warn"` not honored.
  - `Outrig::primary()` is a new `pub(crate)` accessor. `Interpreter::start` still takes a
    `&Container`, because the host's e2e tests start it in a bare container.

- **The error is boxed at the boundary.**
  - That means no new public type and no variant on the frozen `OutrigError`.
  - The crate-private `AgentError` has three variants: `Resolve`, `Outrig`, and `Prompt(String)`.
  - rig's `PromptError` is rendered to text as it is caught. It holds rig's types and, on some
    paths, the whole conversation.
  - Nothing in `0003-04` or `0003-05` branches on the kind. A typed error has something to be
    designed against once `0003-15` separates "ends the round" from "ends the session".

- **What one round needs (fork 1).**
  - **Copied from `outrig-cli`'s loop:**
    - Resolution, including aliases. An alias resolves to the first model this build can reach,
      which is what the CLI did before failover (`first_selectable`), and a `tracing::warn!` says
      the rest go unused.
    - Anthropic's three-tier ceiling and its clamp. A round cannot run without them: Anthropic
      rejects a request with no ceiling.
    - The round call, with the tool-call cap and the partial-history splice for a stopped round.
    - `truncate_for_llm`.
  - **Not copied, and where each piece goes:**
    - Retry and failover, including resolving an alias's whole chain: `0003-15`.
    - Salvaging a reasoning-only reply: `plan/next/python-agent-textless-round.md`.
    - The subagent hook machinery (labels, steering, the repeat breaker), the subagent limits, the
      image hint, and the banner's display names: nothing here reads them.
    - The missing-`max_tokens` error rewrite: unreachable here. The Anthropic arm always sends a
      ceiling, and the OpenAI API needs none.
    - `session_tool.rs` and the MCP adapter: nothing on this side calls them.
      `harness-components.md`'s "Copied" paragraph is corrected to say so.
  - **Changed by landing in the defining crate:**
    - There are no fallback arms for an unknown provider style. `LlmProvider` is
      `#[non_exhaustive]` only outside this crate, so here a new style fails to compile.
    - Building is synchronous. That settles `plan/next/build-agent-is-async-for-nothing.md` for
      this copy only.
    - A library does not print. The fallback-ceiling warning goes through `tracing::warn!` and
      tool-call traces through `debug`. Presentation is `0003-05`'s.

- **The effective ceiling is kept where it is computed.**
  - `build_agent` returns it with the agent, and it is stored on `PythonAgent`. An
    `expect(dead_code)` names `0003-13` as its reader.
  - The tests compare it with the `max_tokens` on the wire, both for a clamped value and for the
    fallback.
  - `plan/next/clamped-ceiling-is-silent.md`'s other half, a warning when the clamp fires, stays
    open.

- **The tool is `submit_python`,** the name `execution-and-rounds.md` uses. It takes one argument,
  `source`, with `deny_unknown_fields` to match the schema's `additionalProperties: false`.
  - **How each outcome reads:**
    - All four outcomes return text, not a tool error: a raise is a result the model reads.
    - `Unknown` never reads as worth retrying: `Exited` says to check for the effects rather than
      run it again, and `Unresolved` says do not run it again.
    - `Refused` names the execution that holds the slot.
    - An empty result reads `(no output)`, because an empty text block is not something to send
      a provider.
  - **When it is a tool error:** only when nothing could be submitted (`InterpreterError`) or the
    arguments are malformed.
  - **Late results (0003-03's open question):** they are taken after the outcome, so one that
    arrives during the call is not held back. Each gets a status line naming its execution and
    how it ended. Its output follows the current call's, which is headed `[this call]`.
  - **The bound is `tool-result-max`,** the same ceiling the MCP surface applies (the task text
    says `tool-result-max-bytes`, but the config key is `tool-result-max`). Its default of
    256 KiB is above anything the interpreter's own bounds produce: 16 KiB of output, 2 KiB of
    background, and a capped traceback. So in practice the ceiling fires only when it is
    configured lower. The acceptance test sets it to 1024.
  - **The truncation marker's advice is reworded** from narrowing a query to printing less.

- **rig 0.40 reinterprets tool output that parses as JSON.** An object carrying `response` or
  `parts` reaches the model as that field alone (`ToolResultContent::from_tool_output`).
  `print(json.dumps(reply))` of an API reply would silently lose every other key.
  - The round's hook rewrites every `submit_python` result, and rig delivers a rewritten result
    verbatim.
  - The first cut rewrote only results that parsed as objects. The review moved it to all
    results, so that no rule of rig's has to be predicted.
  - rig 0.42's `ToolOutput::text` says the same thing at the tool, and would retire the arm. The
    workspace pin is shared with `outrig-cli`, so upgrading is not this task's to do.

- **A stop stays a value until the public boundary.** The round returns a crate-private `RoundEnd`
  with `reply` and `stopped`, and `PythonAgent::round` renders `(round ended: <reason>)`. So
  `0003-13` can record a stop without parsing text.

- **Inherited, not fixed:** a model call that names an unknown tool fails the whole round, as the
  CLI's loop does.

- **After review, two fixes.** The PR's review rejected the first cut on two findings. Each was
  reproduced as a failing test, fixed, and mutation-checked.
  - **A round that failed on a later model call lost the Python it had run.** This was recorded
    above as inherited and left for `0003-15`. The review held that it cannot wait: with no retry,
    any transient error after a `submit_python` drops the record of work whose effects remain,
    and a resent prompt then invites running it again.
    - A failed model call is `CompletionError`, which carries no history. Instead, the round's
      hook keeps what each model call after the first was sent: rig's own `history` and `prompt`
      for that call. That is every completed tool call and result, in an order the provider
      already accepted, so it is not a second, reconstructed copy of the conversation.
    - On such an error it is spliced in, and the error says to continue rather than resend.
    - A failure on the first model call ran nothing, so it still leaves the conversation alone,
      and resending is safe.
    - The review's other option, refusing further rounds after such a failure, was not taken: it
      would end a session over any transient error.
    - This fixes the library's copy only. `plan/next/partial-turn-history-on-failed-model-call.md`
      now says the library's loop is fixed and the CLI's is not.
  - **Truncating a result could erase that the code raised.** Every outcome is text, and the
    traceback came last. So at `tool-result-max = 1024`, a large print followed by a raise
    reached the model as output plus the marker, with no sign of failure. A late result's output
    could push the current outcome out the same way.
    - A rendering now has two parts. The **status** says how each execution ended: `[this code
      raised <the traceback's last line>]`, the refusal, the unknown outcome's no-retry guidance,
      and one line per late result. It comes first, and room is reserved for it whole. The
      **detail** is the output, tracebacks, and the unknown's exit cause, and it is cut to fit
      what is left.
    - Status first would survive most cuts on its own. The reservation matters once statuses
      pass what a head-only cut keeps before its 370-byte marker, which at 1024 bytes is about
      seven late results. The test covers that case.

- **A second review found the status block could itself overflow.** Late results with long
  exceptions -- three with 400-character messages, at 1024 bytes -- made the statuses alone
  exceed the ceiling. The fallback then cut them like ordinary output: a line split mid-way,
  and later results were dropped after `take_late` had already handed them out, so they were
  never reported.
  - Late statuses are now added whole and in order while they fit, with room kept for one
    line counting the rest.
  - The tool holds the records that did not fit, and they lead its next result, so each is
    reported exactly once. At config's 1024-byte floor there is room for the current status
    and at least one late result, so the backlog always drains.
  - The through-the-tool test seeds held-back records with a `cfg(test)` constructor. A first
    attempt made real late records by dropping executions and ordering on an inventory
    round-trip. That does not order anything: the result is sent once the drain sees the
    sentinel, and it can follow the inventory's reply, so the next submission was refused.

- **CI's musl check needed a C toolchain; the maintainer chose `musl-tools`.**
  - rig's and reqwest's `rustls` pull `aws-lc-sys` into the library. Its build script compiles C
    for the target even under `cargo check`, so `cargo check -p outrig --target
    x86_64-unknown-linux-musl` failed looking for `x86_64-linux-musl-gcc`.
  - The step now installs `musl-tools` and sets `CC_x86_64_unknown_linux_musl=musl-gcc`.
  - This machine has no `musl-gcc`, so CI's first run is the step's first run.
  - The alternatives were rejected. reqwest 0.13's `rustls` always selects aws-lc-rs, and the
    `ring` provider compiles C too.
  - The CHANGELOG notes the new requirement.

- **`host.rs`'s dead-code expectation is per item.**
  - The module-wide `expect` would fail under a plain `cargo test` now that `start` has a
    caller.
  - Each of `Unknown::Unresolved`, `Interpreter::inventory`, and `Execution::stop_waiting`
    carries its own `cfg_attr(not(test), expect(dead_code))`, which names `0003-06`.
  - The test helpers that started the payload interpreter on the host moved into
    `python::testing`, and both test modules use them.

- **Tests.**
  - `agent/agent_tests.rs` runs against a scripted Anthropic endpoint and the payload run on the
    host. The mock is `agent/mock_http.rs`, copied from `outrig-cli`'s `tests/common` as the
    first such mock in this crate.
  - What they cover:
    - the round trip, with exactly one advertised tool;
    - a name surviving into a second round;
    - truncation on the wire;
    - JSON output arriving whole;
    - a raise arriving as its traceback;
    - the cap ending a round and the next one continuing;
    - malformed arguments;
    - rendering of every outcome and of late records;
    - the ceiling;
    - resolution.
  - **The e2e test is in-crate**, as `host_tests`' is: the loopback `no_proxy` and the mock are
    both `cfg(test)`. It launches an alpine `Outrig`, drives `PythonAgent::start` and `round`, and
    checks that the tool result is the container's `/etc/alpine-release`. It was run locally
    against rootless podman, outside the sandbox, because the sandbox mounts the podman socket
    read-only.
  - **Mutation-checked:**
    - dropping the JSON guard;
    - dropping truncation;
    - reporting the configured ceiling instead of the one in force;
    - disabling the cap's stop;
    - an unlisted crate planted in the snapshot.
  - **Two unrelated failures:** `network::tests::the_accept_loop_*` fail inside this session's
    sandbox, on the base commit as well, and pass outside it.

- **For later tasks.**
  - `0003-05`:
    - Launch through `Outrig`, and handle `from_config`'s limits noted above.
    - The startup line needs the model's name and the Python version. Neither is on `PythonAgent`
      yet, and each is a method on it.
    - Tool calls are traced at `debug` only, so what the user sees of them is the CLI's to
      decide.
  - `0003-06`: callers for `stop_waiting`, `inventory`, and `Unresolved`.
  - `0003-13`: `PythonAgent::max_tokens` and `RoundEnd::stopped`.
  - `0003-15`:
    - retry and failover, including an alias's whole chain;
    - the typed error;
    - the CLI copy's partial-history loss, which this copy now fixes.

