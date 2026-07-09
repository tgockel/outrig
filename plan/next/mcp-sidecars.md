# MCP sidecar containers

## Context

Today every MCP server in a session runs inside the primary workspace container: the merged
`[images.<name>.mcp]` map is connected by `connect_mcp_clients`, which spawns each server as a
`podman exec -i` child of that one container. This has two costs. First, every server's runtime
dependencies must be baked into the workspace image, even when they have nothing to do with the
project toolchain. Second, there is no isolation between servers and the workspace: a server sees
everything the agent's shell sees.

Because the agent loop runs on the host, nothing about the current stdio transport requires the
server to live in the *primary* container. A second container works just as well as an exec
target, which opens the door to off-the-shelf MCP images (the Docker MCP catalog pattern) and to
least-privilege placement: a server gets a container, a workspace view, and mounts scoped to what
it actually needs.

This spec introduces **sidecars**: additional podman containers owned by a session, hosting MCP
servers, with lifecycle coupled to the primary container, full `[network]` policy parity, and
dynamic addition mid-session. It is a multi-task program; the breakdown at the end is ordered so
that `/groom-plan` can fold it into the numbered queue.

## Goal

Run MCP servers in sidecar containers alongside the primary session container, such that killing
the primary kills the sidecars, session network policy applies to every container, and new
sidecars can be added mid-session from the library API and the REPL.

## Non-goals (v0)

- Streamable-HTTP or any network transport between host and sidecar; stdio only.
- Container-to-container networking (sidecar <-> primary traffic).
- Automatic restart of dead sidecars or servers.
- `/sidecar stop` and removal surfaces in the REPL.
- An agent-invocable tool for adding sidecars (the agent must not grow its own environment).
- Lazy-start of declared sidecars on first tool use.

## Config surface

A sidecar is declared as a named block under an image config, and MCP entries opt into placement.
All new keys follow the existing kebab-case, `deny_unknown_fields` conventions in
`crates/outrig/src/config/mod.rs`.

```toml
[images.dev]
dockerfile = "Containerfile"
context    = "."

[images.dev.sidecars.tools]
image      = "mcp-tools"        # sibling [images.mcp-tools] block first, else raw podman ref
workspace  = "ro"               # "none" (default) | "ro" | "rw"
start      = "auto"             # "auto" (default)  | "manual"
on-failure = "abort"            # "abort" (default) | "warn"

[[images.dev.sidecars.tools.mounts]]
host-path      = "~/.cache/example"
container-path = "/cache"
access         = "read-write"   # "read-only" (default) | "read-write"

[images.dev.mcp]
# Runs in the primary container, exactly as today.
local = ["mcp-local", "--stdio"]
# exec-stdio in the named sidecar "tools".
fs    = { command = ["mcp-fs", "/workspace"], sidecar = "tools" }
# exec-stdio in a dedicated anonymous sidecar (inline shorthand).
grep  = { command = ["mcp-grep"], image = "ghcr.io/example/mcp-grep:1" }
# entrypoint-stdio: no command; the image ENTRYPOINT is the server.
fetch = { image = "ghcr.io/example/mcp-fetch:2", env = { TOKEN = "${FETCH_TOKEN}" } }
```

Key semantics:

- `image` (sidecar block and inline) resolves exactly like `--image`: an `[images.<name>]` config
  name first (Dockerfile-built sidecars get content-hash caching for free), then a raw podman
  ref, which must be present locally (`--pull=never` semantics, as today).
- `workspace` mounts the session workspace at the primary's `container-path`. Default `none`:
  a sidecar sees nothing unless it asks.
- `mounts` reuses the existing `MountConfig` shape (`host-path`, `container-path`, `access`).
- `start = "manual"` declares a sidecar that does not start with the session; it is started by
  `/sidecar add <name>` or the library API.
- `on-failure` governs session-start and dynamic-add failures for that sidecar (see Failure
  semantics). It does not restart anything mid-session.
- An optional `[images.<name>.sidecars.<sc>.security]` block reuses `ContainerSecurity`
  (`capability-profile`, `cap-drop`, `cap-add`) with the same defaults as the primary.
- `McpServerSpec` gains two optional keys on its `Full` form: `sidecar = "<sc>"` (exec-stdio in
  a named sidecar) and `image = "<ref>"` (a dedicated anonymous sidecar for this one server).
  With `command` present, an inline `image` means exec-stdio in the anonymous sidecar; with
  `command` absent, the image's ENTRYPOINT is the server (entrypoint-stdio). The `Short` form
  (bare argv array) always runs in the primary.

Validation rules (config-load errors unless noted):

- `sidecar` and `image` are mutually exclusive on an MCP entry.
- `sidecar = "<sc>"` must name a sidecar declared in the same image block.
- `sidecar` without `command` is an error: named sidecars are exec-stdio hosts only;
  entrypoint-stdio is expressible only through the inline `image` form.
- An entry with `image` and no `command` may carry `env` but nothing else.
- Sidecar names match `[A-Za-z0-9][A-Za-z0-9_-]*` (they embed in container names). Anonymous
  sidecars occupy their server's name in the same namespace; a named sidecar colliding with an
  anonymous one is an error.
- The inline shorthand carries only `image`; anything fancier (workspace access, mounts,
  `on-failure`, security) requires promoting to a named block. Anonymous sidecars take all
  defaults: `workspace = "none"`, no mounts, `on-failure = "abort"`, `start = "auto"`.

## Runtime behavior

**Startup order.** Session setup starts the primary container and bootstraps its user as today,
then starts every `start = "auto"` sidecar, attaches the network interceptor to each container
(see Network parity), and finally connects MCP clients -- primary and sidecar servers through the
same `connect_mcp_clients` funnel, in sorted order. Both `outrig run` and `outrig mcp` get
sidecar-hosted servers, since they share that funnel.

**Naming and labels.** Sidecar containers are named `outrig-<sid>-<sc>` (anonymous: the server
name as `<sc>`). Every session container -- primary included -- gains podman labels
`org.outrig.session=<session-id>` and, on sidecars, `org.outrig.sidecar=<sc>`. These are new
surface in `build_podman_run_cmd`; today no `--label` is passed at all. Session records list
sidecar container names alongside `container_name`.

**UID mapping and bootstrap.** Every sidecar runs with `--userns=keep-id`, like the primary. The
in-container user bootstrap (`bootstrap_user`) runs only when identity matters: the sidecar hosts
at least one exec-stdio server (exec needs `--user` and `HOME`), or has `workspace != "none"`, or
declares mounts. A mount-less entrypoint-stdio sidecar runs with the image's own `USER` untouched.
Exec servers use the workspace path as working directory when mounted; otherwise the image
default.

**Lifecycle coupling.** The three-layer cleanup generalizes to N containers: all session
containers join the `TRACKED` registry before spawn, `stop()` and `Drop` walk sidecars before the
primary, and the panic hook sweeps the whole set. In addition, a watcher task runs
`podman wait <primary>` for the session's lifetime; if the primary dies out from under outrig
(manual `podman kill`, OOM), the watcher reaps all sidecars and ends the session with an error.
Orderly teardown cancels the watcher first. A companion `podman wait` per sidecar logs sidecar
death promptly. Neither podman pods nor `--requires` nor shared PID namespaces are used; coupling
is entirely outrig-managed.

**Teardown ordering** extends the existing load-bearing sequence: MCP client shutdown (primary
and sidecar servers) -> network interceptor detach for every container -> sidecar stop ->
primary stop -> session-store finalize.

**`outrig clean`.** In addition to the session-record walk, clean sweeps
`podman ps -a --filter label=org.outrig.session` so strays survive even a lost session record.
Records for live sessions are skipped as today.

**Failure semantics.** At session start (and dynamic add), a sidecar that fails to start,
bootstrap, or connect any of its servers is handled per its `on-failure`: `abort` (default)
tears the session down with today's fail-fast error enrichment; `warn` logs to stderr, skips the
sidecar and all servers it hosts, and continues with a reduced tool set. Primary-container and
primary-hosted-server failures remain unconditionally fail-fast. Mid-session sidecar death is
uniform regardless of `on-failure`: the watcher logs it, its servers' tools return errors, and
nothing restarts. Dynamic-add failures are reported to the caller only; the session stays
healthy.

## Transports

**exec-stdio (first).** The sidecar container runs `sleep infinity`; each server is spawned with
the existing `McpClient::connect_via_podman_exec` against the sidecar's container name. Env
resolution (`EnvValue`, `${VAR}` from the host), per-server stderr files under the session log
dir, and CLI `--env` scoping via `for_server` all apply unchanged.

**entrypoint-stdio (second).** `{ image = "...", env = {...} }` with no command runs
`podman run -i` with piped stdio; the image's ENTRYPOINT is the MCP server and container lifetime
equals server lifetime. `env` becomes `--env` flags on the run. Requirement: network interception
must be attached before the entrypoint can emit a packet. Candidate mechanism (validate during
implementation): `podman create` + `podman init` to materialize the container process and netns
without executing the entrypoint, attach the interceptor to the created PID, then
`podman start --attach --interactive` for the stdio pipes. Server exit surfaces exactly like a
mid-session sidecar death.

streamable-HTTP is explicitly out of scope for v0.

## MCP merge and naming

Named sidecars honor `org.outrig.mcp` image labels with today's semantics, scoped to that
sidecar: label-declared servers materialize as exec-stdio servers *in that sidecar*, and repo
config overrides by server name (config replaces the whole entry, including placement).
Anonymous sidecars are different: the label is inert there; an inline sidecar runs exactly the
one server that declared it.

The server-name namespace stays flat per session. After per-sidecar label materialization and
config override-by-name, any remaining duplicate name -- e.g. two sidecars' images both advertise
`fs` and neither is overridden -- is a startup error, extending the duplicate checks already in
`ProxyServer::build`. `outrig mcp show-merged` grows a placement column alongside the existing
`McpDeclarationSource` provenance, so users can see which container hosts each server and why.

## Network parity

`NetworkInterceptor` generalizes from one container to N *before* sidecars become usable, so
there is never a state where `[network]` audit or filter silently excludes sidecar egress. The
generalization: one interceptor per session owning the compiled policy and the shared
`AuditSink`, with a per-container attach/detach operation. Each attachment repeats today's
single-container mechanics in that container's namespaces -- fork + `setns` into the container's
user and net namespaces, bind the TCP and DNS sockets there, pass fds back via `SCM_RIGHTS`,
apply the per-session nft NAT table via `nsenter` (netns isolation means the table name does not
collide across containers), and install the audit `resolv.conf`. Audit records already carry a
container field; it is stamped per attachment instead of once per session. Detach tears down one
container's rules and sockets without disturbing the others; dynamic add attaches mid-session.

## Dynamic addition

**Library.** `Outrig` grows from exactly one container to a container set:
`Outrig::add_sidecar(SidecarSpec) -> Result<...>` starts the container (labels, keep-id,
conditional bootstrap), attaches the network interceptor, connects the spec's servers, and
extends `tools()`. `SidecarSpec` is a builder in the `LaunchSpec` style: image (config name or
raw ref), workspace access, mounts, security, and servers (name -> command/env). Arbitrary specs
are allowed here -- the API is the primitive everything else builds on. `LaunchSpec` gains
`with_sidecar(SidecarSpec)` for launch-time declaration by library users, mirroring the config
surface.

**REPL.** Two subcommands join the fixed command match in `repl.rs` (plus `HELP_TEXT` and a
callback wired in `run.rs`):

- `/sidecar add <name>` -- starts a config-declared `start = "manual"` sidecar. Declared-only by
  design; unknown names and already-running sidecars are errors. New tools become available to
  the agent on the next turn.
- `/sidecar list` -- shows each declared sidecar with status (`running`, `not started`,
  `exited`) and the servers it hosts.

## Task breakdown

Ordered for `/groom-plan`; each item is one numbered task on its own branch.

1. **Interceptor multi-container generalization.** Attach/detach API, per-container namespace
   sockets and nft application, per-record container stamping in the audit sink, teardown of all
   attachments. No behavior change for single-container sessions. Acceptance: two containers
   under one session policy produce correctly attributed audit records in both audit and filter
   modes; teardown leaves no nft tables in either netns. Depends on: nothing new (builds on the
   landed interceptor).
2. **Sidecar core + exec-stdio.** Config surface and validation, container labels, lifecycle
   (start, bootstrap conditions, watcher, N-container cleanup and teardown ordering), session
   records and label-sweeping `outrig clean`, scoped label merge and collision rules,
   `show-merged` placement, exec-stdio connection, `on-failure` semantics, doc updates.
   Acceptance: a declared sidecar hosts servers visible to `outrig run` and `outrig mcp`;
   `podman kill` of the primary reaps sidecars; `outrig clean` sweeps labeled strays; `abort`
   and `warn` behave as specified; sidecar egress obeys session network policy. Depends on: 1.
3. **entrypoint-stdio transport.** The no-command inline form, interception attached before the
   entrypoint runs, lifetime-equals-server semantics, env injection. Acceptance: an off-the-shelf
   MCP image runs as a server with policy applied from its first packet; its exit surfaces as
   tool errors without killing the session. Depends on: 2.
4. **Dynamic addition.** `Outrig::add_sidecar`, `SidecarSpec`, `LaunchSpec::with_sidecar`,
   `/sidecar add` and `/sidecar list`, agent toolset refresh on the following turn, error-to-
   caller failure handling. Acceptance: a manual sidecar added mid-session serves tools with
   network policy attached; a failed add leaves the session fully usable. Depends on: 2.

Documentation lands with each task in the pages under `crates/outrig-cli/src/mcp_self/docs/`
(symlinked into `doc/`): `concepts/containers.md`, `concepts/mcp-servers.md`,
`concepts/workspace.md`, `concepts/mcp-trust-model.md`, `usage/mcp.md`, `reference/config.md`.

## Acceptance

- Killing the primary container kills every sidecar, through orderly teardown, Drop, panic, and
  external kill (watcher) paths alike.
- Session `[network]` policy applies to every container from the first sidecar-capable release;
  there is no mode in which sidecar egress bypasses audit or filter.
- All four config shapes work: primary (unchanged), named sidecar via `sidecar = "<sc>"`,
  anonymous exec-stdio via inline `image` + `command`, entrypoint-stdio via inline `image` only.
- Sidecars can be added mid-session from the REPL (declared) and the library API (arbitrary).
- `outrig mcp show-merged` shows placement and provenance for every server; name collisions
  fail at startup with an actionable error.
- Existing single-container sessions behave byte-for-byte as before when no sidecar is declared.

## Dependencies

None on queued work; the numbered queue is empty. Builds on the landed network interceptor
(tasks 0059/0060 lineage) and the existing MCP merge/exec machinery. The MITM follow-up
(`network-interceptor-mitm.md`) is orthogonal but its interceptor changes will want to land
against the multi-container shape from task 1.

## Decisions

Design calls made with the user during specification (2026-07-09):

1.  Transports: exec-stdio first, entrypoint-stdio second, as separate tasks; no HTTP in v0.
2.  Granularity: both named sidecar blocks (N servers) and an inline one-server shorthand.
3.  Lifecycle coupling: outrig-managed cleanup generalized to N containers plus a `podman wait`
    watcher; pods, `--requires`, and shared PID namespaces rejected.
4.  Dynamic-add surfaces in v0: library API and REPL command only.
5.  Sidecar images: a single `image` key with `--image`-identical resolution.
6.  `org.outrig.mcp` labels on named sidecars: honored, config overrides by name, provenance in
    `show-merged`; server names unique across primary and all sidecars (startup error).
7.  Workspace access: none by default; `workspace = "none"|"ro"|"rw"` plus explicit mounts.
8.  Network: full audit/filter parity in v0 (no documented-gap or hard-error interim).
9.  UID mapping: `--userns=keep-id` always; bootstrap only where identity matters.
10. Failures: per-sidecar `on-failure = "abort"|"warn"`, default `abort`; no auto-restart.
11. Dynamic scope: REPL starts declared `start = "manual"` sidecars; API accepts arbitrary specs.
12. Sequencing: interceptor generalization lands before sidecars are usable.
13. Inline shorthand carries `image` only; everything else requires a named block.
14. Labels are inert on anonymous sidecars: exactly the one declaring server runs there.
15. REPL surface: `/sidecar add <name>` and `/sidecar list`; no `/sidecar stop` in v0.

Drafting-level defaults chosen while writing this spec (not separately user-confirmed): sidecar
container naming `outrig-<sid>-<sc>`; label keys `org.outrig.session` / `org.outrig.sidecar`;
bootstrap also triggered by extra mounts (not just workspace); `security` sub-block reuse on
named sidecars; entrypoint-stdio restricted to the inline form; the create/init/attach/start
mechanism for closing the entrypoint interception race.

## See also

- `network-interceptor-mitm.md` -- the other queued interceptor follow-up.
- `doc/concepts/mcp-servers.md` -- current MCP declaration and lifecycle narrative.
- `doc/concepts/mcp-trust-model.md` -- trust boundary rationale sidecars must preserve.
- `doc/concepts/workspace.md` -- mounts, UID mapping, and the network-policy narrative.
- `doc/reference/config.md` -- authoritative config key reference to extend.
