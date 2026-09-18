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
  | agent/      turn loop, model resolution,   |
  |             retry, failover  (private)     |
  | agent/tool  the tool surface   (private)   |
  | python/     kernel host, console, payload  |
  |   kernel.rs ---- NDJSON over podman exec --+------> kernel.py
  |                                            |         +------------------------+
  | container/  lifecycle, exec, mounts        |         | session module in      |
  | mcp/        clients, proxy                 |         | sys.modules            |
  | config/     Agent, Model, LlmProvider      |         | asyncio event loop     |
  | network/    interceptor                    |         | runtime.channels[user] |
  +--------------------------------------------+         | foreground execution   |
                                                         +------------------------+
                                                   /outrig/python (read-only mount)
                                                         +------------------------+
                                                         | static CPython + stdlib|
                                                         +------------------------+
```

## What moves, what is copied, what stays

**Moved** -- `python/`: the kernel host, console, payload locator, prompt, and tool. New on
the prototype branch, so no 0.2.x commit can conflict with a file that line does not have.

**Copied** -- `llm.rs`, `llm/retry.rs`, `llm/failover.rs`, `rig_tool.rs`, `session_tool.rs`.
The 0.2.x line actively edits these, and a copy is what keeps those merges clean. The MCP
adapter comes along even though the first milestone hands the model no MCP tools, because it
is wanted once they are reachable from Python.

**Neither** -- `llm/mistralrs.rs` and `llm/registry.rs`. The in-process backend is deprecated
and its removal is `plan/next/remove-deprecated-local-llm.md`. The new loop omits it, which
costs nothing now and avoids `mistralrs-core`, `hf-hub`, and `candle-core` becoming library
dependencies.

**Not yet** -- `builtin_tool.rs`, `self_tool.rs`, `subagent/`. Subagent and self-documentation
tools are MCP-shaped and have no Python equivalent designed; `self_tool.rs` would bring the
1,100-line `mcp_self/` corpus with it.

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

**The new modules are private, with one entry point that is not.** Six of the modules being
copied are `pub` in `outrig-cli` only under the `internal-test-api` feature, which means their
current shape was never designed as an API. They stay private in `outrig`.

Private means crate-private, though, so `outrig-cli` cannot call them either -- something has
to be `pub` for `run-new` to reach the loop at all. That something is deliberately minimal:
whatever starting a session and driving a turn requires, in terms that name no rig type, and
nothing beyond it. It exists because Rust requires it, not because it is an interface anyone
should build on, and it will move. A consumer-facing API is later work and there is nothing yet
to design one against.

**The error type does not carry rig's.** `outrig-cli`'s `CliError` has
`Prompt(rig::completion::PromptError)` and `LlmResolve(..)`. The library's equivalent converts
at the boundary instead. That conversion is the one unavoidable cost of hiding rig, and it is
paid once per error variant rather than at every call site.

**Two loops will drift.** That is accepted, not overlooked. A fix landing on the 0.2.x line
does not reach `outrig`'s copy unless someone carries it across. `crate-split-tradeoffs.md`
records what was weighed against that.

## The container side

Ported from the prototype rather than redesigned. One interpreter process per
session, started through `podman exec -i` and spoken to in NDJSON. It owns the agent's global
namespace, its asyncio event loop, its foreground execution, and its channel endpoints. The
interpreter itself is a static build mounted read-only, so the image needs nothing -- not a
Python, not a shell, not a libc of its own.

The boundary is worth naming precisely: the host writes requests to the process's stdin and
reads results from its stdout, while the executed code's own stdout and stderr are redirected
to a pipe the kernel drains. Generated code therefore cannot write anything the host will read
as a protocol message.

What is *not* settled by the port is what happens when generated code misbehaves. Synchronous
code that never yields blocks the interpreter's event loop and every path that could report it;
the interrupt mechanism and the liveness probe that recover from it are designed in
`runtime-protection.md`, and they come across with the port rather than after it. Resource
limits, descendant processes, and output rate do not, and are a later milestone.
