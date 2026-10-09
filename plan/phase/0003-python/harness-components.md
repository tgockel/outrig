# Harness components

What owns what, once the agent loop lives in `outrig`. Read `crate-split-tradeoffs.md` for why
the split is shaped this way rather than another; this page is the shape itself.

The confusing part is that **two agent loops exist at once**. `outrig-cli` keeps the one it has
today, unmodified, so that merges from the 0.2.x line apply cleanly. `outrig` gets a second
one, copied and then changed. They do not share code and are not meant to converge during this
phase.

## The picture

```text
  HOST                                                    CONTAINER (primary)
  ------------------------------------------------        --------------------------------

  outrig-cli
  +--------------------------------------------+
  | cli/        clap args, subcommand dispatch |
  |   run       -> legacy loop, unchanged      |
  |   run-new   -> outrig's loop               |
  | repl.rs     stdin/stdout, /help, SIGINT    |
  | init/, image_setup/, config_init.rs        |
  |                                            |
  | llm.rs, llm/, rig_tool.rs, subagent/  <----+--- untouched; 0.2.x still edits these
  | builtin_tool.rs, self_tool.rs              |
  +--------------------------------------------+
         |
         | depends on
         v
  outrig
  +--------------------------------------------+
  | agent/      round loop, model resolution,  |
  |             retry, failover  (private)     |
  | agent/tool  the tool surface   (private)   |
  | python/     interpreter, console, payload  |
  |   interpreter.rs -- NDJSON, podman exec ---+------> interpreter.py
  |                                            |         +------------------------+
  | container/  lifecycle, exec, mounts        |         | interpreter process    |
  | mcp/        clients, proxy                 |         |  reader thread         |
  | config/     Agent, Model, LlmProvider      |         |  _PROTO_IN/_OUT, fd 1  |
  | network/    interceptor                    |         |  RLIMIT_DATA           |
  +--------------------------------------------+         |                        |
                                                         |  per agent, per thread:|
                                                         |   session module in    |
                                                         |    sys.modules         |
                                                         |   asyncio event loop   |
                                                         |   runtime.channels[..] |
                                                         |   output buffer + pipe |
                                                         |   execution slot       |
                                                         +------------------------+
                                                   /outrig/python (read-only mount)
                                                         +------------------------+
                                                         | static CPython + stdlib|
                                                         +------------------------+
```

## What moves, what is copied, what stays

**Moved** -- `python/`: the interpreter host, console, payload locator, prompt, and tool. New on
the prototype branch, so no 0.2.x commit can conflict with a file that line does not have.

**Copied** -- `llm.rs`, `llm/retry.rs`, and `llm/failover.rs`. The 0.2.x line actively edits
them, and copying is what keeps those merges clean. `0003-04` copied the part of `llm.rs` one
round needs; `0003-15` brought retry and failover. From `rig_tool.rs` only the result truncation
came across, and nothing from `session_tool.rs`. The model's one tool is `submit_python`, and
MCP servers would reach Python as the objects `mcp-wrappers.md` designs, after this phase
(`plan/next/rust-object-as-python-object.md`), rather than as rig tools, so the MCP adapter has
no caller on this side. An earlier draft had it coming along anyway.

**Neither** -- `llm/mistralrs.rs` and `llm/registry.rs`. The in-process backend was deprecated
in 0.2.0 and has since been removed from this line. The new loop never had it, so `mistralrs-core`,
`hf-hub`, and `candle-core` never became library dependencies.

**Designed, not yet ported** -- `builtin_tool.rs`, `self_tool.rs`, `subagent/`. These are
MCP-shaped, and their Python equivalents now have designs rather than a gap: `work.md` and
`typed-agents.md` for the subagent surface, queued as `0003-26` and `0003-27`, and
`discovery.md` for self-documentation. None of the three files is ported; `self_tool.rs` would
bring the 1,100-line `mcp_self/` corpus with it.

**Stays in `outrig-cli`** -- `repl.rs`. Terminal interaction is the CLI's job, and `run-new`
drives the same loop the existing command does.

**Stays for now** -- `session.rs` and `paths.rs`. Both encode host conventions, XDG directories
and session roots, that the library has deliberately avoided. Revisit if the library loop needs
a session record of its own.

## Consequences worth stating

**The library gains `rig-core` and a hard `reqwest`.** `outrig` depends on neither today.
Both become private dependencies: nothing rig-typed appears in the library's public surface,
which is what keeps a rig release from forcing an `outrig` major. `clap` and `dialoguer` stay
in `outrig-cli`; no terminal-interaction crate crosses over.

**The new modules are private, behind the session API.** Six of the modules being
copied are `pub` in `outrig-cli` only under the `internal-test-api` feature, which means their
current shape was never designed as an API. They stay private in `outrig`.

Private means crate-private, though, so `outrig-cli` cannot call them either -- something has
to be `pub` for `run-new` to reach the loop at all. For the first milestone that was
deliberately minimal: `PythonAgent`, which existed because Rust requires it rather than as an
interface anyone should build on. After the first milestone the maintainer decided the loop gets a
designed public surface after all. `embedding.md` describes the session API that an embedder
and `run-new` both use, `0003-19` builds it and removes `PythonAgent`, and the surface may change
freely until the 0.3.0 release fixes it in `public-api.txt`. The modules behind it stay private,
and no rig type crosses it.

**The error type does not carry rig's.** `outrig-cli`'s `CliError` has
`Prompt(rig::completion::PromptError)` and `LlmResolve(..)`. The library's equivalent converts
at the boundary instead. That conversion is the one unavoidable cost of hiding rig, and it is
paid once per error variant rather than at every call site.

**Two loops will drift.** That is accepted, not overlooked. A fix landing on the 0.2.x line
does not reach `outrig`'s copy unless someone carries it across. `crate-split-tradeoffs.md`
records what was weighed against that.

## The host side gains a process per binding

Hosted objects (`hosted-objects.md`) add processes on the host side of the picture. Each binding
a session declares is the same embedded static CPython the container mounts, run on the host as
a child of the owner -- one process per binding, in its own process group -- with the binding's
packages on its path. Agent code reaches it only through the interpreter: RPyC frames travel the
existing NDJSON protocol, and `python/host.rs` relays them between the interpreter and each
binding process. Nothing in the container listens. The new mounts are the binding's package
directory, read-only, and -- when any binding exists -- the workspace and every
`[[workspace.mounts]]` entry at their host paths.

## The container side

The prototype's files are named for the older vocabulary -- `kernel.py` and `kernel.rs` implement
what these documents now call the interpreter -- and the port renames them, since "kernel" is
taken by the per-agent environment they host. The program is
`crates/outrig/src/python/interpreter.py`.

Ported from the prototype rather than redesigned, with one change of shape. There is one interpreter
process per session, started through `podman exec -i` and spoken to in NDJSON -- and it hosts one
agent per thread rather than being one agent. Each agent owns a session module, an event loop, its
channel endpoints, a backlog of background output, and an execution slot; the process owns the
protocol descriptors, the reader thread, the memory ceiling, and the signal handler. The
primary agent runs on the main thread, because that is the only thread an interrupt can reach.
`agent-placement.md` decides all of this and records what it costs. The interpreter itself is a
static build mounted read-only, so the image needs nothing -- not a Python, not a shell, not a libc
of its own.

The boundary is worth naming precisely: the host writes requests to the process's stdin and
reads results from its stdout, and the interpreter moves both aside before any code runs. The
executed code's `sys.stdout` and `sys.stderr` write into a pipe per execution that the interpreter
drains; its fd 0 reads `/dev/null`, and its fds 1 and 2 are the exec's stderr, which the host reads
as diagnostics. Generated code therefore cannot write anything the host will read as a protocol
message, nor read anything the host meant for the interpreter.

What is *not* settled by the port is what happens when generated code misbehaves. Synchronous
code that never yields blocks the event loop of the agent running it and every path that could
report it; the interrupt mechanism and the liveness probe that recover from it are designed in
`runtime-protection.md`, and they come across with the port rather than after it. Co-hosting
bounds how far that reaches and also how far the cure reaches -- a wedged subagent costs its
siblings throughput rather than stopping them, and cannot itself be freed. Descendant processes
and output rate remain a later milestone; the memory ceiling does not, because without it
one agent's allocation ends the session.
