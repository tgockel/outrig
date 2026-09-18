# Discovery

A persistent namespace only helps if the agent can find out what is in it. The phase already
promises "bounded observations everywhere output can reach a model" -- execution output, value
previews, the variable inventory, tracebacks. That is the safety half. This page is the other
half: how an agent learns what it has, what a thing does, and what this interpreter can and cannot
run.

The governing constraint comes first because it shapes everything else: **automatic observation
must never execute code the agent wrote.** The inventory the prototype built reports names and
type names only, and nothing in it calls `repr()`, a property, a descriptor, or an iterator. That
is not a limitation to grow out of. It is the reason an inventory cannot be wedged by a hostile or
merely buggy `__repr__`, and every richer rendering added here is explicit, bounded, and
cycle-aware rather than automatic.

## Three questions an agent asks

**What do I have?** The bounded inventory: names and type names, capped, with the boot-time
infrastructure names hidden so the listing is the agent's own work. What it lacks today is a way
to find what it omitted -- a cap of two hundred names silently becomes a lie at two hundred and
one. A count and a way to ask for the rest is the smallest fix.

**What is this thing?** Signature, docstring, whether it must be awaited, what it returns, and
what it is likely to raise. Ordinary Python answers most of this already: `help()`, `inspect`, and
`__doc__` are things a model knows without being taught, which is the strongest argument for not
inventing an API. What is worth adding is that the answer is bounded and that asking is an
explicit act -- an agent calls `help(x)` and pays for the output, rather than having every object
described in its preamble.

**What can this interpreter do?** See below; this is the question with the least obvious answer
and the worst failure mode.

## What goes in the preamble

The temptation with a discoverable runtime is to describe it all up front. That is how MCP tool
schemas work and it is one of the things this phase is moving away from: the cost is paid on every
round whether or not anything is used. But "discover it" is the wrong default for a small number
of things, and the line is sharper than progressive disclosure alone.

**The preamble carries what an agent cannot learn by looking, and will need every round.** Both
halves matter. Something surprising but rarely used can wait for `help()`. Something common but
obvious does not need saying. It is the intersection that has to be given away.

`runtime.wait` is the clearest case and worth naming as the worked example. Its signature is
deliberately identical to `asyncio.wait`'s, which is what makes it legible -- and it means the
signature discloses nothing about the behavior that is not asyncio's. Nothing in
`runtime.wait(fs, timeout=60)` suggests it also watches the agent's channels and raises when input
arrives. A model reading only the signature would be right to expect otherwise, and would be
wrong. That is exactly "cannot be learned by looking", and waiting is in almost every non-trivial
round, so it is in the preamble.

The same test admits a short list and rejects a long one: that names persist across rounds, that
observations are bounded while the values behind them are not, that waiting watches channels, and
that this interpreter cannot install packages -- the last because `discovery` exists partly so an
agent does not learn it by failing. What it excludes is every individual capability, every tool
schema, and every object an agent might eventually touch. Those are `help()`.

A concise orientation plus `help()` beats an exhaustive preamble, and it degrades better: a model
that does not know something can find out, and one that does is not charged for the reminder.

## Capability is a contract, not a discovery

The interpreter is a static CPython mounted read-only, which is what makes the feature work in an
image that contains no Python at all. The same property means it **cannot load third-party
extension modules** -- not because they are unavailable but because there is no mechanism to load
one. `mcp-wrappers.md` records what that already cost: the official Python MCP SDK reaches
`pydantic-core`, which is compiled, so an entire category of integration cannot run in this
process.

An agent must not learn this by trying. Repeated failed `pip install` attempts are the worst
possible discovery path -- slow, confusing, and they look to the model like a permissions problem
it might route around. Publish instead:

- an import and capability manifest, so "can I use `numpy`" has an answer before the attempt;
- a diagnostic on a failed import of a compiled module that names the reason and the route, rather
  than surfacing a bare `ModuleNotFoundError`;
- the documented alternative -- an image-provided Python, a subprocess, a service -- since native
  work is not forbidden, only forbidden *in this heap*.

Pure-Python imports from the workspace do work, and that is worth testing explicitly rather than
assuming, because it is the case an agent will reach for first.

## Large values stay whole

Worth restating because discovery is where it gets violated by accident: bounding an observation
does not bound the value. A dataframe stays a dataframe; what is clipped is the rendering of it
that reaches the model. An agent that wants the tenth row asks for the tenth row rather than
receiving a truncated description of all of them and guessing. This is the same principle
`history.md` applies to the conversation and `agent-placement.md` applies to output, and it is the
reason processing data larger than the context window is possible at all.

## Open questions

- Whether `help()` is enough or whether proxies need their own description path. A Pyro proxy's
  signature is not locally knowable, so `help()` on one either lies or round-trips.
- Whether the active-work inventory belongs here or in `work.md`. "What am I waiting on" is a
  discovery question and a lifecycle question at the same time.
- How much of the runtime guide is a preamble and how much is a module docstring the agent reads
  on demand. The second is cheaper and only works if the model thinks to look.
- Whether the inventory should ever render values, given a bounded and explicit request. The safe
  baseline says no; usefulness pulls the other way.

## Unverified

- That `help()` and `inspect` behave normally in the static build was not checked. They should --
  nothing here needs a compiled module -- but the payload has surprised this phase once already
  with `ctypes`.
- The claim that a model reaches for `help()` unprompted is an assumption about behavior, not a
  measurement, and it is the assumption this page's economics rest on.
