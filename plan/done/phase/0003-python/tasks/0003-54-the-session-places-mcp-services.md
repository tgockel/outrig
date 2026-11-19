# 0003-54 -- The Session places MCP services

## Context

The harness starts no MCP server. `container_spec` (`crates/outrig/src/harness/builder.rs:808`)
clears the image's MCP entries and every sidecar, sets `EmbeddedMcpPolicy::Ignore`
(`crates/outrig/src/outrig_.rs:401`) and returns the names it left out; the default container goes
through it (`builder.rs:665`), and `run-new` prints "run-new starts no MCP servers"
(`crates/outrig-cli/src/cli/run_new.rs:715-719`). The reason is `security.md`'s: "OutRig starts
nothing that holds a credential from its config where the agent can reach it. That is also why
`run-new` starts no MCP server: one placed in the primary would hold its resolved secrets beside
the interpreter, as the same user." What exists below the harness works and is in use by the legacy
`run`, `outrig mcp` and the image build: `McpClient` over `podman exec` and `podman start --attach`
stdio (`crates/outrig/src/mcp.rs`, `connect_via_podman_start` at `:189`, `list_tools` at `:324`,
`shutdown` at `:479`), `Outrig::launch` starting a spec's `mcp` map and sidecars (`outrig_.rs:1045`,
the connect at `:1178`), `Outrig::add_sidecar` (`:1237`), `SidecarSpec` with `SidecarView::Primary`
(`:303`; `crates/outrig/src/config/mod.rs:2615`), the `McpServerSpec` entry shapes
(`config/mod.rs:2870`) and `SidecarConfig` (`:2539`), `${VAR}` values resolved from the process
environment (`crates/outrig/src/config/env_value.rs:98`) and passed to podman by name
(`resolve_mcp_env`, `mcp.rs:558`), and the `Secrets` resolver a session's model keys go through
(`crates/outrig/src/config/api_key.rs:46`, `resolve_with` at `:97`), which `embedding.md` ("What
the embedder supplies") says exists so an embedder "changes no environment at all".

After `0003-44` the session has a service mechanism (`services.md`): a `Service` bound by name
through `SessionBuilder::serve`, a table that admits, checks, dispatches, bounds, cancels, events
and reports every call, a generated object per kernel, a presenter-kind registry and the `service`
facility kind. This task makes each placed MCP server a `Service` OutRig ships, `McpService`, bound
into that table, and adds what the mechanism cannot know of a server: placement, startup, secrets,
the writer control and the lifecycle.

A startup failure is `McpStartupFailure` (`crates/outrig/src/error.rs:431`) whose `source` is an
erased `Box<dyn Error>`, built by `enrich_startup_error` (`mcp.rs:592`) from rmcp's
`ClientInitializeError`, while a mid-session failure is an `McpSessionError` with an
`McpFailureKind` (`error.rs:395`, `:363`; `session_error_from_rmcp`, `mcp.rs:817`).
`plan/next/mcp-startup-failure-erases-its-cause.md` records the asymmetry; this task absorbs that
entry, under A-08: `OutrigError::McpStartupFailed` is an item the first application's rig engine
matches on, so its payload's field is not retyped (the entry's option 1); the classification is
added beside it on the `#[non_exhaustive]` struct and the field's doc says what it holds (the
entry's option 2). `plan/next/the-rmcp-upgrade-review-cannot-fail.md` asks for a test that pins the
reviewed rmcp version, since `session_error_from_rmcp`'s wildcard files a new variant under `Other`
silently; this task adds a second classifier, so it adds that test.

The requirement is OR-MCP1 and OR-MCP4 of `02-channels-and-mcp.md`: given declarations, resolvers
and optional policy, OutRig owns validation, placement, transport, startup, discovery, reconnect and
teardown; HTTP URL, stdio command, sidecar and primary-view placements, with two picks of one
declaration coexisting under different names; startup returning per-service outcomes that separate
misconfiguration and refusal from unavailability after a budget, retrying transient failures with
redacted causes; credentials resolved on the trusted side and kept out of every view; opaque
identity carried unchanged; revocation closing admission with no reconnect under a newer claim;
managed writers participating in the cut. OR-A3-5 asks that the MCP surface share attribution,
events and secret rules with a Rust service's, which binding it as a service gives. The validation
found this lands on reserved ground (`mcp-wrappers.md`, "Not in this phase") with one decided point
to carry: a server that holds a credential cannot be primary-view, which `security.md` already
implies and OR-MCP4 asks to "define and validate" (conflict check C7, tension 8). It also fixed the
wording: revocation is the one-way `close_admission()`; the pausable gate the suspend requirements
call "admission" is `continuation.md`'s pause. The design is
`plan/phase/0003-python/mcp-presenter.md` ("The shape", "Declarations", "Startup",
"Admission, the close and the report", "Secrets, placement and what a service grants", "The
pause-or-stop control", "Events"). This task records in its `## Decisions` the end of the waiting
`mcp-wrappers.md` describes, for placement and startup, quoting the sentences the page quotes. The
URL placement is `0003-57`'s; the presenter `0003-55`'s; the dispatch states `0003-56`'s.

## Goal

A Session places, starts, discovers and tears down the MCP services an owner declares -- command,
sidecar and primary-view placements, each with secrets resolved on the trusted side -- binds each
ready one as a service, returns a classified outcome per service, starts with the ones that are
ready, and offers the cut a control that quiesces a service or says why it cannot.

## Deliverables

- **Declarations as data.** `McpServiceSpec`: a bound `name` under `bind`'s rule, a placement --
  `Command { command, env }`, `Sidecar(SidecarSpec)` hosting one server, `PrimaryView { image,
  args, env }` -- a `call_timeout_secs` and a one-line `description`, both optional; `env` values
  are `EnvValue`s (`env_value.rs:37`). The enum is `#[non_exhaustive]`, so `0003-57` adds `Url`
  without a break. `McpPolicy` with OutRig's defaults: `connect_budget` (fork 4) and
  `reconnect_max` (read by `0003-56`). The call deadline, the result bound and `record_bodies`
  are the service policy's (`0003-44`); a declaration's `call_timeout_secs` becomes its
  description's `call_deadline`.
- **On the builder and on `outrig.Session`.** `SessionBuilder::mcp(name, spec)`, `mcp_policy`,
  `mcp_identity(headers)`, beside `bind` (`builder.rs:225`), `serve` (`0003-44`) and `secrets`
  (`:187`); no approval, as for a binding given through the API.
  `outrig.Session(mcp=[...], mcp_policy=, mcp_identity=)`
  (`crates/outrig/src/python/outrig/_host.py:703`) takes the same as Python data, which the program
  sends in `session.start` as JSON (`crates/outrig/src/owner/mod.rs`, the owner protocol) and the
  engine answers with the outcomes; the program holds no client. Configuration entries become
  declarations too (fork 2): `[images.<name>.mcp]` with the `[sidecars.<sc>]` blocks they name, and
  the image's `org.outrig.mcp` label under `env-allow` as today, a server name that is not an
  identifier bound as fork 3 says, and a repository's `url` entry under the approval gate `0003-57`
  adds; `container_spec` keeps clearing the launch spec, and this task's MCP table starts them.
- **Validation before anything starts.** A name given twice, or one a binding or service uses,
  is a start error, as a binding's is (`crates/outrig/src/harness/bindings.rs:32` and `gather`). A
  declaration is `misconfigured` before it starts, and the session starts without it, when: a
  `${VAR}` appears in the `env` of a `Command`, a `PrimaryView`, or a `Sidecar` whose spec's `view`
  is `Primary` -- the effective view is normalized before any check, so such a sidecar is a
  primary-view placement for the credential rule and the writer predicate alike; a header names
  an identity header; an `env` key is one podman reads itself
  (`doc/reference/config.md`, "MCP `env` value syntax") and its value is a `${VAR}`, since such a
  value stays on the command line; or the `${VAR}` resolves to nothing.
- **Secrets through the session's resolver.** `ResolvedEnvValue::resolve_with(&dyn Secrets)`
  beside `resolve` (`env_value.rs:98`), used for every declaration's `env`; the value reaches
  podman as its own environment, by name, through the hidden-environment path `Cmd::env_hidden`
  provides, never on a command line; values are resolved once at start and held, and a reconnect
  (`0003-56`) reuses them. No event, outcome, error, `Debug` rendering or report holds a resolved
  value; a `${VAR}` is shown as the reference.
- **Starting.** Each declaration is placed and started concurrently with the others (fork 1): a
  `Command` as `connect_via_podman_exec` does, a `Sidecar` as `add_sidecar` does, a `PrimaryView`
  as an entrypoint sidecar with `SidecarView::Primary`; the handshake, then `tools/list` with the
  client's paging safeguards, as the first listing. One attempt is bounded by `STARTUP_TIMEOUT`;
  the policy's `connect_budget` bounds them together. Each failure is classified -- `transient`,
  `refused`, `misconfigured`, `unclassified`, as the page's table says, with a stdio server that
  exited before `initialize` classified by its exit status (fork 6) -- and a transient one retried
  with a backoff of the task's choosing within the budget, each wait an event with a redacted
  cause; the others stop at once. A sidecar's `on-failure` is not consulted.
- **`McpService`**, the `Service` implementation (`mcp-presenter.md`, "The shape"): `describe()`
  is the listing as a `ServiceDescription` -- one method per tool with the exact name, the
  description, `inputSchema` as `params`, `outputSchema` as `result`, `title`, `annotations`,
  `icons` and `_meta` in `extras`, `presenter: "mcp"`, the generation and `call_deadline` -- and
  `call` dispatches to the client's `call_tool`, returning the `McpToolResult` as JSON; `0003-56`
  gives it the dispatch states and the cancel notice. A ready service is bound into the service
  table under its name, which publishes `service.bound`; the `mcp` presenter kind is registered in
  `0003-44`'s registry with the class `0003-55` writes, the generic class standing in until then.
- **Outcomes.** `McpServiceOutcome`: `Ready { tools, listing, attempts }`, or `Unavailable`,
  `Refused`, `Misconfigured`, `Unclassified` -- `0003-57` adds `Unapproved` beside them -- each with
  a redacted cause -- the name, the placement kind, the command with references as references, the
  exit status, the stderr tail the client captures. `Session::mcp()` returns the MCP table's view:
  `outcomes()`, and the control below. A host program reads `session.mcp` after entry. `run-new`
  prints one line per outcome in place of "starts no MCP servers", and continues with what is ready.
- **One classification of a startup failure, beside the erased cause.** `McpStartupFailure` gains
  `kind: McpFailureKind` and `classified: McpSessionError`, additively; `source` stays as it is,
  with a doc saying it holds the SDK's handshake error and that downcasting it is not a supported
  surface; a classifier over `ClientInitializeError`'s variants beside `session_error_from_rmcp`,
  and a test naming every variant rmcp 3.4.1 declares, as `every_rmcp_service_error_is_classified`
  does. The `REVIEWED_RMCP` test `plan/next/the-rmcp-upgrade-review-cannot-fail.md` describes: the
  locked `rmcp` in `Cargo.lock` equals a constant beside the classifiers, with a message naming the
  two reviews and `SUPPORTED_PROTOCOL_VERSIONS` (`crates/outrig/src/mcp_proxy.rs:48`).
- **The MCP table.** One per session, beside the service table, holding each declaration, its
  client, placement and state (`ready`, `disconnected`, `paused`, `stopped`, or its failed
  outcome), behind a sealed `Placement` trait with `connect`, `freeze`, `thaw` and `stop`, so a
  test drives it with a fake placement (fork 7). It hears the close through `Lifecycle::on_close`
  (`crates/outrig/src/harness/lifecycle.rs:117`), after which nothing is started again. Calls are
  the service table's.
- **Identity headers.** `mcp_identity`'s pairs are held for the URL services the owner declared
  (`0003-57`) and for no URL service a configuration declared, never read, logged or previewed; a
  declaration's `headers` naming one is `misconfigured`; a `Command` or `Sidecar` placement receives
  identity only through its own `env`.
- **The pause-or-stop control**, on `Session::mcp()`: `writers()` -- each service with whether it is
  a writer and the paths it reaches, from its declaration alone as the page's rule says, a `Sidecar`
  with `view = Primary` a writer of the primary's filesystem whatever its `workspace` and `mounts`;
  `quiesce(service, Pause | Stop) -> QuiesceOutcome` -- `Quiesced { how }`, `InFlight { calls }`,
  `Uncontrolled { why }` -- where pause holds new dispatch for the service in the service table (a
  call made meanwhile waits, bounded by the cut's deadline, when the pause is a cut's; it is
  `refused`, reason paused, when the owner paused the service outside a cut), waits a bounded time
  for the service's in-flight calls, and freezes the container with `podman pause`; stop sends each
  in-flight call the token with reason `stopped`, closes the transport and stops the container; a
  `Command` placement is `Uncontrolled`; `resume(service)` thaws and reopens dispatch. The control
  never reports `Quiesced` before the freeze or stop returned. `0003-62` calls it.
- **The close.** At terminating each ready service's transport is closed (`McpClient::shutdown`,
  which the page notes says nothing about the server) and its container stopped with the session's
  (`Session::shutdown`, `crates/outrig/src/harness/session.rs:396`); a paused service is resumed
  first; a `Command` placement ends with the primary. `lifecycle.md`'s close table gains the
  **MCP services** rows the page lists, and `ShutdownReport`
  (`crates/outrig/src/harness/report.rs:19`) gains `mcp_services`: each service, its placement, how
  it was stopped and whether that was proven. The Python `ShutdownReport` mirrors it. A call's
  close is the service mechanism's.
- **Events**, in the integration-audit category, documented in `doc/reference/events.md`:
  `mcp.service.started`, `mcp.service.retry`, `mcp.service.failed`, `mcp.service.paused`,
  `mcp.service.resumed`, `mcp.service.stopped`, with the fields the page's "Events" lists; a
  service's events carry no subject, as `binding.exited` carries none
  (`crates/outrig/src/harness/event.rs:763`). A call's events are `service.call.*` (`0003-44`),
  with `kind: "mcp"`.
- **A-08.** `Outrig::launch`, `tools`, `call_tool`, `exec_capture` and `shutdown`, `image::*`,
  `LaunchSpec` and its `with_*`, `SidecarSpec`, `MountSpec`, `EmbeddedMcpPolicy`, `McpToolResult`,
  `ToolHandle` and `CapabilityProfile` keep their signatures and behavior: this task reads
  `SidecarSpec` and `McpServerSpec`, calls `add_sidecar` and the client's constructors, and adds
  its types beside them. `McpStartupFailure` gains fields additively, as above.
- **Docs.** `doc/concepts/mcp-servers.md` gains the Session's placements, the startup outcomes and
  the credential rule; `doc/concepts/mcp-trust-model.md` the rule and what a placement grants;
  `doc/reference/config.md` the key fork 5 adds, if taken; `SECURITY.md`'s known boundaries an
  entry for a service the Session places. `container_spec`'s doc comment and `run-new`'s message
  say what now starts and under which rule. `crates/outrig/CHANGELOG.md` `[Unreleased]`.
- **`## Decisions` records** the quoted sentences of `mcp-wrappers.md` ("Until it exists, `run-new`
  starts no MCP server") and `security.md` ("That is also why `run-new` starts no MCP server ...")
  with what replaces each and why, the forks taken, the backoff constants, and the spellings.

## Acceptance

Mock-driven unless marked: a fake MCP server built on rmcp's server side over an in-process
transport, as `mcp.rs`'s tests build one, stands behind a fake `Placement`; the session runs in the
host-session test mode `builder.rs` provides, with no container; `0003-44`'s service table and its
test service are in place.

- **Three placements through one facility.** A session declaring a `Command`, a `Sidecar` and a
  `PrimaryView` service starts all three (fake placements), each `Ready` with its listing's size,
  with one `mcp.service.started` each naming its placement kind and `declared_in`, followed by
  `service.bound` with `kind: "mcp"`; `runtime.bindings` lists the three as services; a call
  through the generic object reaches the fake. Gated e2e, as `tests/mcp_handshake.rs` is: the
  `mcp-fs` fixture image placed as a sidecar and as a primary-view entrypoint, and a command in the
  primary, through a `SessionBuilder` against real podman.
- **Two picks coexist.** `gh_a` and `gh_b` declare one command with `TOKEN = "${A}"` and
  `TOKEN = "${B}"`; the fake placement records two connects with different hidden environments;
  neither value appears in any event, outcome, error, `Debug` rendering or the report, checked by
  searching their serialized forms; the command renders `TOKEN=${A}`.
- **A missing binary names the declaration.** A fake whose connect exits 127 yields
  `Misconfigured` naming the service, the exit and the stderr tail, after one attempt, with no
  `mcp.service.retry`.
- **Transient failures are retried within the budget.** A fake whose first `initialize` never
  answers and whose second does yields `Ready { attempts: 2 }` and one `mcp.service.retry` with a
  `transient` cause, within a budget the test sets; one that never answers yields `Unavailable`
  once the budget is spent, with every attempt bounded as `STARTUP_TIMEOUT` bounds one.
- **A refusal is definitive.** A fake answering `initialize` with a JSON-RPC error yields
  `Refused` after one attempt; an unknown failure yields `Unclassified`, named so.
- **The credential rule.** A `Command` and a `PrimaryView` declaration with `${VAR}` in `env` are
  `Misconfigured` before anything connects (the fake's connect count is 0); the same `env` on a
  `Sidecar` with the default view starts, and on a `Sidecar` with `view = Primary` is
  `Misconfigured`. A `${VAR}` under a key podman reads is `Misconfigured`. A header naming an
  identity header is `Misconfigured`. A duplicate name, or one a binding uses, fails `start`.
- **Secrets go through the resolver.** A counting `Secrets`, as `session_tests.rs` builds one,
  resolves each `${VAR}` once; the process environment is never read for it.
- **Configuration lowers into declarations.** A config with a credential-free `[images.<n>.mcp]`
  command entry, a sidecar entry with `${VAR}`, a label-declared entry and a server named `my-fs`
  lowers into declarations with their sources, `my-fs` bound as fork 3 says with `server: "my-fs"`
  in its events; `run-new` prints one outcome line each, in a CLI test.
- **The control.** `writers()` says a `Command` and a `PrimaryView` are writers, a `Sidecar` with
  `workspace = "rw"` is a writer of the workspace, one with `"ro"` is not, and one with
  `view = Primary`, no workspace access and no mounts is a writer of the primary's filesystem.
  `quiesce(Pause)` on a sidecar freezes the fake placement and returns `Quiesced`, a call made
  meanwhile during a cut waits and runs after `resume`, one made under an owner's pause outside a
  cut is `refused`, reason paused, through the generic object; `resume` thaws it and the next call
  works; `quiesce(Pause)` with a call in flight past the bounded wait returns `InFlight` naming it;
  `quiesce(Stop)` sends the token and the call ends `unknown`; `quiesce` on a `Command` returns
  `Uncontrolled` naming it; a fake whose freeze fails returns `Uncontrolled` with the reason. Gated
  e2e: a sidecar writing a file in a loop is paused, the file stops changing, and changes again
  after `resume`.
- **The close.** At `shutdown`, each ready service's transport is closed and its placement
  stopped; a paused one is resumed first; `mcp_services` lists each with `proven`; a fake whose
  stop fails makes the report not proven stopped, as a container's would. The verdict is otherwise
  unchanged by services.
- **The classification sits beside the cause.** `McpStartupFailure::source` is the type it was;
  `kind` and `classified` are filled for every `ClientInitializeError` variant, named one by one;
  `REVIEWED_RMCP` equals the locked version, and the test's message names the reviews.
- **A host program.** `outrig.Session(mcp=[...])` with a fake placement enters, and `session.mcp`
  holds one outcome per declaration as the engine answered; a program test as `program_tests.rs`
  has them. No value of a `${VAR}` reaches the program.
- **The facade is unchanged.** `cargo public-api` over the items A-08 names shows no change from
  `8fb6eac`; `McpStartupFailure` shows additions only.
- `crates/outrig/public-api.txt` regenerated; `cargo test --workspace`,
  `cargo clippy --all-targets`, `cargo fmt --check` pass; the docs, `lifecycle.md`'s rows and
  `events.md`'s entries say what the code does, checked by reading.

## Design forks

1. **When services start -- Recommended: concurrently with the interpreter's start, after
   `Outrig::launch` (`builder.rs:415-422`), joined before the interpreter is told the services
   (`0003-44`'s declaration line) and before `assemble`.** Both need only the container. The
   alternative, after the interpreter is up, is simpler to order and adds the services' startup to
   the session's.
2. **Configuration and label entries as declarations -- Recommended: yes**, under the credential
   rule and `env-allow`, so `run-new` starts what the config declares. The alternative keeps the
   builder as the only source and leaves the config's MCP entries dead under the harness.
3. **A config server name that is not an identifier -- Recommended: bound with `-` replaced by
   `_` where that name is free, else `misconfigured` saying so, with the server's own name kept as
   `server` in events and logs.** A builder declaration's name is held to `bind`'s rule outright.
   The alternative refuses every such entry, which makes `run-new` unable to start a server the
   legacy `run` started.
4. **The connect budget's default -- Recommended: 120 seconds**, one attempt's bound, so a session
   that sets nothing waits as `outrig run` waits and still retries a fast failure. The alternative,
   a longer default, delays every session whose server is down.
5. **A config key for the budget -- Open; recommended: a top-level `mcp-connect-budget-secs`**,
   beside `mcp-call-timeout-secs`, so `run-new` can set it. The alternative is defaults only for
   the CLI.
6. **A stdio server that exits before `initialize` -- Recommended: exit 126 or 127 is
   `misconfigured`, any other status `unclassified`, still running at the bound `transient`.** The
   alternative treats every early exit as transient, which retries a crash loop for the budget.
7. **A `Placement` trait for the MCP table -- Recommended: yes, sealed**, with a fake for the
   tests; the alternative tests only against containers, which is e2e-gated throughout.
8. **Backoff -- the task's constants**, recorded in `## Decisions`.

## Dependencies

- **Hard: `0003-44`.** The service mechanism: `serve`, the table, the presenter-kind registry,
  the `service` facility kind, the call events and rows.
- **Hard: `0003-19`.** `SessionBuilder`, `Session`, the report and the lifecycle.
- **Hard: `0003-20`.** Bindings' declaration, gathering and the `Secrets` resolver on the builder.
- **Hard: `0003-29`.** `outrig.Session` and the owner protocol the declarations cross.
- **Soft: `0003-39`.** This task adds no interpreter code of its own; the service task before
  it and the presenter task after it do.

## See also

- `plan/phase/0003-python/mcp-presenter.md` -- the whole design; this task is its placement
  and lifecycle column.
- `plan/phase/0003-python/services.md` -- the mechanism a placed server is bound into.
- `plan/phase/0003-python/mcp-wrappers.md`, `security.md` -- the sentences this task's
  `## Decisions` quote.
- `plan/phase/0003-python/lifecycle.md` -- "Admission", "Closing a session", "The close table",
  "The report".
- `plan/phase/0003-python/embedding.md` -- "What the embedder supplies", on the resolver.
- `plan/phase/0003-python/continuation.md` -- the cut that calls the control (`0003-62`).
- `A-08-container-facade-stays-for-both-engines.md` -- the items this task adds beside.
- The `plan/next/` entries on the MCP startup failure's erased cause and on the rmcp upgrade
  review -- folded into this task.
- `crates/outrig/src/mcp.rs`, `outrig_.rs`, `harness/builder.rs`, `harness/session.rs`,
  `harness/report.rs`, `harness/lifecycle.rs`, `config/env_value.rs`, `config/api_key.rs`;
  `crates/outrig-cli/src/cli/run_new.rs`.

## Decisions

- **A configuration sidecar hosting several servers stays one container** (the user, 2026-11-18).
  `[sidecars.tools]` named by `fs` and `git` runs one `tools` container with both servers exec'd
  in it, as the legacy `run` does; each server is a service of its own, with its own client and
  outcome. The control acts on the container, so quiescing `fs` holds, freezes or stops `git`
  with it, and the outcome names both. An owner's `McpPlacement::Sidecar` hosts one server in a
  container of its own: only configuration lowering shares, since two owner declarations with
  different credentials in one container would read each other's environment through `/proc`.
- **Fork 5 taken: `mcp-connect-budget-secs`** (the user, 2026-11-18), a top-level key beside
  `mcp-call-timeout-secs`, 1 to 3600, so `run-new` and a host program's engine can set the
  budget; an owner's `mcp_policy` takes its place.
- **The end of the waiting, for placement and startup.** `mcp-wrappers.md` (now: "until they
  land, `run-new` starts no MCP server"; at planning, "Until it exists, `run-new` starts no MCP
  server") is replaced by the MCP table: a Session places each declared server and binds it into
  `services.md`'s table, and `run-new` starts what its configuration declares. `security.md`'s
  "That is also why `run-new` starts no MCP server: one placed in the primary would hold its
  resolved secrets beside the interpreter, as the same user" is narrowed to what its first
  sentence always said: no server *holding a credential* -- a `${VAR}` in its `env` -- is placed
  in the primary or a primary view; such a declaration is `misconfigured`. A credential-free
  command or primary view starts. The pointer edits to the plan pages are the integrator's.
- **Forks.** 1: servers start concurrently with `Interpreter::start` after `Outrig::launch`
  (`tokio::join!`), the primary image's label read on the servers' side of the join, joined before
  `serve_services` tells the interpreter; an interpreter that does not start cancels what has not
  (`Unavailable`, "given up") rather than waiting out the budget. 2: configuration and
  label entries become declarations. 3: a configured name binds as itself, or with `-` read as `_`
  where that is free; otherwise it is `misconfigured`, listed as `<server> (unbound)` so it cannot
  shadow the service that took its name, and `server` keeps its own name in events. 4: a 120 s
  budget. 5: the user's (above). 6: exit 126/127 `misconfigured`, another exit `unclassified`,
  still running `transient` -- applied to a transport failure only, so a handshake that timed out
  is not judged by the exit its closed stdin caused. 7: the placement trait, with a fake. 8: below.
- **Backoff and bounds.** Waits of 500 ms doubling to 8 s, no jitter (a session's servers start
  once; nothing herds), each one an `mcp.service.retry`, never past the budget. One attempt's
  handshake is bounded by `min(STARTUP_TIMEOUT, budget left)` -- `McpPolicy`'s crate-private
  `attempt`, which a test lowers -- its `tools/list` by what is left, and the whole attempt, its
  container's preparation and attachment included, by what is left too. One unfinished then is
  dropped: `Container`'s drop removes a container it created, the interceptor's `Rollback` undoes
  an attach in flight, and an entrypoint's uncommitted attachment retires -- the drop the start's
  cancellation already relied on. So a wedged preparation cannot hold the session's start past
  the budget, and no server starts after it. `McpPolicy::reconnect_max` defaults to 3, for
  `0003-56` to read.
- **The classifier** (`mcp.rs`, `init_error_class`): transport and connection closed transient;
  a JSON-RPC error refused; no compatible or preferred protocol version misconfigured; the four
  wrong-response variants and `Cancelled` unclassified; `LegacyFallbackFailed` as its `fallback`.
  `ClientInitializeError` is `#[non_exhaustive]`, so the plan's "no `_` arm" cannot hold: the
  wildcard files a new variant as `Other`, unclassified, and `REVIEWED_RMCP` (3.4.1) is the guard,
  a test holding it to `Cargo.lock` and naming both classifier reviews,
  `SUPPORTED_PROTOCOL_VERSIONS` and `plan/next/rmcp-list-result-spec-gaps.md`. The startup class
  rides `McpStartupFailure` in a crate-private field beside the public `kind` and `classified` the
  task names (`kind` repeats `classified.kind`, for a match that need not reach in). The two
  `plan/next/` entries the task folds were already removed when the plan was groomed.
- **Configured servers reach the session in the `LaunchSpec`.** `container_spec` lowers the
  block's `[mcp]` entries, the sidecars they name (each image built, pulled or found; a failure is
  that sidecar's servers' `misconfigured`) and those sidecars' labels into a private
  `LaunchSpec::configured_mcp`, which `Outrig::launch` never reads; the builder takes it at
  start and reads the primary's label once the primary runs. No signature changes, so `run-new`
  and the host-program engine, which pass `container_spec`'s spec to `container()`, start them
  unchanged (A-08). A label that cannot be read is a warning and adds nothing; a label entry
  referencing a variable `env-allow` does not list is `misconfigured`, where `outrig run` fails.
  A `start = "manual"` sidecar's servers are `misconfigured`: a session starts every server.
- **Sidecars start apart from the `Outrig`.** `add_sidecar`'s body moved into `SidecarHost`, a
  cloneable handle holding the primary's facts, the log directory and the session's interceptor,
  now `Arc<tokio::sync::Mutex<Option<NetworkInterceptor>>>`, locked only to attach, detach or
  retire -- never across a handshake, so concurrent starts do not queue. `add_sidecar` goes
  through it with its behavior unchanged; `UncommittedAttachment` retires on drop by `try_lock`,
  else on a task. The shutdown takes the interceptor out of its lock whoever holds the `Arc`, so a
  late detach or retire finds nothing rather than the shutdown finding it shared; the session
  closes its MCP table -- every container at once -- before `outrig.stop()` all the same, so its
  sidecars are detached and stopped first. An owner's sidecar is named
  `mcp-<service>`; a configuration's keeps its block's name, as `outrig run`'s does. A block
  named as an owner's sidecar is not placed -- its servers are `misconfigured` -- since in its
  stead the owner's server would run, with its resolved values, in what the repository's
  configuration describes. Refused rather than renamed: an owner's sidecar name passes the same
  sidecar-name rule a block's does, so no spelling keeps the two apart. The primary's host key is
  `<primary>`, which no sidecar name matches, so a block named `primary` is a sidecar like any
  other; and `Plan::sidecar` never replaces a host planned before it.
- **The placement trait is crate-private**, which is what seals it: the primary, the sidecar and a
  test's fake are its only implementations. It is `Placement` in `harness::mcp_placement`, beside
  the unrelated public `container::sidecar::Placement`. A command's freeze is never asked: the
  table answers `Uncontrolled` for a command placement itself.
- **`McpService::call` does not watch the token** until `0003-56`: a call told to stop -- the
  deadline, an interrupt, the close, `Quiesce::Stop` -- is abandoned after the service policy's
  grace, the dropped future's `CancelOnDrop` sends the cancel notice, and it ends `unknown`. The
  client has no deadline of its own (`Duration::MAX`): one would fire inside the table's grace and
  end the call `raised`, though its server may have done the work.
- **No close flag in the MCP table.** The deliverable has it hear the close through
  `Lifecycle::on_close`; every start ends before the `Session` exists, so such a flag could never
  be read while it mattered. The close itself is `McpServices::close`, which `Session::shutdown`
  and `abandon` call; `0003-56`'s reconnect reads `Lifecycle::closed()`, as the service table's
  admission does.
- **Pause in the service table.** `ServiceReason` gains `Paused` and `Stopped`; the table holds a
  per-service `Hold` (refuse, wait until, stopped), checked with the flight marked dispatched in
  one change, so a quiesce that sets a hold and then waits for dispatched calls cannot miss one.
  A call waiting on a cut's hold is refused `paused` at its `until`, cancelled at its own deadline
  or when told to stop, and refused `admission_closed` at the close: the table now hears the close
  through `Lifecycle::on_close` to wake it, where `0003-44` only read it.
- **The control's edges.** A pause whose calls outlast the wait (`InFlight`) or whose freeze fails
  (`Uncontrolled`) keeps the holds until `resume`, so the cut decides. A command placement is
  `Uncontrolled` and nothing changes. A stopped service is not started again; `resume` says so.
  `writers()` lists the services that started. `quiesce` takes the in-flight wait as an argument,
  and `Quiesce::PauseHolding { until }` is the cut's pause. A shared container's outcome names
  every service it hosts.
- **Identity headers** are held, never read; no placement here has headers, so the check that a
  declaration's header names one is `names_identity_header`, unit-tested, and the outcome-level
  test lands with `0003-57`'s URL placement.
- **Spellings.** Builder: `mcp`, `mcp_policy`, `mcp_identity`. Types: `McpServiceSpec`
  (`command`, `sidecar`, `primary_view`, `with_env`, `with_args`, `with_call_timeout_secs`,
  `with_description`), `McpPlacement`, `McpPolicy`, `McpServiceOutcome`, `McpStartupCause`,
  `McpRetryCause`, `McpOutcomeKind`, `McpPlacementKind`, `McpDeclaredIn`, `McpServices`
  (`outcomes`, `writers`, `quiesce`, `resume`), `McpWriter`, `McpWrites`, `Quiesce`,
  `QuiesceOutcome`, `McpServiceStop`, `McpStopHow`, `McpPauseHow`; `ServiceKind::Mcp`;
  `ShutdownReport::mcp_services`. The retry event's cause carries `kind` and `message` and no
  class, which is always transient there. Python: `outrig.McpCommand`, `McpSidecar`,
  `McpPrimaryView`, `McpPolicy`; `session.mcp`, a read-only mapping of `McpOutcome` (with an
  `McpCause`); `ShutdownReport.mcp_services` of `McpServiceStop`. A program's sidecar mounts are
  `Mount`s, seen at their host paths. The `mcp` presenter kind is registered with the generic
  class until `0003-55` writes its own.
- **Verified against podman**, beyond the mock suite: the `mcp-fs` fixture as a command, a
  sidecar and a primary view (whose entrypoint must be an ELF the launcher can run -- the fixture's
  `entry.sh` is refused, so the test derives an image naming `node`), and a sidecar appending to a
  workspace file in a loop, which stops changing while paused and grows again after `resume`.
- **Review fixes.** A sidecar's stop that fails keeps its container (`stop_or_keep`) for the next
  stop, and the close stops again a container a quiesce's stop left unproven, so the verdict says
  whether it stopped. A pause asked of a service already paused sets the hold it asks for: calls
  waiting on the earlier hold read the new one at once, and nothing is frozen twice. A server in
  the primary is stopped as the primary is: the close closes its transport, and
  `McpServices::closed`, called after the container's stop, gives its row and its
  `mcp.service.stopped` that stop's proof (the test's `fail_stop` now counts as that stop). A
  live object takes no MCP service's name, started or not: `outrig.Session` reserves the names it
  declares before entry and every service's after, and `Session::inject` refuses a name a
  binding, a service, an MCP service or a live object holds, which an object injected before
  entry under a configured server's name meets at the start. `outrig.Session(mcp=...)` reads its
  iterable once. The renderer's sweep plants the six `mcp.service.*` events and a report row.
  Re-running the podman e2e found a race the review did not: a primary view's server exits as
  its transport closes, its `--rm` removes it while `podman stop --ignore` runs, and podman
  reports it missing all the same, so the session read not proven stopped about one run in two.
  `Container::stop` now reads "no such container" for a stop by id as gone -- an id is never
  handed out twice -- and the e2e passed five runs of five after.
- **Second review.** A container an attempt makes is held from its creation until it has an
  owner: `prepare` holds it in `Unowned`, a guard that lands it in `abandoned` if dropped -- the
  attempt cut off at its budget, or the start cancelled -- and an unwind stops it in place
  (`Container::stop_in_place`), so one dropped mid-stop is kept too; an entrypoint host's
  container sits in the placement's own slot from creation through its connect. The close's stop
  then finds and proves it, where `Drop`'s detached removal would have proven nothing. The
  listing is bounded by the attempt's bound as well as the budget. A row for a server that never
  started is as proven as its container's stop. A paused-for request on a stopped service is
  `Uncontrolled`; a second stop is `Quiesced`. A connect budget outside 1..=3600 s is refused --
  by `outrig.McpPolicy`, by the engine decoding a program's, and by the builder's `check` -- never
  replaced by the default. An owner's MCP sidecar whose container name a launch spec's sidecar
  takes fails `check`, and a configured block a launch sidecar names is `misconfigured`, as for
  an owner's. A program's session has no workspace, so an `McpSidecar`'s `workspace` lowers to a
  mount of the session's `cwd` at its host path, with no more access than the innermost session
  mount holding it grants; with no `cwd`, outside every mount, or `rw` over a read-only mount,
  the start is refused before anything is made. Rejecting the option instead was the other
  choice, but its docstring promised the working directory, which this keeps.
- **Third review.** A sidecar's stop stops each container in place and detaches it from the
  interceptor only once every one is proven stopped: the interceptor's `detach` succeeds for a
  container that has exited, and one that would not stop stays filtered and audited, held in the
  placement for the next stop -- as is one whose stop was dropped part-way. An entrypoint host's
  failed connect no longer unwinds itself, whose detach came first; its container stays held for
  the stop, and an earlier one that would not stop is not replaced under its name. `quiesce` and
  `resume` run on a task of their own and to their end, as `Root::release` does, so an owner that
  gives up on one leaves no hold without its state; recording each state before its await would
  still leave a freeze dropped mid-`podman pause` unknown. A sidecar of one server's own is stopped
  before each retry, so each attempt gets a fresh container, as an entrypoint's always did, and
  after the start every sidecar none of whose servers started is stopped: a server that ignored
  its closed stdin would otherwise run, outside `writers()` and the control, until the close. A
  shared sidecar is not stopped between one server's attempts, its siblings being in it. A
  program sidecar's `rw` working directory carries each read-only session mount inside it as a
  read-only mount, the session's view kept rather than the request refused. The primary image's
  label read is raced against the start's cancellation and held to the connect budget, its
  timeout a warning that adds nothing. A handshake that failed is diagnosed within what its own
  bound left, and a client that exited is read once, its empty stderr empty for good; a timed-out
  handshake keeps the ceilings, its class not depending on them. An attempt gives its connect
  `min(attempt, left - 250 ms)`, a sidecar's preparation counted in it, so a refusal diagnosed to
  the end of its bound is reported before the attempt's own bound ends it as out of time.
  `REVIEWED_RMCP` and its test moved to `tests/reviewed_rmcp.rs`, excluded from the package as
  `public_api_boundary.rs` is: a shipped unit test read the workspace's `Cargo.lock`, missing in a
  package and a consumer's in a vendored copy. An e2e in `network_interceptor.rs` holds the detach
  of a stopped container to success.
- **Fourth review.** `quiesce` and `resume` lock their container -- an owned guard, the host's
  state now an `Arc` -- before they spawn, so a step asked for after one given up on waits for it
  rather than overtaking it: a resume that ran first would find the container running, return,
  and leave the pause to freeze it. One given up on before it locked does nothing. A sidecar none
  of whose servers started, and whose stop after the start was not proven, keeps those servers in
  `writers()` and the control, which says the stop is not proven; failing the session's start
  instead was the alternative, and would make one leftover container cost the servers that did
  start. A server in the primary that did not start is as proven stopped as the primary when it
  was launched -- a closed transport proves nothing of the process -- and proven only when it was
  never launched. A listing's failure is kept before its transport is closed, so a close that
  outlasts the attempt is not reported as the attempt timing out. A one-server sidecar's stop
  between attempts comes before the budget check, so a stop that ran past the budget starts no
  zero-time attempt and keeps the cause. The budget's end is set before the label read and given
  to each start, as the docs said: a stalled read and a stalled server took twice the budget.
- **Fifth review.** A command launched in the primary that did not start stays in `writers()`
  too: nothing stops the primary after a start, so what its attempts ran may still write there,
  and quiescing it gives the command placement's `Uncontrolled`. One never launched -- found
  misconfigured before anything ran -- is still left out.
