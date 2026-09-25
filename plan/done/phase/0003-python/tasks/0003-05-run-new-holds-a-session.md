# 0003-05 -- `outrig run-new` holds a session, and `run-legacy` names the old one

## Context

The first four tasks build a pipeline nobody can use. This one makes it a command.

`crate-split-tradeoffs.md` settles the naming: `run` keeps its current meaning and its current
code, `run-new` is added beside it, and `run-legacy` is added at the same time as an alias for
`run`. Adding the alias early is what makes the eventual retarget cheap -- by then `run-legacy`
has been the stable name for the old behavior for a while, and the switch strands nobody who had
been typing `run` and meaning the old thing. Retargeting `run` is a later milestone.

Interactive from the start, deliberately. The milestone's wording is "holds an interactive session
end to end", and a one-shot front end would be built only to be thrown away.

One staging note: this command passes the user's typed line to the model as an ordinary prompt.
The `user` channel that replaces that arrangement is `0003-08`. The phase's deliverable is the
channel; gating the first runnable CLI on it buys no verification the prompt does not already
give.

## Goal

A person can type at `outrig run-new`, watch an agent write and run Python against the workspace,
and get an answer -- with names the agent bound surviving into the next thing they type.

## Deliverables

- `run-new` as a subcommand: one enum variant, one match arm, and a reuse of the existing
  `repo_cmd_ctx` preamble. Global flags are inherited.
- `run-legacy` as an alias for `run`, reaching the same code.
- Session setup that starts the interpreter alongside the container the session already builds,
  and tears it down with the session.
- The REPL loop, reusing `repl.rs` rather than writing a second one. Terminal interaction stays in
  `outrig-cli`; `harness-components.md` is explicit that `run-new` drives the same loop the
  existing command does.
- A startup line that says what a person needs to know: the model, and that Python is ready with
  its version.
- **State across rounds, demonstrated rather than assumed.** Binding a name in one round and using
  it in the next is the phase's whole premise and the thing a reader will try first.
- **A stated policy on which integrations start.** `README.md` says the sandbox holds no
  credentials until isolation lands, and in the same breath says MCP servers still run and the
  model is simply not handed their tools. Those are in tension: a primary-placed server started
  with a resolved token is a same-UID process whose environment arbitrary Python can read where
  `/proc` policy permits. Not exposing a tool is not the same as putting the credential out of
  reach. Either do not start credential-bearing primary integrations for `run-new`, or narrow the
  claim to "OutRig does not inject credentials into the agent interpreter" and document the
  same-container exposure. Operator-supplied images, files, and mounts stay outside any blanket
  guarantee either way.

## Acceptance

- An end-to-end run against a real model: typed message in, Python executed against
  `/workspace`, answer out. This is the milestone's own criterion and it is checked by running
  it, with the transcript recorded in the task's `## Decisions`.
- **A name bound in one round is still bound in the next.** The test that distinguishes this
  phase from the legacy loop.
- `run` behaves exactly as before -- the existing `run` tests pass unchanged, and `run-legacy`
  reaches the same path. A test that invokes both and compares behavior, not an eyeball.
- `outrig run-new --help` describes the command without implying `run` has changed.
- **A dummy secret in a primary MCP configuration exercises whichever policy was chosen**, and
  the assertion is on the startup behavior rather than on `os.environ` inside Python -- reading
  the interpreter's own environment proves only that injection was avoided, not that the
  credential is unreachable.
- Ctrl-C at the prompt returns to the prompt rather than ending the session, matching `run`.
  Whether it reaches the interpreter is `0003-06`; here it must at least not make things worse.
- `cargo test --workspace`, `cargo clippy --all-targets`, `cargo fmt --check` pass.

## Design forks

1. **Whether `run-new` gets its own session-dir layout -- Recommended: no.** It is a session like
   any other. `logs/events.jsonl` arrives in `0003-13`.
2. **What the model is told about its one tool -- decide and record.** The preamble is the model's
   only orientation until `0003-10`, and `discovery.md` already argues that what an agent cannot
   learn by looking belongs there.

## Dependencies

- **Hard: 0003-04.** There is no round to drive until the loop and the tool exist.

## See also

- `plan/phase/0003-python/crate-split-tradeoffs.md` -- "Add `run-new`, and rename later".
- `crates/outrig-cli/src/cli/run.rs` and `repl.rs` -- the existing command and the line editor
  this reuses.
- `crates/outrig-cli/src/cli/session_setup.rs` -- where a session's container and logs are built.

## Decisions

- **Which integrations start: none (the maintainer's call).**
  - `run-new` launches no MCP server and no sidecar. It strips the chosen image-config's `mcp`
    table and `[sidecars]` from a copy of the config before `LaunchSpec::from_config` lowers it,
    and sets `EmbeddedMcpPolicy::Ignore` so servers in the image's `org.outrig.mcp` label stay
    out too. Startup names the configured servers it left out.
  - The copy is stripped *before* lowering: `from_config` builds or pulls every sidecar image it
    plans, for containers that would never start.
  - "Credential-bearing" could not be detected. A literal `TOKEN = "abc"` is as much a secret as
    `${TOKEN}`. Placement was the structural line, and starting nothing is simpler than drawing it:
    the model could not call a sidecar-hosted server either.
  - The claim is now "OutRig starts nothing in the sandbox that holds a credential from its
    config". The phase README's "the servers still run" and `security.md`'s "the agent
    interpreter holds no credentials" were narrowed to match. Both carry the operator exception
    and the runtime's proxy variables (see the review below).
  - It also makes `from_config`'s sidecar limits, which 0003-04 noted, moot here: no manual
    sidecars, no `on-failure = "warn"`.

- **How the session names its container: `PythonAgent` reports it (the maintainer's call).**
  - `Outrig::launch` names its primary `outrig-<timestamp>-<hex>`, attaches no label, and exposes
    no name. `session.json`'s `container_name` is what `discard` and `clean` use to tell a live
    session from a finished one, so a wrong name lets `clean` remove a running session's
    directory.
  - `PythonAgent::container_name` is captured from `outrig.primary()` in `start`.
  - **The record is written after the interpreter is up.** `run` writes its record first because
    it chose the name. Here the name is not known until `launch` returns, and a record written
    earlier would be live with a name that is not yet true. The `logs/` directory is created first
    because the launch writes there. A bad `--session-dir` is still checked before anything
    starts.
  - A start that fails is recorded too, and finalized at once with exit 1. An ended record's
    name is never checked.
  - `session.rs` is untouched: no setter was needed.
  - The label-based stray sweep still cannot see a `run-new` container whose record is gone:
    `plan/next/run-new-container-has-no-session-label.md`.

- **Ctrl-C mid-round keeps what the round ran (the maintainer's call).**
  - Dropping `round`'s future loses nothing from `history`, because rig works on a clone. What was
    lost was the round itself, including Python whose effects stand. That is the gap 0003-04's
    review closed for a failed model call.
  - `run_round` holds its `&mut history` in a guard while rig runs. If the round is dropped there,
    the guard's `Drop` splices in what the hook journaled, through `RoundHook::keep_what_ran`,
    the same method the error path uses. Once the round returns, the
    guard is disarmed. So `history` is whole the moment the round is gone, not a round later,
    and nothing outside `round.rs` has to know.
  - The first cut kept the hook on `PythonAgent` and spliced at the start of the next round.
    `/simplify`'s altitude review moved it into the round itself. The caller's version left
    `history` short until the next prompt, and never repaired it if the session ended first.
    Every later reader of history (0003-11, 0003-13) would have had to know that.
  - A round dropped before any call started changes nothing, so resending is safe. An execution
    still running when the round was dropped keeps the slot and reaches the model as a late
    result; getting Ctrl-C to it is 0003-06. What the journal keeps, and why every call in an
    unfinished batch is answered, is under the review below.
  - The REPL keeps the agent behind a `tokio::sync::Mutex` borrowed across the round, never moved
    into the future. Moving it would drop the agent, and the interpreter with it, on Ctrl-C. That
    is `plan/next/repl-interrupt-history-loss.md`'s bug in `run`, one level worse.

- **What the model is told (fork 2): a short orientation ahead of the configured preamble.**
  - `agent/orientation.rs` covers what the tool description does not and `discovery.md`'s test
    admits:
    - `submit_python` is the only way to act;
    - the working directory is the workspace (the primary's `-w`, named from
      `container_workspace()`, and left out when there is none);
    - this is a static CPython with the standard library only, with no `pip install` and no
      compiled third-party modules, and the image's programs are reached through `subprocess`.
  - That names persist and output is bounded is already in `submit_python`'s description, which
    is also sent every round, so it is not repeated.
  - `[agents.<n>].preamble` follows after a blank line, and an agentless session gets the
    orientation alone. `run`'s agents are shared, so a preamble written for MCP tools reaches
    `run-new` unchanged. That is the operator's to adjust.

- **The public surface grows by five lines, all under `outrig::PythonAgent`:** `check`, `model`,
  `python_version`, `container_name`, and `on_submit`.
  - `check` is `start`'s resolution alone. `run-new` calls it before pulling or starting anything,
    so a bare directory fails on "no model selected" as `run` does. The alternative, the CLI's own
    `llm::resolve_agent_with_overrides`, is the legacy loop's copy and resolves an alias
    differently.
  - `python_version` is what the interpreter's `ready` greeting says. The interpreter already
    sent it and the host discarded it; `Reply::Ready` now carries it, and a greeting without one
    is refused. A test ties it to the pinned payload's name.
  - `on_submit` exists because the goal is to *watch* the agent write Python, and a library does
    not print. The tool calls it once the interpreter has accepted a submission, from the source
    it already parsed. `PythonAgent` and the tool share the slot, because the tool is handed to rig
    before any observer exists. A call the cap refuses never reaches the tool, so it is not shown.
    `run-new` prints each source indented under `[outrig] python:` on stderr.
    - The first cut fired it from the round's hook, which parsed the arguments a second time and
      would show a submission the interpreter then refused.

- **The CLI side is one new file plus the wiring the task named.** Those are `cli/run_new.rs`, the
  `Cmd::RunNew` variant and its arm, `visible_alias = "run-legacy"` on `Cmd::Run`, and one
  transparent `CliError::PythonAgent` variant for the library's boxed errors. The variant has no
  `#[from]`, so a boxed error from anywhere else cannot turn into it by `?`.
  - Nothing in `run.rs`, `session_setup.rs`, `llm*`, `rig_tool.rs`, or `subagent/` changed.
  - `run-new` reuses `ProgressSpan`, the session store (`symlink_path` names the directory it
    creates `logs/` in), and `Repl`.
  - **The built-in default arrives without its servers.** `builtin_image::inject_primary` is new
    beside `inject`, and they share the veto on reserved names. It adds `[images.outrig-default]`
    with its `mcp` table empty, and no sidecars.
    - Plain `inject` would have written the shell sidecar's Dockerfile to the cache. On a host
      without the launcher, it would also have told the user to install a musl target so that a
      `shell` server would work, one that `run-new` never starts.
    - Startup would then have listed `fs` and `shell` as "not started", which the user never
      configured.
  - It mirrors `setup`'s image-config cascade: `--image`, then the agent's image, then
    `default-image`, then the built-in default. It takes only named image-configs, because
    `from_config` requires one.
  - It ensures the image itself, as `run` does: `compute_tag_for` and `ensure_tagged_image_for`
    under the image-config's name. The copy of the config it lowers has that image-config replaced
    by one naming the tag, keeping only its `security`.
    - This way `launch` gets an image, never a Dockerfile, and has nothing to pull or build. The
      tag the session records is the image that runs, and a Dockerfile image-config shares
      `run`'s and `build`'s cache.
    - The first cut called the nameless `image::ensure_image` and let `launch` build. `/simplify`
      found that `launch` keys its build over a synthetic config with no `mcp` table. So an
      image-config declaring servers was built twice, and the recorded tag was not the image
      that ran.
    - What is left, for library consumers, is
      `plan/next/launch-image-handling-differs-from-the-cli.md`.

- **A failed round returns to the prompt.** `run` ends the session on a round error. Here
  `PythonAgent`'s own error text says to send a prompt, which needs a prompt to type it at, and
  there is no retry yet to make errors rare.

- **No slash commands beyond `/help` and `/quit`.** `/reset` would clear the conversation and
  leave the interpreter holding names the model has no record of. `/tools` would list one entry.
  Both are left for `plan/next/run-new-flag-parity.md`.

- **`run-legacy` is a visible alias**, so `outrig --help` shows it beside `run`, where someone
  choosing to stay on the existing system will look.

- **Flags.** `run-new` takes `--agent`, `--model`, `--image`, and `--session-dir`. The rest of
  `run`'s, and why each needs more than wiring, are in `plan/next/run-new-flag-parity.md`.

- **What `/simplify` found and this task left.** Each would edit a file the 0.2.x line also edits,
  or change library behavior every caller sees.
  - **Test helpers.** `stub_runtime_path`, `podman_names`, and the Anthropic envelope helpers now
    have a copy in the new test files. Sharing them through `tests/common` means editing
    `builtin_default.rs`, `anthropic_mock.rs`, and the e2e files that hold the originals.
  - **A third copy of the image-config choice.** It is in `run_new.rs`, beside
    `session_setup.rs` and `cli/build.rs`, with two messages copied word for word. One resolver
    in `builtin_image` could serve all three once `session_setup` can change;
    `plan/next/builtin-image-nameable-as-default.md` changes this rule.
  - **`SessionStore` split into reserve and record.** That would retire `prepare_log_dir`'s own
    copy of the layout and the two `--session-dir` checks, but `session.rs` is the store `run`
    uses.
  - **`Repl` bound on `AsyncFnMut`.** That would let a callback borrow the agent across the await
    without the `Mutex`, and is the same root as `plan/next/repl-interrupt-history-loss.md`,
    which now says so.
  - **`from_config` lowering the primary as it lowers sidecars.** That is, ensuring it under its
    name, which would retire `run-new`'s own ensure and its pinned copy of the image-config. The
    copy keeps only `security`, a caller's list over a `#[non_exhaustive]` type.
    `plan/next/launch-image-handling-differs-from-the-cli.md` now says so.

- **The review rejected the first cut on four findings.** Each was reproduced against the code
  before it was fixed, and each fix is mutation-checked.
  - **A batch dropped mid-way lost the calls that had returned.** rig runs a turn's calls one at a
    time (its default `tool_concurrency`, 1) and refreshes nothing between them. What the round
    kept was only what the latest model call was sent. So a Ctrl-C during call B of a turn lost
    call A's source and result, while A's effects stood. A's outcome had already been handed to
    the round, so no late result would bring it back.
    - The hook now keeps a journal per turn: the request, the reply (`ModelTurnFinished`), how
      many calls have started, and each result as it comes back.
    - A round that ends without rig's history (a dropped future, or a failed model call) keeps the
      request and, if the reply's calls were running, the reply and one result for every call in
      it. A call that returned gets its result, the one in flight a note that it had not returned
      and may still be running, and any after it a note that it never started. A provider
      refuses a tool call without its result, which is why no call is left out.
    - "Ran" now means a call started, not that a second model call happened. So a round dropped
      inside its first call is kept too, where before it vanished. A round that started no call
      still leaves the conversation alone.
    - The n-th result answers the n-th call because the calls run in order. `ToolResult` does not
      carry the provider's id for Anthropic (it carries `call_id`, which is `None` there), so
      order is the only link, and the journal says what it relies on.
    - The test runs three calls in one turn and drops the round once the second's source has gone
      to run. The next request carries A's result, B's note, and C's. Keeping only `sent` fails
      it.
  - **Two `run-new --session-dir D` could each end the other.** The record is written after
    start, and in audit or filter mode the first holds `D/logs/network.jsonl`'s lock. So a second
    invocation passed every check, failed on that lock, wrote its failed record into `D`, and the
    first then failed on finding it.
    - `D` is now reserved by an exclusive, non-blocking `flock` on the directory itself, taken
      before `logs/` or anything else and held until `run-new` returns. `session.json` is checked
      under it.
    - A second invocation fails at once and writes nothing there. The kernel drops the lock with
      the process, so a killed `run-new` leaves nothing stale, and locking the directory rather
      than a file leaves no lock file behind.
    - `nix` joins `outrig-cli`'s dependencies for it, with `fs`. It was already in the build
      through `outrig`, and `std`'s `File::try_lock` is newer than the workspace's 1.88.
    - Legacy `run` takes no such lock. It writes its record before launching, so its window is the
      one it always had, and a `run` landing in a directory `run-new` holds fails `run-new` alone.
    - Tested twice: two reservations in one process, and the binary refused a directory the test
      holds, before any image work and without writing a record or `logs/`. A shared lock in place
      of the exclusive one fails the first.
  - **The observer printed source the interpreter refused.** `Interpreter::submit` returns an
    `Execution` whose outcome is already `Refused` when the slot is taken, which it is after a
    Ctrl-C leaves Python running. `Execution::queued` now says whether the source went to run,
    and the tool calls the observer only then. The refusal still reaches the model. Tested with a
    dropped round's `sleep` holding the slot and the next submission refused.
  - **The credential guarantee missed the runtime's proxy variables.** podman forwards the host's
    `HTTP_PROXY` and friends into every container by default, and a proxy URL can carry a
    password; exported with one, `podman run alpine env` printed it back. The guarantee is
    narrowed to credentials from OutRig's own config, and `doc/reference/cli.md`, the phase
    README, and `security.md` name the exception. Turning proxy inheritance off would cut an agent
    behind a proxy off the network, and `run` has the same exposure, so the choice is
    `plan/next/proxy-credentials-reach-the-primary.md`'s.

- **Tests.**
  - `tests/run_legacy.rs`, not gated, runs the binary against a failing `podman`/`buildah`.
    - `run` and `run-legacy` give the same exit, stderr, and session record, with the session id
      and elapsed times normalized. It covers a bare directory, a repo that reaches its first
      pull, and a missing `--session-dir` alongside a `run`-only flag.
    - Their `--help` is byte-identical.
    - `run-new --help` says what it is and that `run` is unchanged. `run --help` and `run`'s
      summary are as they were.
    - `run-new` fails on the model before any session exists, and records a failed start as
      ended.
  - `tests/run_new_e2e.rs` (`e2e`) drives the binary against podman, alpine, and a scripted
    Anthropic endpoint:
    - round 1 binds `x` and prints `os.getcwd()`, which reads `/workspace`;
    - a `kill -INT` arrives at the prompt;
    - round 2 prints `x + 1`, which reads `42`;
    - the primary server `leaky`, with `env = { TOKEN = "${UNSET}" }`, is named as not started;
    - the record carries the real container name and exit 0, and the container is gone.
    - Mutation-checked: letting the image's `mcp` table through fails the launch on the unset
      secret. That is the evidence the policy is asserted on startup rather than on `os.environ`.
  - `cli/run_new.rs`'s unit tests check that the lowered `LaunchSpec` has no server, no sidecar,
    and `Ignore`, and that the session's own config is left whole. They also cover the banner,
    the submission rendering, and the `--session-dir` checks.
  - `agent_tests` cover:
    - a round dropped after it ran Python, whose work is in `history` as it is dropped and
      reaches the next round's request (mutation-checked by disarming the guard's `Drop`);
    - the observer, and the cap withholding a call from it;
    - the orientation, then the preamble, on the wire, with an agentless session oriented too;
    - `check` agreeing with `start`;
    - `model` and `python_version`;
    - `container_name` in the in-crate e2e.
  - The e2e tests ran outside the sandbox, which mounts `/run/user/1000` read-only.

- **End to end against a real model.** The run used `opus-5` through the maintainer's global
  config, the alpine image, and a scratch repo holding a four-line `hello.txt`. It ran on the
  final build, after `/simplify`. The two typed lines were "How many lines does hello.txt in the
  workspace have? Store the count in a variable named n." and "Without reading any file again,
  what is n times 2? Use the variable you stored." rig's `INFO` lines are elided from stderr
  (`plan/next/rig-info-logs-reach-the-terminal.md`), and so are the `> ` prompts they shared a
  line with.

  ```text
  $ outrig --session-root <tmp>/sessions run-new < input.txt
  [outrig] loading config
  [outrig] config loaded (2ms)
  [outrig] ensuring image for primary
  [outrig] image ready: docker.io/library/alpine:latest (cache hit) (32ms)
  [outrig] starting container
  [outrig] container ready (286ms)
  [outrig] starting python
  [outrig] python ready (228ms)
  [outrig] image-config:  primary
  [outrig] image:         docker.io/library/alpine:latest
  [outrig] model:         opus-5
  [outrig] python 3.13.15 ready in outrig-20260925T175508-b9aa
  [outrig] session id: 20260925T175508-7b5b   (Ctrl-D to exit, /help for slash commands)
  [outrig] python:
      from pathlib import Path
      p = Path('/workspace/hello.txt')
      print(p.exists())
      print(repr(p.read_text()) if p.exists() else sorted(x.name for x in Path('/workspace').iterdir()))
  [outrig] python:
      text = p.read_text()
      n = len(text.splitlines())
      print(n)
  [outrig] python:
      print(n * 2)
  ```

  stdout, the two replies:

  ```text
  **hello.txt has 4 lines**, and the count is stored in the variable `n` (n = 4).

  The file contains: `alpha`, `beta`, `gamma`, `delta` — each terminated by a newline, so there's no partial trailing line to worry about.
  n × 2 = **8** (using the stored `n = 4`, no file access needed).
  ```

  - The second round's only submission was `print(n * 2)`: a name bound in round 1, and `p`
    from its first call, used without re-reading.
  - `session.json` recorded `container_name: outrig-20260925T175508-b9aa` and `exit_code: 0`.
  - The session id and the container's suffix differ, because the facade names its own container.
  - An earlier run on the first cut, before `/simplify`, went the same way: its round 2 was also
    `print(n * 2)`.
