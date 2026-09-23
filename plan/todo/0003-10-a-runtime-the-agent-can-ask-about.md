# 0003-10 -- The agent can ask what it holds and what it cannot import

## Context

A persistent namespace only helps if the agent can find out what is in it. `discovery.md` splits
that into three questions -- what do I have, what is this thing, what can this interpreter do --
and settles the constraint that shapes all three: **automatic observation must never execute code
the agent wrote.**

That constraint has a boundary rather than being absolute, and the boundary is worth implementing
deliberately. The echo on a successful execution calls `repr()`, which *is* agent code. It runs
inside the agent's own execution, so a looping `__repr__` wedges that execution and the interrupt
path already applies. The inventory runs outside any execution, at the host's request, and must
therefore stay to names and types.

The third question has the worst failure mode. The interpreter cannot load third-party extension
modules -- not because they are unavailable but because there is no mechanism -- and an agent must
not learn this by repeated failed `pip install` attempts, which look to a model like a permissions
problem it might route around.

## Goal

An agent can discover an unfamiliar method rather than guess it, list what it holds, and get a
straight answer about what this interpreter cannot run.

## Deliverables

- The bounded inventory, already in the protocol, plus **a way to find what it omitted**: a count
  and a means of asking for the rest, since a cap of two hundred names silently becomes a lie at
  two hundred and one.
- Ordinary Python as the answer to "what is this thing": `help()`, `inspect`, `__doc__`. Bounded
  output, and asking is an explicit act rather than a preamble entry.
- An import and capability manifest, so "can I use `numpy`" has an answer before the attempt.
- **A diagnostic on a failed import of a compiled module** that names the reason and the route --
  an image-provided Python, a subprocess, a service -- rather than surfacing a bare
  `ModuleNotFoundError`.
- The preamble, per the rule: what an agent cannot learn by looking *and* needs every round.
  Names persist across rounds; observations are bounded while the values behind them are not;
  waiting watches channels; this interpreter cannot install packages.
- **The echo/inventory boundary implemented as designed**: the echo renders inside the execution,
  the inventory never calls `repr`, a property, a descriptor, or an iterator.

## Acceptance

- A value with a looping `__repr__` as the trailing expression wedges only that execution, and the
  interrupt path recovers it. The inventory taken afterwards still answers.
- **An inventory of an over-cap namespace reports the count and offers the rest**, rather than
  silently truncating.
- `help()` on a runtime object returns a signature and docstring, bounded.
- Importing a compiled third-party module produces the actionable diagnostic, not a bare
  `ModuleNotFoundError`. A pure-Python import from the workspace succeeds -- worth testing
  explicitly, because it is the case an agent reaches for first.
- Processing a value larger than the context window works: the value stays whole in Python and
  only the observation is clipped.
- `cargo test --workspace`, `cargo clippy --all-targets`, `cargo fmt --check` pass.

## Design forks

1. **Whether proxies need their own description path -- defer.** A Pyro proxy's signature is not
   locally knowable, so `help()` on one either lies or round-trips. Nothing exposes proxies until
   the credential work, which is unqueued.
2. **Preamble versus module docstring for the runtime guide -- Open.** The docstring is cheaper
   and only works if the model thinks to look.

## Dependencies

- **Hard: 0003-05.** There is no preamble to put anything in until the command exists.
- **Hard: 0003-06.** The looping-`__repr__` criterion below asserts the interrupt path recovers
  the execution, which does not exist until then -- numeric order places this task later anyway,
  but the dependency is the reason rather than a coincidence.
- **Soft: 0003-09**, whose channel-watching sentence is one of the preamble's entries.

## See also

- `plan/phase/0003-python/discovery.md` -- the three questions, the safe-observation constraint,
  and the preamble rule.
- `plan/phase/0003-python/mcp-wrappers.md` -- what the extension-module limit already cost, which
  is the concrete example the diagnostic should not make an agent rediscover.
