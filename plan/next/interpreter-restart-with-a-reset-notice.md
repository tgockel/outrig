# Continue a session in a fresh interpreter after the old one dies

## Context

When the interpreter process exits -- `os._exit`, a crash in native code -- the execution that was
running reports `unknown` (`Unknown::Exited`), and the model is told the interpreter is gone and
later calls will fail (`crates/outrig/src/agent/tool.rs`). Phase 0003 keeps that: interpreter
death ends the session (`plan/phase/0003-python/lifecycle.md`).

The phase 0003 design brief (now in `plan/phase/0003-python/lifecycle.md`) records the maintainer's
rule for continuing instead: a fresh interpreter, an explicit notice that state was reset, and never
a replay of executed source, because running code again repeats whatever effects it already had. The
maintainer deferred it past 0.3.

## Shape

- Start a new interpreter in the same container. A container that is gone still ends the session.
- Present what the session supplies again: `runtime`, the user channel, the store's turns as
  `runtime.history`, bindings and skills. Binding processes run on the host and outlive the
  interpreter. The dead interpreter's connections are closed, which releases every object they
  held, and the new kernels open their own.
- Report what was in flight as ended, in events: the running execution with its outcome
  `unknown`, child kernels and their work handles terminated (`0003-25`), pending escalations
  cancelled.
- Put a notice in the next model call: the interpreter was replaced, which names were lost, which
  work was reported terminated, and that nothing was run again. The host keeps no copy of the
  namespace, so the names can only come from the last inventory it received. Keeping that current,
  for example by asking after each execution, is part of this work.
- Never replay. No earlier source runs again, imports included; rebuilding state is the agent's
  decision, made in code it writes.
- Bound the restarts. An interpreter that dies at start, or dies repeatedly, ends the session as
  it does today.

## Open questions

- Whether the restart is automatic or an owner call the embedder makes
  (`plan/phase/0003-python/embedding.md`).
- Whether binding processes restart too, so that no host state the dead interpreter created
  survives into the new one.
- Messages that waited unread in the dead interpreter: keep copies on the host to deliver again,
  or report how many were lost.

## Acceptance

- After `os._exit(0)` in an execution, that call reports `unknown`, the next model call carries
  the notice, the agent's names are gone, and `runtime`, bindings and skills work.
- A file that earlier code appended to is unchanged by the restart.
- A child's work handle held at the death is reported terminated in events and the shutdown
  report.
- An interpreter that dies at start ends the session with its cause, not a restart loop.
