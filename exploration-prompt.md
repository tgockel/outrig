# Architecture exploration: a Python execution environment for the agent

## What you are being asked to do

Design **one** end-to-end architecture for giving an LLM agent a Python execution environment
inside OutRig, and argue for it. Several agents are working on this in parallel, each producing a
different design, so do not hedge toward a compromise -- take a position and defend it. The
designs will be compared against each other afterward.

This document exists so that you do not have to rediscover what is already known. Everything
below marked as measured was measured; where something is unverified it says so.

## The vision

The general idea is to give the LLM the ability to do deterministic tasks by giving it a Python
execution environment. Talking with external sources would eventually just be `await ...`, where
you are awaiting on any number of possible inputs.

A prototype exists in which a user's typed line is not sent to the model at all -- it is appended
to a Python global called `user_input`, and the model has to read it from the interpreter. That
is one input. The design should anticipate many: network responses, subagent results, timers,
tool completions. `await` is the intended shape for all of them.

## Why a scripting language, and why Python

These are the properties that matter. They are the actual argument; implementation difficulty is
secondary to them.

- **Dynamic typing.** When an agent makes a subagent, it needs to describe the shape of results
  it wants the subagent to send, which can be composed dynamically.
- **Reflection.** A type system mechanism to describe what is available to call. This does not
  need to be overly strict or statically validated -- runtime rejection is OK -- but there should
  be a consumable way for the model to see what an object has available to it. Python has this
  via the `help(thing)` interface and through things like `typing`.
- **Exceptions.** We need to be able to communicate that something arbitrary went wrong to the
  LLM. Return codes and `Result`-style interfaces do not really cut it here, since the LLM could
  write code that ignores these codes. Exceptions seem a natural way to escalate.
- **Async.** Right now `user_input` is the only external force, but it is reasonable to have an
  LLM instigate web connections and whatnot and wait for multiple results. Python's `asyncio`
  library has a lot of flexibility here -- one could argue it has too much.
- **LLM trained.** The LLM needs to have an understanding of the language it is writing. The more
  common, the better.

The list of scripting languages that have all of these is pretty limited. Python happens to have
them all, which makes it the presumptive answer. **If you propose something else, you must beat
that list**, not merely be easier to build.

The bar for "Python" is *regular old Python* -- the code a model has seen a million times, using
`dataclasses`, `pathlib`, `asyncio`, and `open()`. A dialect that looks like Python but breaks on
ordinary idioms does not clear the bar.

## What OutRig is, and what a new subsystem inherits

Read `doc/README.md` and `SECURITY.md` first. The load-bearing statements:

- The trade, `doc/README.md`: "outrig takes a different trade. Instead of approving each call,
  you set up a sandbox once: a Dockerfile, a config file, a network policy." The consequence, two
  lines later: "The blast radius of a wrong tool call is bounded by the container."
- The property, `SECURITY.md`: "OutRig's security property is the **container boundary**. MCP
  servers and the tools the agent runs execute inside a podman-managed container; the host stays
  outside that boundary except for the workspace mount(s) and the runtime services OutRig
  explicitly connects."
- What is *not* restricted, `SECURITY.md`: "The agent has arbitrary code execution _inside_ the
  container. That is the point -- there are no command allowlists within a normal OutRig
  container."
- The invariant most at risk from this work,
  `crates/outrig-cli/src/mcp_self/docs/concepts/mcp-trust-model.md`: "the agent cannot grow its
  own environment -- there is no agent-invocable tool that starts sidecars; new containers come
  from the config or the operator."

The operator owns the environment; inside it the agent is free. Any design that quietly moves
capability from the operator's control to the agent's is changing the premise, and has to say so.

### Hard constraints

**Works against any image.** Distroless, Alpine, an image with no `useradd` at all. The one
thing OutRig demands today is a `sleep` that accepts `infinity`. See
`doc/concepts/containers.md`.

**Nothing is installed at run time.** The image has what it has; OutRig does not fetch tools.
See `doc/concepts/containers.md`.

**Linux host, x86-64 or AArch64.** Anything that runs inside the container is a Linux binary
regardless of the host. See `README.md` and `doc/quickstart.md`.

**Degrade rather than fail**, naming exactly what is missing and the build-time reason for it.
An absent tool is legible; a wrong one is not. See `doc/reference/config.md`.

**Anything shipped into a container is ELF and must match the container's architecture.**
Nothing validates `e_machine` today and no podman invocation passes `--platform`, so the
image's architecture is whatever the registry served. See `plan/next/enter-arch-mismatch.md`.

One further rule, which is not in the docs: **a secret must never reach the model's context.**
Credentials living inside the container is not itself an exposure -- the agent already has
arbitrary execution there -- but no design may route one into the transcript.

### The existing precedent for shipping a binary in

OutRig already solves "get a binary into an image that contains nothing." `crates/outrig/build.rs`
compiles the `outrig-enter` launcher static for `<arch>-unknown-linux-musl`, `include_bytes!`
embeds it in the library, `crates/outrig/src/container/enter/mod.rs` writes it 0755 into the
session directory, and it is bind-mounted read-only into the container.

If your design ships an interpreter, say how it differs from this, because the difference is
where the cost lives. `outrig-enter` is one dependency-free source file compiled by a direct
`rustc` invocation. An interpreter is not.

## What the prototype settled, and what it did not

The prototype is `crates/outrig-harness/` (commit `573896e`); its README describes the shape.

**Settled: the interaction model works.** Message history is discarded at the end of every turn,
so the Python session is the only thing that persists. Told "remember the number 7" and then, in
a turn carrying no history whatsoever, "what number did I ask you to remember?", the model read
`user_input`, did not find the answer, listed `globals()` to see what its predecessor had left
behind, found the name it had bound, and answered correctly.

**Not settled: everything about where the interpreter runs.** These were measured, so treat them
as given:

- RustPython's VM is `!Send` and `!Sync` three times over -- `PyObjectRef`'s `Send`/`Sync` impls
  are gated on the `threading` feature, `PyRc` is a plain `Rc` without it, and the type zoo lives
  in a `thread_local!`. It is therefore pinned to one OS thread, and needs a 32 MiB stack, because
  the 2 MiB default overflows on startup.
- The `host_env` feature gates `posix`, `socket`, `subprocess`, and `select` in
  `rustpython-stdlib`. It is a single switch with no middle setting. With it off,
  `dataclasses`, `random`, `io`, `pathlib`, `csv`, and `importlib` all fail, because each imports
  one of those transitively. Losing `subprocess` is the sandbox working as intended; losing
  `dataclasses` is collateral, and it is why the prototype is not a direction.
- It cannot simply be switched on where the interpreter currently lives, because "host" there
  means the machine OutRig runs on -- outside the container boundary.
- The stdlib *sources* for `asyncio`, `socket`, `ssl`, `subprocess`, and `dataclasses` **are**
  shipped in `rustpython-pylib`. Whether they actually work with `host_env` on is **unverified**.
  Somebody should establish this empirically rather than assume it either way; it is close to
  decisive for any RustPython-based design.
- A static musl build of `rustpython-stdlib` **fails today**: it depends unconditionally on
  `liblzma-sys`, a C library, so it needs a musl C cross-toolchain. This is the concrete obstacle
  between "ship RustPython into the container" and a working build.
- A reasoning model given no output-token ceiling can spend its entire budget thinking and return
  an empty response. Any design where the model drives a loop by emitting code will meet this; see
  `plan/next/openai-arm-sends-no-ceiling.md`, which already describes it.

## Scope

**In scope.** Where the interpreter runs and how it gets there. What the agent's Python can
reach. What `await` is awaiting on, and what wakes it. How the model discovers what is callable.
What happens when the interpreter dies, wedges, or runs too long.

**Out of scope, but do not foreclose:**

- **C extensions and `pip`.** The embedded interpreter is an orchestration layer, not a data
  science stack. Something that needs CPython-dependent libraries can write a Python file and run
  it with a CPython that is installed in the container. Aim for a robust interface, not a
  strictly complete one.
- **Credential design.** Say only whether your design makes the problem harder.
- **Exposing MCP servers as Python objects** -- an MCP is an object, its tools are methods on
  that object. This is a later task and may be the answer to credential hiding, so do not design
  something that prevents it.

**Changing under you.** MCP tool-calling is largely replaced in the first pass: in testing, LLMs
are much better at using Python libraries than MCPs, so tools living in sidecars take a back
seat. Note that this contradicts what `doc/concepts/mcp-servers.md` and the trust model currently
describe as *the* way an agent acts. Naming that contradiction is part of the job.

## Questions your design must answer

1. Where does the interpreter run, and how does it get into an image that may contain nothing?
2. Is "regular old Python" reachable without CPython ABI compatibility? If your design says yes,
   what evidence supports it?
3. What is the unit of execution -- a REPL call, a long-running program, a coroutine the harness
   drives? What happens to the agent turn while Python is running?
4. What is `await` actually awaiting on, and what wakes it? How does a network response, a
   subagent result, and a typed line all become the same kind of event?
5. How does the model see what is available to call? Reflection is a stated requirement, not a
   nicety.
6. What happens when Python blocks forever, crashes, or exhausts memory? What does the operator
   see?
7. What does this do to the container boundary, and to "the agent cannot grow its own
   environment"?

## What to return

- The design, end to end.
- What it makes easy, and what it makes impossible.
- Build and runtime cost: binary size, what it does to the build, what it demands of CI.
- How it fails, and what that failure looks like to the operator.
- **The single fact which, if true, would kill this design** -- and whether anyone has checked.
