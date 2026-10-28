# MCP presenter

An **MCP service** is one MCP server the Session places, starts, discovers, reconnects and tears
down, held in Rust by OutRig's own client and bound to the session as a **service** in
`services.md`'s sense: a `Service` OutRig ships, whose description comes from `tools/list` with the
tools' names kept exact and whose `call` dispatches to the Rust client. Its **presenter** is the
Python face of that service: the generated object `services.md` gives every service, built from the
`mcp` presenter kind, which presents a call's result as content blocks and adds what an MCP server
has and a Rust service does not -- a listing that can change. This page decides what is MCP's:
the declarations an owner supplies, where each placement runs and what it may hold, how startup
classifies and reports, what the presenter adds to the generic object, the three dispatch states a
call can end in and how they map onto a service call's outcomes, what the close does to services,
the control the cut uses to quiesce a service, and the MCP events. `0003-44` builds the service
mechanism every part here rides; `0003-54` placement, startup, secrets and the service lifecycle;
`0003-55` the presenter; `0003-56` the dispatch states and reconnect; `0003-57` the URL placement.
The requirements are OR-MCP1 through OR-MCP4 of `02-channels-and-mcp.md` and OR-A3-5 of
`A-03-rust-service-facilities.md`; the first application is the running example and no name of its
own appears in a mechanism here.

Spellings of types, methods and keys on this page are illustrative. Each task fixes its own, under
the rules the page states, and records them in its `## Decisions`.

## What exists, and what this page supersedes

What the code has at `8fb6eac`. The harness starts no MCP server: `container_spec`
(`crates/outrig/src/harness/builder.rs:808`) clears the image's MCP entries and every sidecar,
sets `EmbeddedMcpPolicy::Ignore` and returns the names it left out, which `run-new` prints as
"starts no MCP servers" (`crates/outrig-cli/src/cli/run_new.rs:715-719`). The default container
goes through it (`builder.rs:665`). An owner-supplied `LaunchSpec` passes through unchanged, and
`Outrig::launch` (`crates/outrig/src/outrig_.rs:1045`) does start its `mcp` map and sidecars, but
nothing in `harness/`, `agent/`, `python/` or `owner/` presents them: the model's one tool submits
Python. The client itself works and is used by the legacy `run`, by `outrig mcp` and by the image
build: `McpClient` over `podman exec` or `podman start --attach` stdio (`crates/outrig/src/mcp.rs`;
`list_tools` at `:324`, `call_tool` at `:403`, `shutdown` at `:479`), the content types that keep a
result whole (`crates/outrig/src/mcp_content.rs`: `McpTool` at `:46`, `McpToolResult` at `:129`,
`McpContent` with its `Other` kind at `:190` and `:205`, the lossy `render_text` at `:175`), the
sanitizer for a 64-character tool namespace (`crates/outrig/src/tool_name.rs:66`), the proxy that
serves a union of servers (`crates/outrig/src/mcp_proxy.rs`), the `${VAR}` values for a server's
environment (`crates/outrig/src/config/env_value.rs:37`, resolved from the process environment at
`:98`), and `SidecarSpec` with its views and mounts (`outrig_.rs:303`). Only the server side of
rmcp's streamable HTTP transport is enabled (`crates/outrig/Cargo.toml:60`).

`mcp-wrappers.md` decided the route on 2026-09-30 -- "the route is the MCP client OutRig already
has, in Rust" -- and what the presented object has to be, whichever way it is built: synchronous as
a hosted object is, results that stay data, errors that are distinguishable with the unknown outcome
separate from every failure, timeouts and retries declared rather than assumed, names handled with
an escape, documentation from the schema. This page builds those; it does not restate them.

What this page supersedes is the waiting. `mcp-wrappers.md` says: "What the route still needs is a
way to present a Rust object to agent Python with the same policy and events as a hosted Python
object, and that mechanism is deferred past this phase to
`plan/next/rust-object-as-python-object.md`. Until it exists, `run-new` starts no MCP server." The
0003 README's out-of-scope list repeats it, and `embedding.md` ("Services written in Rust") says
"`mcp-wrappers.md` routes MCP servers through it." That mechanism now exists: `services.md` builds
the trait the entry sketched, in `0003-44`, and the MCP presenter is its first service, as the entry
itself proposed ("This is also how an MCP server would become a Python object"). What this page
adds is what the generic mechanism cannot know of an MCP server -- placement, startup, secrets,
exact tool names, content-block results, a listing that changes, three dispatch states, HTTP --
and nothing that `services.md` already decides. `0003-54` records the end of the waiting for
placement, and `0003-55` for presentation; the pointer edits to the three pages are the
integrator's.

`security.md`'s reason for `run-new` starting no server stands and becomes a rule: "OutRig starts
nothing that holds a credential from its config where the agent can reach it. That is also why
`run-new` starts no MCP server: one placed in the primary would hold its resolved secrets beside
the interpreter, as the same user." The first sentence is the rule ("Secrets, placement and what a
service grants"); the second is narrowed from "no server" to "no server holding a credential", which
is what the first sentence always said. `0003-54` makes the change.

**A-08 holds throughout.** The 0.2 container facade -- `Outrig::launch`, `tools`, `call_tool`,
`exec_capture`, `shutdown`, `image::*`, `LaunchSpec` and its `with_*`, `SidecarSpec`, `MountSpec`,
`EmbeddedMcpPolicy`, `McpToolResult`, `ToolHandle`, `CapabilityProfile` -- keeps its signatures and
behavior on `version/0.3.x` while the first application runs both engines
(`A-08-container-facade-stays-for-both-engines.md`; `0003-40`). Every task here adds beside those
items: the harness places servers through a table of its own and `container_spec` still hands the
facade a launch spec with no server in it; `McpToolResult` is read, never changed; a public type
that gains anything gains it additively, on a `#[non_exhaustive]` item. Each task says so.

## The shape

```text
UNTRUSTED: the interpreter               TRUSTED: the owner's process                ELSEWHERE
+--------------------------------+       +------------------------------------+     +----------+
| gh_a = outrig.bindings["gh_a"] |       | the service table (services.md):   |     | server:  |
|   .call("create-issue", {...}) | serv- | admission, schema check, dispatch, |     |  sidecar |
|   blocks the calling thread ---+-ice.--+-> deadline, cancel, events, report |     |  primary |
|   woken by the reader thread <-+-call--+-- McpService: Service impl, one    +-----+  view    |
|   result: blocks, structured   |       |   per placed server, McpClient in  |     |  URL     |
|   (the mcp presenter kind)     |       |   it; placement, startup, quiesce  |     +----------+
+--------------------------------+       +------------------------------------+
```

Each placed server is one `Service` implementation OutRig ships, `McpService`: `describe()` is
the server's current listing as a `ServiceDescription` -- one method per tool, named exactly as the
server names it, with the tool's `description`, `inputSchema` as `params`, `outputSchema` as
`result`, and `title`, `annotations`, `icons` and `_meta` in `extras`; `presenter` is `mcp`; the
server's `call_timeout_secs` is the description's `call_deadline` -- and `call(method, args, cx)`
dispatches to the service's `McpClient::call_tool`, observing `cx`'s token. The service is bound
under the declaration's name, so it is a facility of kind `service` (`facilities.md`), reached as
`outrig.bindings[name]`, listed in `runtime.bindings` beside hosted objects, admitted, checked,
evented, bounded, cancelled and reported as `services.md` says of every service. What the table
does not know -- that a server has a process to place and a listing that moves -- is the MCP
module's, kept in a table of its own beside the service table: each service's declaration, client,
placement and state.

In one process a message is a Python object, never JSON: the presenter hands agent code a result
object, and the interpreter's kernels share the generated class. JSON crosses only the interpreter
pipe -- `services.md`'s lines, with the MCP result as the call's JSON value -- and, for a host
program, the owner connection, where the program sends declarations and the engine answers with
outcomes (`crates/outrig/src/owner/mod.rs`, the owner protocol).

## Declarations

An owner supplies **service declarations**: data, never a client or a process. Each names a
service and a placement, with what the placement needs:

- `name`: the bound name, under `bind`'s rule (`services.md`, "The trait": a Python identifier,
  not a keyword or a reserved name, unique in the session); it is how agents, events and the owner
  name the service. A configuration entry's server name follows the config's own rule,
  `^[a-zA-Z][a-zA-Z0-9_-]*$` (`crates/outrig/src/config/validate.rs:2475`), which admits `-`; how
  such a name is bound is `0003-54`'s fork, and the server's own name stays in events and logs as
  `server`. `outrig` is reserved (`tool_name.rs:48`).
- `placement`, one of:
  - **command** -- an argv exec'd in the primary container over stdio, with an `env` table: the
    `McpServerSpec` short and full forms of today (`crates/outrig/src/config/mod.rs:2870`), as
    `McpClient::connect_via_podman_exec` starts one.
  - **sidecar** -- a `SidecarSpec` (`outrig_.rs:303`) hosting one exec-stdio or entrypoint-stdio
    server, with the image, workspace access, mounts, security and `env` the spec carries, as
    `Outrig::add_sidecar` (`outrig_.rs:1237`) starts one.
  - **primary-view** -- an entrypoint-stdio sidecar with `SidecarView::Primary`
    (`config/mod.rs:2615`; `doc/concepts/mcp-servers.md`, "Primary filesystem view"). A `sidecar`
    whose `SidecarSpec.view` is `Primary` is this placement under another spelling: the effective
    view is normalized before any check below reads the placement.
  - **URL** -- a streamable-HTTP endpoint with a `headers` table and an optional trust anchor
    (`0003-57`).
- `env` and `headers` values are `EnvValue`s (`config/env_value.rs:37`): a literal, or a `${VAR}`
  reference the session's `Secrets` resolver (`crates/outrig/src/config/api_key.rs:46`) resolves on
  the trusted side. The `${VAR}` syntax is the config's own (`doc/reference/config.md`, "MCP `env`
  value syntax").
- `call_timeout_secs`, optional, as `McpServerSpec::Full` carries it today, which becomes the
  service description's `call_deadline`; `description`, optional, one line for the orientation,
  else the server's `instructions` first line, bounded.

Two picks of one declaration with different inputs are two declarations with two names -- `gh_a`
and `gh_b`, each with its own `env` -- and coexist: a command or sidecar placement is a process or
container of its own, and a URL placement an HTTP session of its own, so the two credentials never
share a process. That is the difference from bindings, where one process holds every factory
binding and "an embedder that needs two credentials kept apart runs them in two sessions"
(`security.md`, "The trusted side runs on the host"). Resolving an application's picks and inputs
into declarations is the application's work; OutRig sees declarations.

Declarations are fixed when the session starts, as bindings and services are (`security.md`, "What
this does not change"; `services.md`, "The trait"); adding a service to a running session is not in
this phase. What a placement sees of the workspace is its container's mount: a sidecar with
`workspace` access, a primary-view or command placement, sees a file the agent puts there during
the session -- a repository it clones -- by the mount's own semantics, with nothing added here, and
a URL service sees no file of the session's.

Beside the declarations, an owner may supply an **MCP policy** -- a connect budget and a reconnect
bound, each with OutRig's default -- and **identity headers**: opaque name-value pairs stamped on
every request of every URL service the owner declared on the builder or `outrig.Session`, and on
no URL service a configuration declared, which a declaration's own `headers` may not name. One of
them may be a fence token; OutRig reads none of them and carries each unchanged ("Identity and the
fence token"). The call deadline, the result bound and whether bodies are recorded
are the service policy's (`services.md`, "The trait"), shared with every service.

Where declarations are given:

- **A Rust owner**: on `SessionBuilder`, beside `bind` (`builder.rs:225`), `serve` (`0003-44`),
  `secrets` (`:187`), `approver` (`:251`) and `skill_source` (`:278`): `mcp(name, McpServiceSpec)`,
  `mcp_policy`, `mcp_identity`. The declarations are the embedder's own and need no approval, as a
  binding or a service given through the API needs none (`embedding.md`, "What the embedder
  supplies").
- **A host program**: on `outrig.Session` (`crates/outrig/src/python/outrig/_host.py:703`), as
  Python data -- `mcp=[...]`, `mcp_policy=`, `mcp_identity=` -- that the program sends and the
  engine receives in `session.start` as JSON, with the outcomes in the answer. The program holds no
  client. Python-host parity is a deliverable here only because the data is the same; the first
  application is a Rust embedder (`A-00-index.md`).
- **The configuration**: `[images.<name>.mcp]` entries and the `[sidecars.<sc>]` blocks they name
  (`doc/reference/config.md`), and the image's `org.outrig.mcp` label under `env-allow`, become
  declarations under the same rules, so `run-new` starts what the config declares. A repository's
  entry is content the agent can write, which is why its `${VAR}` references are already held to
  `[repo] env-allow` (`doc/concepts/mcp-trust-model.md`); this page adds the placement rule below
  and one gate: a repository's URL entry (`0003-57`) starts only once the user approves that exact
  declaration, through the `Approver` a repository's binding passes, since the host dials a URL
  service itself and a repository's entry would otherwise reach loopback and host-only services the
  container cannot; the global config needs no approval, as for a binding. The container launch
  itself still starts none: `container_spec` keeps clearing the launch spec, and the MCP module
  starts them.

A declaration that fails validation -- a URL carrying userinfo, a `${VAR}` in a primary or
primary-view placement, a header naming an identity header, an `env` key podman reads itself -- is
an outcome, `misconfigured`, before anything of it starts, and the session starts without it. A
name given twice, or one a binding or service already uses, is a start error, as a binding's name
given twice is (`harness/bindings.rs:32` and `gather`): there is nothing sensible to start.

## Startup

Each declared service is placed and started as the session starts, after the container is up.
Starting is: place (exec in the primary, start a sidecar, create and attach an entrypoint
container, open an HTTP session), the `initialize` handshake, `tools/list` with the paging
safeguards `list_tools` has, the first **listing** -- the description's generation 1 -- and the
binding of the `McpService` into the service table, which publishes `service.bound`.
`STARTUP_TIMEOUT` bounds one attempt as it does today; the MCP policy's **connect budget** bounds
the attempts together.

Every failure is classified before anything is decided about it:

| class           | examples                                                          | then  |
| --------------- | ----------------------------------------------------------------- | ----- |
| `transient`     | transport failed or closed; connect refused; handshake timed out; | retry |
|                 | HTTP 429 or 5xx                                                   |       |
| `refused`       | HTTP 401 or 403; `initialize` answered with a JSON-RPC error      | stop  |
| `misconfigured` | binary not found (exit 126 or 127); image not present; `${VAR}`   | stop  |
|                 | unresolved; the declaration failed validation; TLS verification   |       |
|                 | failed; protocol version not negotiable                           |       |
| `unclassified`  | anything else, a stdio server's other exit statuses included      | stop  |

A transient failure is retried within the budget, with a backoff of OutRig's choosing, and each
wait is an event carrying the redacted cause. A definitive one stops at once. An unclassified one
stops too, and says it is unclassified rather than guessing: "report unknown classification
honestly" is the requirement's own wording. The retries concern establishment, never a call's
effect ("Calls").

`McpStartupFailure::source` (`crates/outrig/src/error.rs:431`) is an erased `Box<dyn Error>`
today, built by `enrich_startup_error` (`mcp.rs:592`) from rmcp's `ClientInitializeError`, while a
mid-session failure is classified into `McpSessionError` with an `McpFailureKind` (`error.rs:395`,
`:363`; `session_error_from_rmcp`, `mcp.rs:817`).
`plan/next/mcp-startup-failure-erases-its-cause.md` asks for one representation; `0003-54` absorbs
it. Its option 1, retyping the field, would change the payload of `OutrigError::McpStartupFailed`,
an item the first application's rig engine matches on (A-08), so the task takes the other half of
each option: the classification is added beside the erased `source`, additively on the
`#[non_exhaustive]` struct, by a second classifier over `ClientInitializeError`'s variants, and
the field's doc says what it holds and that downcasting it is not a supported surface. The
classifier joins the review `plan/next/rmcp-list-result-spec-gaps.md` records for every rmcp
upgrade, and `0003-54` makes that review a test, as the folded `plan/next/` entry on the rmcp
upgrade review proposed.

Startup returns a **per-service outcome**: `ready` with the listing's size, or `unavailable` after
the budget, `refused`, `misconfigured`, `unclassified` or, for a repository's URL entry the user did
not approve, `unapproved` (`0003-57`), each with a redacted cause -- the declaration's name, the
placement, the command with `${VAR}` references shown as references, the exit status, and the stderr
tail the client already captures. The session starts with the services that are `ready`; the owner
reads the outcomes from the `Session` (a host program from its `session`) and decides what each
means for its own task. OutRig takes no task-state decision. A sidecar's `on-failure` key is not
consulted here: the outcome is the owner's to act on.

Where a start waits: the services start concurrently with each other, and the orientation that
lists them is composed once per agent (`crates/outrig/src/agent/orientation.rs:66`), so the
outcomes are in before the first round. Whether they start concurrently with the interpreter's
start or after it is `0003-54`'s fork.

## The Python surface

What is generic is `services.md`'s, "The generated object", and this page cites rather than
restates it: one class per description per interpreter and one instance per kernel; a method per
tool whose exact name is an identifier, with `__signature__` and `__doc__` rendered from
`inputSchema` and the tool's description, and `(**arguments)` where the schema yields no signature;
`call(name, arguments=None, *, timeout=None)`, `method(name)` and `methods` for every tool by exact
name, so `read-file`, `class` and `a__b` are callable without exception, which is what OR-MCP2's
acceptance asks; the attribute convenience -- `-` to `_`, a trailing `_` for a keyword or a member,
an ambiguity error naming the exact names where two tools meet one attribute -- and `dir()`; the
bounded `help()`; the listing in `runtime.bindings` with `kind: "service"` and the class's
qualified name; `outrig.bindings[name]` as the authoritative lookup; a convenience global where the
name is free; the schema check in Rust before dispatch, with a tool whose `inputSchema` the
validator cannot compile dispatched unchecked and never dropped; the service kind's blocking rule
and `asyncio.to_thread` in the orientation. The proxy's `<server>__<tool>` sanitizer is not used
here: it exists for a 64-character namespace a model calls tools in, and the generic object has an
exact-name path instead.

What the `mcp` presenter kind adds, in `0003-55`:

**The description is the listing.** `McpService::describe()` is the current listing: each tool's
exact `name`, `description`, `inputSchema` as `params`, `outputSchema` as `result`, and `title`,
`annotations`, `icons` and `_meta` in `extras`, as `McpTool` holds them (`mcp_content.rs:46`). The
presenter exposes `tools`, the descriptors as the protocol has them, and `tool(name)`, the
exact-name lookup returning a `Tool` -- `method(name)` with the descriptor beside it -- that calls
by the name it was resolved with and keeps its `listing`. Nothing of a declaration's `env`,
`headers` or URL userinfo is in the description; the placement kind and the name are.

**Results are data.** `McpService::call` returns the `McpToolResult` as JSON -- `content` in order,
`structuredContent`, `_meta`, `isError` -- and the presenter wraps it into a result object holding
`content`, the ordered blocks, each typed -- text, image, audio, embedded resource, resource link,
and an `Other(kind, raw)` for a kind this build does not model, kept as the JSON it arrived as
(`mcp_content.rs:190`, `:205`); `structured`, as Python data, or `None`; `meta`; and `is_error`.
`text` is the derived view `render_text` defines (`:175`), lossy on purpose, and the result says so
in its docstring. A binary block offers its MIME type and its bytes. Python filters a
multi-megabyte structured result in the interpreter and prints a slice; what reaches the model is
the output observation OutRig already bounds, and nothing at the boundary flattens the result to
text. Whether a result with `is_error` raises or returns is `0003-55`'s fork, recommended raise,
carrying the result.

**Two bounds, named apart.** The **host result bound** is the service policy's `result_max`
(`services.md`, "The table"): a result larger than it is not handed over, and the call fails naming
the limit and the size, with the outcome `returned` recorded for the server -- it did the work; the
result exceeded what the session keeps -- and the dispatch state response received. It replaces
nothing of `tool-result-max` (`config/mod.rs:41`), which bounds the rendered text a model is sent
and goes on doing so for the legacy agent; here the model's bound is the execution's output
observation. A config key for the policy's bound, so `run-new` can set it, is `0003-55`'s fork.

**Relisting never retargets.** `relist()` runs `tools/list` again through the host, replaces the
description under the next generation -- `services.md`'s `service.described` line carries the
change to every kernel -- and returns what changed: added, removed, changed. A `Tool` object
already held keeps the exact name and descriptor it was resolved with and calls by that name; one
whose name the relisting removed is refused before dispatch as unknown method; attribute names are
recomputed, and one whose exact name is now ambiguous raises rather than picking. A server's
`tools/list_changed` notification is recorded and marks the service `stale`; it relists nothing by
itself ("Rejected alternatives"). A reconnect always relists ("Calls").

**A tolerant listing.** One malformed descriptor costs that tool, not the service.
`plan/next/one-bad-tool-schema-aborts-the-tool-list.md` corrected its own premise: rmcp's typed
decode of `tools/list` fails the whole page for a non-object `inputSchema`, and tolerance "needs a
lenient decode of `tools/list`". The seam is rmcp's `ServerResult`, whose last untagged variant is
`CustomResult(Value)`: a page the typed decode refuses arrives there through
`send_request_with_option`, and the MCP module decodes its `tools` one by one, keeping those that
decode and recording the rest by name and reason. `0003-55` absorbs the entry; "Unverified" names
what it must confirm.

**Protocol scope.** Tools and their results. Resources, prompts, sampling, elicitation and roots
are neither advertised nor answered beyond what rmcp's client does with a `()` handler; a server
asking for them gets what the SDK answers. Whoever adds resources or prompts "inherits the
cache-metadata gap" (`plan/next/rmcp-list-result-spec-gaps.md`) and does it for a selected server
that needs them, not for the SDK's sake.

**Blocking, as every service call blocks.** `services.md`, "The crossing", says it once: a call
holds the calling thread, and so a call on a kernel's own thread holds that kernel's event loop
until the answer; `await asyncio.to_thread(...)` keeps the loop turning. A worker's call ends by
the deadline at the latest, which a hosted call cannot promise (`hosted-objects.md`, "RPyC's
request timeout is off").

## Calls

A call has one of **three dispatch states**, and every failure says which:

- **Known not dispatched.** Nothing reached the transport: admission closed; the service is not
  ready, or its reconnect failed; the tool is not in the listing; the arguments failed the schema
  check or are not a JSON object; the deadline asked for is not positive. Effects: none possible.
- **May have dispatched.** The request was handed to the transport and no response was read: the
  write failed or was partial, the transport closed, the deadline passed, the caller was
  interrupted, the session closed while it was in flight. A failed write does not prove the server
  saw nothing; a cancel does not prove it stopped. Effects: open.
- **Response received.** The server answered: a result, with `is_error` true or false; a JSON-RPC
  error; a response the protocol does not allow there; or a result past the host bound. Effects: as
  the response says, partial ones included -- "a server-reported error is still a response".

The states map onto a service call's five outcomes (`services.md`, "Dispatch, deadline and
cancel") so one reader's rule covers every service: known not dispatched is `refused` with its
reason; may have dispatched is `unknown`, with the `McpFailureKind` as `failure`; response received
is `returned` for a result -- `is_error` either way, the presenter deciding what Python sees -- and
`raised` for a JSON-RPC error or a response of the wrong type, with a `ServiceError` category
(`InvalidArguments` for the protocol's invalid-params error, `NotFound` for a tool the server does
not know, `Failed` otherwise). The event carries both: `outcome` for the owner reading every
service, `state` for the agent's question -- did it happen.

Today's `call_tool` (`mcp.rs:403`) folds a failed send and a failed wait into one
`McpCallToolFailed` (`error.rs:242`) whose `McpFailureKind` says what failed and not where.
`0003-56` splits them in a bounded form of the call added beside `call_tool` (A-08: `McpClient` is
not among the frozen items, and its signatures stay as they are regardless): the state is decided
by where in the flow the failure happened -- before `send_request_with_option` returned, after it,
or on a response -- and `McpFailureKind` stays beside it as the kind. Python sees `McpError`, a
subclass of `outrig.ServiceError` with `state`, `failure`, `service`, `tool` and `call`, and a
subclass per state, so `except MayHaveDispatched:` is the spelling that keeps a write from being
repeated.

**No call is retried by OutRig**, under any state, and nothing is inferred from a tool's name or
its `annotations` (`read_only_hint`, `idempotent_hint`): they are advisory and unverified, as
`McpToolAnnotations`'s doc says, and `execution-and-rounds.md` ("No rollback, ever") already fixes
the rule for a lost result: "must never be retried automatically". Repeating a call is the agent's
decision, made with the state in hand; the reconnect below replays nothing.

**The deadline and the cancel are the service mechanism's** (`services.md`, "Dispatch, deadline
and cancel"): the service policy's default, the description's `call_deadline` from the declaration's
`call_timeout_secs`, a call's `timeout` lowering and never raising it, the token at the deadline,
on an interrupt and at the close. What is MCP's is what `McpService::call` does with the token: it
sends the server `notifications/cancelled` as the client does today (`CancelOnDrop`, `mcp.rs:513`)
and returns at once with the state may have dispatched and the failure `Timeout` or `Canceled`, so
the call never reaches the grace or the abort; a response that arrives later is discarded by the
client. Cancellation is the fourth of `execution-and-rounds.md`'s "Four different cancellations":
an accepted remote operation, whose cancel "may be impossible, and its effect may already have
happened". Nothing here claims a server-side stop, and the orientation says so.

**Reconnect serves future calls.** A failure of kind transport finishes the client's session
(`McpFailureKind::Transport`'s own doc). The service is marked disconnected and an event says so;
the failed call keeps its state. The next call on the service, or an explicit `reconnect()`,
re-establishes it: the placement is re-created from its declaration -- a new exec in the primary,
a new exec in a running sidecar or a new sidecar when its container is gone, a new entrypoint
container, a new HTTP session -- with the values resolved at start and never resolved again, within
the connect budget and the same classification as startup, then relisted, with the changes reported
through `service.described`; only then is the new call dispatched. The bound on reconnects per
service is the MCP policy's. Nothing of the earlier call is sent again. Closing a transport is not
stopping a server: a server process the old transport spoke to may still run until its container
is torn down, and the quiesce control below and the close account for it. After
`close_admission()` no reconnect is made.

## Admission, the close and the report

A call's admission, drain, cancel at terminating, report entry and verdict are the service
mechanism's (`services.md`, "The table", "Admission, the close and the report"): a call after the
close is `refused`, admission closed; a call in flight drains and is then sent the token, which for
an MCP call is the cancel notice and the state may have dispatched, outcome `unknown`, reason
`closed`; `ShutdownReport`'s `services` lists it. This page adds what a server has and a Rust
service does not.

The close table (`lifecycle.md`, "The close table") gains:

**MCP services** (`0003-54`): ready -- transport closed at terminating (`McpClient::shutdown`,
`mcp.rs:479`, which the page notes says nothing about the server), its container stopped with the
session's (`Session::shutdown`, `crates/outrig/src/harness/session.rs:396`); a server in the
primary ends with the primary; a URL server is disconnected, since it is not OutRig's to stop;
effects: what its tools did stands, and a write a sidecar was making is cut off by its container's
stop, which the report says was proven or not. Paused by the cut -- resumed, then as ready.
Disconnected, or reconnecting -- the reconnect is abandoned; nothing more. Not started --
`unavailable`, `refused`, `misconfigured`, `unclassified`, `unapproved`: nothing to stop; the report
lists the outcome.

**MCP calls** (`0003-56`), as the **Service calls** rows with two additions: a call waiting for a
reconnect at the close is `refused`, reason closed, known not dispatched; a call in flight ends
`unknown` with `state: may_have_dispatched` and the cancel notice sent.

The report (`lifecycle.md`, "The report"; `ShutdownReport`,
`crates/outrig/src/harness/report.rs:19`) gains `mcp_services` -- each service, its placement, how
it was stopped and whether that was proven -- and `mcp_uncertain`, the calls that ended may have
dispatched during the session, by call, service, tool and agent (`0003-56`'s fork), beside the
mechanism's `services`. A call in either with that state is an unknown outcome for the verdict
(`report.rs:41`): stopped with unknown, never clean, never failed. The three rules of the report
hold unchanged -- no false clean report, no rollback, a local stop says nothing about the remote
outcome -- and teardown reports a sidecar's remaining or uncertain activity whether or not the cut
selected anything it wrote (OR-MCP4). A host program reads the same report through its mirror.

## Secrets, placement and what a service grants

**Resolution is on the trusted side, through the session's resolver.** A `${VAR}` in a
declaration's `env` or `headers` is resolved by the `Secrets` the session was given
(`api_key.rs:46`), as a model's `api-key` is (`embedding.md`, "What the embedder supplies", which
names `ResolvedEnvValue::resolve`'s direct read of `std::env::var` as what the resolver replaces).
The value reaches a container server as podman's own environment, by name, never on a command line
(`doc/reference/config.md`, "MCP `env` value syntax"), and a URL server as a header the client
sends. Values are read once, at start, and held: a reconnect reuses them and never resolves again
(`api_key.rs`: "A key is read once, at start, and held for the session's life"), which is what
keeps a reconnect from adopting a newer credential or claim.

**Nothing OutRig injects appears where the agent or a reader can see it.** `services.md`,
"Internals never cross", is the rule for every service, and OR-A3-5 asks that both surfaces share
it: the description crosses without `env`, `headers` or URL userinfo; `repr`, `help()`, the listing
and every exception are built from the service's name, the placement kind and the protocol's own
data; every event and the report are built in Rust from known fields and name a `${VAR}` by its
reference. That is achievable here where it is not for hosted objects: `hosted-objects.md`
("Exceptions") says of a library's arguments and exception text "Nothing scrubs them ... no
library-neutral rule can tell a token from text", and the validation records that an MCP
presenter's events are OutRig's to build. The honest limit stays: a server's stderr tail and the
content a server returns are the server's text, and OutRig redacts neither. The promise is
non-injection, not secret-free content.

**A declaration that holds a credential may not be placed in the primary or the primary view.**
"Holds a credential" is: names a `${VAR}` in its `env` or `headers`. The rule is the one
`security.md` states and `container_spec`'s doc restates -- a server beside the interpreter, as the
same user, keeps its resolved secrets where `/proc` lets the agent read them, and "Why a second
process in the same container is not enough" is why no same-UID helper changes that; a primary-view
sidecar joins the primary's mount namespace with the capabilities `SECURITY.md` lists, so OR-MCP4's
"define and validate" has one answer: not with a credential. Such a declaration is `misconfigured`
before it starts. A credential-free command or primary-view placement is allowed; a credentialed
one is a sidecar or a URL. This resolves the conflict check's C7 and its tension 8 -- a required
placement whose safety was undecided -- by letting the placement stand for the servers it is safe
for. A literal in `env` is not a credential by this rule: OutRig cannot tell a token from a path,
and a literal secret in a config file is the operator's choice as it is today.

**What a placement grants**, in the sense `security.md` gives "What a binding grants": the service
is the grant (`services.md`, "The trait"), and every tool it offers is callable by every agent of
the session, recorded as events. A command placement runs as the session user beside the
interpreter: the agent can signal it, read its `/proc` entries and literals in its environment, and
it writes the workspace as the agent does. A sidecar sees what its container is granted and nothing
else (`doc/concepts/mcp-trust-model.md`, "Least-privilege placement with sidecars"); a primary-view
sidecar reads the primary's whole filesystem and is "inside the primary's trust boundary"
(`SECURITY.md`, known boundaries). A URL service is called under the headers OutRig stamps -- the
resolved credential and, for an owner-declared service, the identity headers -- so whatever the
server lets that identity do, every agent can do. The session's grant is per session, not per agent
(`security.md`, "Agents are not isolated from each other"): a child's narrower facility dict is
presentation, as `facilities.md` says. And "The agent still cannot grow its own environment" holds:
`relist` and `reconnect` act on declared services, and no agent-callable operation declares one.

**Identity and the fence token.** The owner's identity headers are a grant to destinations the owner
chose: they are carried unchanged on every request of every URL service the owner declared on the
builder or `outrig.Session`, and on no URL service a repository or image configuration declared,
whatever that declaration's approval, since a repository is content the agent can write and its URL
is a destination the agent chose (`doc/concepts/mcp-trust-model.md`); approving a configured
declaration grants what the declaration says and nothing of the owner's. An owner that wants
identity at a configured service's URL declares that service itself, under a name of its own. They
are never read, logged or previewed; a declaration whose `headers` names one is `misconfigured`. A
command or sidecar server receives identity only through the `env` its declaration gives it; OutRig
stamps nothing there, since there is no header to stamp. That is the intended scope of OR-MCP4's
"carry owner-supplied opaque identity unchanged": a URL service, which is where the first
application's fence-aware service lives; an application that needs the fence at a command or sidecar
placement passes the token in that placement's `env` itself. A service that can enforce a fence does
so with the token it is handed; one that cannot is not promised a stop.

**Revocation is the owner's one-way close.** The requirement writes "close admission" for what
happens when the owner's claim is lost, and the design's word for that is `close_admission()`
(`session.rs:299`; `lifecycle.md`, "Admission"): no new call is dispatched, no reconnect is made,
no credential is resolved again, and a call in flight ends as the close table says, may have
dispatched -- "an unfenced remote call may finish after revocation", which the report records as an
unknown outcome and never as a failure. The pausable gate the suspend requirements also call
"admission" is a different mechanism with its own name, the pause, defined in `continuation.md`;
nothing on this page reopens.

## The pause-or-stop control

MCP servers with write access to state a cut covers are writers that participate in
`continuation.md`'s cut: quiescing the interpreter is not enough while a sidecar indexes or a build
a tool started runs on. `continuation.md` decides the pause, what a cut covers and what a refused or
degraded cut says; this page defines the control it uses, which `0003-54` builds and `0003-62`
calls. The control is on the Rust `Session`, per service:

- **Which services are writers.** From the declaration alone: a command placement is a writer (it
  runs in the primary with the workspace); a primary-view placement is a writer (the view holds the
  workspace read-write), a sidecar whose spec's `view` is `Primary` included, whatever its
  `workspace` and `mounts` say; a sidecar is a writer when its `workspace` is `rw` or any of its
  `mounts` is read-write, and for those paths; a URL service is not a writer, since it reaches no
  file the session mounts. The control reports `writer` per service with the paths it can reach, and
  the cut decides which to quiesce against its designation.
- **`quiesce(service, how)`**, where `how` is pause or stop. Pause holds new dispatch for the
  service: during a cut a call made meanwhile waits, bounded by the operation's deadline, as the
  relay holds a boundary request during the pause (`continuation.md`, "The pause and the cut") --
  the round the cut lets finish is admitted whole, and a loop task's call waits -- while a service
  the owner pauses outside a cut answers a call `refused`, reason paused, until the resume, since a
  model should learn that a service is paused rather than wait on it. Pause then waits a bounded
  time for the service's in-flight calls in the service table to end, and freezes the container
  (`podman pause`), which stops every process in it, descendants included, since the freezer acts on
  the container's cgroup. Stop sends each in-flight call the token with the reason `stopped` --
  the cancel notice goes to the server and the call ends `unknown`, may have dispatched -- then
  closes the transport and stops the container. A command placement cannot be frozen apart from the
  interpreter it shares a container with, and the control does not pretend otherwise.
- **The outcome**, one of: quiesced, with how; in flight, naming the calls that had not ended when
  the wait ran out, their states uncertain; uncontrolled, for a command placement, or a container
  whose pause failed, naming the service and why. The cut refuses or reports degraded on in flight
  and uncontrolled, naming the writer, as the requirement demands; the control never reports
  quiesced without the freeze or stop having returned.
- **`resume(service)`** unfreezes the container and reopens dispatch once the cut is staged, so a
  writer is stopped for the staging and not for the persisting.

A service paused when the session closes is resumed before it is stopped ("Admission, the close and
the report"). The control is library-neutral and application-neutral: it names services and paths,
and knows nothing of what the application designated.

## Events

OR-A3-5 asks that a Rust service and an MCP service share attribution, events and secret rules,
and leaves whether they share code to OutRig. **Decided here: one call family, two lifecycles.**
An MCP call's events are `services.md`'s `service.call.received`, `service.call.dispatched`,
`service.call.cancel.sent` and `service.call.ended`, with `kind: "mcp"` and, on `received` and
`ended`, the MCP fields: `tool` (the exact name, which is the `method`), `listing` (the
generation), `state`, `failure` (the `McpFailureKind` name, or `null`), `is_error`, `blocks` (the
content kinds and their counts) and `structured` (whether one came). An owner reads every service
call with one subscription and `diagnostics.md`'s correlation fields once -- the family's own id
`call`, and `execid`, `invocation` and `agent` as claims -- and the alternative, a second
`mcp.call.*` family with the same fields, would be read twice; the fork is recorded with that
reason. A server's placement lifecycle, which a Rust service does not have, keeps the
`mcp.service.*` prefix. All are in the integration-audit category, under `diagnostics.md`'s rule
for a family; a call's events carry the agent's subject and a service's none.

Services (`0003-54`):

- `mcp.service.started` -- `service`, `server` (the declaration's own name, where it differs),
  `placement`, `declared_in` (as `McpDeclarationSource` names it,
  `crates/outrig/src/container/embedded.rs:58`, plus the owner), `tools`, `listing`, `attempts`,
  `duration`; followed by `service.bound`.
- `mcp.service.retry` -- a transient startup failure and the wait: `service`, `attempt`, `delay`,
  `cause` (class, kind, redacted message).
- `mcp.service.failed` -- `service`, `outcome` (`unavailable`, `refused`, `misconfigured`,
  `unclassified`, and `unapproved` from `0003-57` on), `cause`, `attempts`, `duration`.
- `mcp.service.paused`, `mcp.service.resumed`, `mcp.service.stopped` -- `service`, `how`,
  `proven`, and for a pause the calls left in flight.

Listings (`0003-55`): `mcp.service.relisted` -- `service`, `agent`, `listing`, `undecoded` (the
descriptors that did not decode, by name and reason); followed by `service.described` with the
changes; `mcp.service.list_changed` -- `service`.

Reconnect (`0003-56`): `mcp.service.disconnected` -- `service`, `cause`;
`mcp.service.reconnected` -- `service`, `attempts`, `listing`; followed by `service.described`.

Bodies are the service policy's: off by default, and when on, previews built from JSON by value and
cut to the bound children's previews use, never from a resolved header or environment, which no
event holds. That a call was not dispatched is read from `outcome` and `state`, never from a missing
`service.call.dispatched` across a gap, by `boundary-policy.md`'s rule for a reader.

## What each task builds

| Task      | Builds                                                                              |
| --------- | ----------------------------------------------------------------------------------- |
| `0003-44` | the service mechanism: trait, table, crossing, generated object, events, rows       |
|           | (`services.md`)                                                                     |
| `0003-54` | declarations on the builder and `outrig.Session`; command, sidecar and primary-view |
|           | placements under the harness, the config's entries included; startup with the      |
|           | classification, the budget and the outcomes; `McpService` bound per placed server; |
|           | `Secrets` for `env`; the credential rule; identity headers; the quiesce control;    |
|           | the service rows, the report's `mcp_services`, the lifecycle events; `run-new`      |
|           | prints outcomes                                                                     |
| `0003-55` | the `mcp` presenter kind: the description from `tools/list`, `tools` and `tool()`,  |
|           | content-block results, the result bound's config key, relisting through             |
|           | `service.described`, the tolerant listing, the listing events                       |
| `0003-56` | the three dispatch states split by position in a bounded call added beside          |
|           | `call_tool`, `McpError` and its subclasses, the cancel notice on the token,         |
|           | reconnect with relist, `uncertain()` and `mcp_uncertain`, the reconnect events      |
| `0003-57` | the URL placement: rmcp's streamable-HTTP client, a trust anchor, resolved and      |
|           | identity headers, HTTP failure classification, reconnect as a new HTTP session      |

## Rejected alternatives

**Rejected: a call mechanism of MCP's own beside the service mechanism.** An earlier draft of this
page carried one -- its own `mcp.call` line, its own table, its own events -- because the generic
trait was deferred. With `services.md` built first, a second mechanism would be the "second way of
presenting an object" `embedding.md` warned against, written twice. MCP keeps only what the generic
mechanism cannot know.

**Rejected: a pure-Python MCP client hosted as a binding.** `mcp-wrappers.md`'s finding: the
official Python SDK reaches `pydantic-core`, which the static CPython on either side of the
boundary cannot load (`discovery.md`, "Capability is a contract"). A client written without the
SDK would be a second MCP implementation beside a working one.

**Rejected: a mapping of MCP services of their own, `outrig.mcp`.** A service is a binding in the
agent's view (`services.md`, "The generated object"; `facilities.md`), and a second mapping would
make the model learn where each kind lives. `outrig.bindings[name]` is the one lookup.

**Rejected: the proxy's `<server>__<tool>` names as the Python names.** The sanitizer serves a
model calling tools in a 64-character namespace; a hash suffix and a widened collision are what
exact-name lookup makes unnecessary. `mcp_proxy` keeps them for `outrig mcp`.

**Rejected: rendering text at the boundary.** `render_text` as the result is what `outrig run`
gives a `String`-shaped tool channel; `mcp-wrappers.md` ("Results stay data") says why a Python
agent must not get it. `text` stays a derived view.

**Rejected: automatic relisting on `tools/list_changed`.** A listing that changes under a running
execution is the "silently" the requirement forbids. The notification is recorded and marks the
service stale; relisting is a reconnect's or the agent's act.

**Rejected: retrying a call whose annotations say it is read-only or idempotent.** The hints are
advisory and unverified; `mcp-wrappers.md` ("Timeouts and retries are declared, not assumed") puts
the property with the tool's declaration, and no declaration here carries one. The agent repeats a
call with the state in hand.

**Rejected: a startup failure aborting the session.** `outrig run` exits when any server fails
(`doc/concepts/mcp-servers.md`, "Lifecycle"). An owner that receives outcomes decides; a session
with one service down and one up is a session the application may still want.

**Rejected: a credentialed primary-view placement validated as safe.** OR-MCP4 asks to "define and
validate what that placement exposes before supplying secrets". `security.md`'s "Why a second
process in the same container is not enough" is the validation, and the answer is the rule above.

**Rejected: retyping `McpStartupFailure::source`.** The plan/next entry's first option; A-08 keeps
the payload of `OutrigError::McpStartupFailed` as it is, so the classification is added beside the
erased field.

## Open questions

- Resources and prompts: presented when a selected server needs them, and through which addition
  to the `mcp` presenter kind; `plan/next/rmcp-list-result-spec-gaps.md` names what their listing
  must carry.
- Whether a command placement's server can be found by pid inside the primary so a per-process
  freeze (`SIGSTOP` through `podman exec`) could make it a controlled writer. Until then it is
  uncontrolled, and the cut says so.
- Whether `mcp.service.list_changed` should wake a `runtime.wait`, as a channel arrival does. Not
  built: a listing change is not input.
- How a dict-constructed child's view lists its services and what its orientation says of them:
  `facilities.md`.
- Whether the config gains keys for the policies -- a connect budget for MCP and a result bound for
  the service policy, beside `mcp-call-timeout-secs` -- so `run-new` can set them, or `run-new`
  keeps the defaults. `0003-54` and `0003-55` carry it as a fork.

## Unverified

- That a `tools/list` page rmcp's typed decode refuses reaches `ServerResult::CustomResult` through
  `send_request_with_option`, rather than an earlier untagged variant matching it first. Read from
  rmcp 3.4.1's `model.rs` (`CustomResult` at `:893`; the union lists it last) and its comment on
  `CallToolResult`'s decoder, which exists so that other variants do not shadow `CustomResult`.
  `0003-55` confirms with a fake server before building on it.
- That `podman pause` freezes every process of a primary-view sidecar: the trust model doc says
  only the mount namespace is joined and "PID, network, and cgroup stay the sidecar's own", so the
  freezer should reach the payload and its descendants. Not tested.
- That rmcp's reqwest-backed streamable-HTTP client takes a `reqwest::Client` built with a custom
  root certificate (`StreamableHttpClientTransport::with_client`, rmcp 3.4.1
  `src/transport/streamable_http_client.rs:2028`) and carries `custom_headers` and `auth_header`
  from its config (`:2055-2057`) on every request, the SSE stream included. Read, not run.
- That a `${VAR}` value resolved by a `Secrets` resolver, rather than from the process environment,
  reaches podman by name through the hidden-environment path `Cmd::env_hidden` already provides
  (`mcp.rs`, the transcript test). Read, not run.
- Which rmcp HTTP transport errors carry an HTTP status, so that 401 and 403 can be classified as
  refused and 429 and 5xx as transient. `0003-57` reads `StreamableHttpError` and records the map.
- That the service table's schema validator compiles the `inputSchema` of the servers in
  `tests/fixtures/`; a schema it cannot compile is dispatched unchecked, so the cost of a miss is a
  precision the server supplies instead.
