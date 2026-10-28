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
