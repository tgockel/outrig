# Subagents

A **subagent** is a second agent loop, launched by the agent itself, running against the same
container and the same tools. It exists so a big task can be split up without every piece having to
fit in one context under one preamble: the parent hands off a scoped job, the subagent works
through it with a fresh context, and only its report comes back.

The agent reaches subagents through the `outrig__` tools -- OutRig's own built-ins, which appear
alongside MCP tools like `fs__read_file` and `shell__exec` and are called the same way. The
`outrig` server name is reserved in config so nothing can shadow them.

Subagents are enabled by default. Set `subagents = false` on an `[agents.<name>]` block to leave
the tools out entirely; see [Reference -> Config](../reference/config.md).

## What a subagent is

A subagent is a headless REPL. `outrig run` drives one agent loop from lines you type; a subagent
is the same loop driven by prompts from the parent instead, keeping its own conversation history
across them. That is the whole idea -- the rest is bookkeeping.

What it inherits, and what it does not:

| Inherited                                 | Not inherited                              |
|-------------------------------------------|--------------------------------------------|
| The container, and everything in it       | Your conversation and context              |
| The MCP tools, over the same connections  | The session preamble -- the parent sets it |
| The tool limits and sampling settings     | The `outrig__` launch tools past the limit |
| The model and provider, unless overridden | `max-tokens`, when a model is named        |
| `/workspace`, at the same paths           | --                                         |

Because it borrows the parent's MCP connections, launching one starts no container and connects no
server. That keeps the [MCP trust model](mcp-trust-model.md) invariant intact: a subagent can only
reach tools the operator already granted the session.

## The tools

```
outrig__subagent({"name": "audit-config", "prompt": "..."})   -> returns immediately
outrig__wait_results({"names": [...], "min_count": 1})        -> which ones have something
outrig__get_result({"name": "audit-config"})                  -> that one's findings
outrig__subagent_send({"name": "audit-config", "prompt": "..."})
outrig__subagent_release({"names": ["audit-config"]})
```

Inside a subagent there is exactly one:

```
outrig__set_result({"status": "result", "body": "..."})    // findings
outrig__set_result({"status": "error",  "body": "..."})    // could not finish
```

`name` is a short kebab-case handle the parent picks, and is how it refers to that subagent
everywhere afterward.

### Choosing the subagent's model

`outrig__subagent` takes an optional `model`. Omit it and the subagent runs under the model the
launching agent is running under, which is what every call above does.

```
outrig__subagent({"name": "grep-callers", "prompt": "...", "model": "fast"})
```

The value is a **model name** -- a key under `[models.<name>]`, like `fast` or `smart` -- not a
provider and not a wire identifier like `gpt-4o-mini`. The names configured for the running build
are listed in the argument's schema, so an agent never has to guess one.

This is for delegating mechanical work. Grepping a tree, summarizing logs, or checking whether a
symbol is still used does not need the parent's expensive reasoning model, and moving it to a cheap
one keeps the parent's context free for synthesis.

Sampling and the tool limits still come from the launching agent -- `temperature`, `tool-call-max`,
and `tool-result-max` are unaffected by the model named. `max-tokens` is not: an output-token
ceiling belongs to the model, and a cheap model generally serves fewer output tokens than an
expensive one, so a subagent takes the ceiling that applies to the model it actually runs on. That
is `[agents.<name>].max-tokens` where the launching agent sets one, otherwise the named model's
`[models.<name>].max-tokens`, capped at the model's published ceiling where outrig knows one --
the same order [config](../reference/config.md#anthropic-models) applies to any other turn.

A subagent's model is fixed for its lifetime: `outrig__subagent_send` cannot re-point a live one,
since its history was accumulated under the model it started on. Launch a second subagent instead.

### Launching is not waiting

`outrig__subagent` returns as soon as the subagent starts. Two calls in a row give two subagents
running at once -- that is how fan-out works, and it does not depend on the model emitting parallel
tool calls. The parent's own tool calls stay sequential and ordered.

Fan-out has a ceiling. `subagent-width-max` (default `8`) bounds how many live subagents one agent
may hold, and a launch past it is refused rather than queued. Every subagent counts against the
budget until it is released -- including one that has finished and whose result the parent already
collected -- so at the limit the remedy is `outrig__subagent_release`, not a retry. The budget is
per launching agent: a parent that is full does not stop its own subagents from launching theirs.

### Reporting is explicit

A subagent reports by calling `outrig__set_result`, not by finishing with a nicely worded message.
This matters more than it looks: a model's last message is whatever it happened to close with, and
"Done, let me know if you need anything else" is a poor thing to hand another agent. Publishing
explicitly also makes failure honest -- a subagent that runs out of tool calls never publishes, so
the parent is told it stopped rather than handed a status string dressed up as an answer.

A subagent may call it more than once. The inbox keeps only the latest value, so a later call
simply supersedes an earlier one, and calling it does not end the subagent's round.

Both fields are required, which is deliberate. An earlier shape took `{result}` or `{error}` as
two optional strings, and models called it as `{}` constantly -- a schema where every field is
optional *permits* the empty call, so the only rejection possible came from the runtime, after the
subagent had already spent a tool call. `required` is the one constraint providers enforce and
models reliably attend to, and an exclusive choice between two optional fields cannot use it.
Folding the choice into a `status` enum makes both fields mandatory and the empty call
unrepresentable.

### When a report does not fit

`body` carries the whole report, so it is the one argument in the toolset large enough to run into
the model's output-token ceiling. A reply cut off part-way arrives as a `set_result` call with
`status` present and `body` missing, because fields generate in schema order.

OutRig recognizes that shape rather than passing a bare "missing field" back: the subagent is told
its report was probably truncated and asked to shorten it, which is something it can act on.

That ask is bounded, because the ceiling does not move between attempts. A subagent that could
have fit the report in `body` does it when first asked; past that it regenerates the same oversized
body and fails the same way, so retrying only spends model calls:

| Attempt | What the subagent is told                                                   |
| ------- | --------------------------------------------------------------------------- |
| 1st     | The report was probably cut off -- call again with a shorter `body`.        |
| 2nd     | Stop chasing the report; send `status: "error"` with a one-sentence `body`. |
| 3rd     | The failure has been reported upward already -- stop calling.               |
| 4th on  | The same refusal, unchanged.                                                |

The third attempt is what makes this end. OutRig publishes the truncation as that round's outcome
on the subagent's behalf, so a parent blocked in `outrig__get_result` wakes with the cause
immediately instead of once the subagent's whole tool-call budget has drained into one call that
could not succeed.

Having given up, the round stays given up: it publishes and warns once, and every later truncated
call gets the same refusal. That refusal is a tool *failure*, not a success, so identical repeats
keep accumulating against the breaker below -- which is what ends the round if the subagent will
not take the hint.

The durable fix is a bigger ceiling. `max-tokens` is unset by default, which leaves the limit to
the provider, and that default can be much lower than expected behind a gateway. The first
truncated attempt prints one line to stderr naming the agent and the ceiling that was in effect for
that subagent, because that is the part a human -- not the model -- has to act on. Set
`[agents.<name>].max-tokens` explicitly if subagents produce long reports, or, for one launched
under a named model, `[models.<name>].max-tokens` on the model it runs.

### Repeating a failing call does not pay

The same shape shows up beyond `set_result`: a model that cannot act on a tool error tends to
re-emit the identical call rather than try something else. Within a subagent round, OutRig counts
consecutive failures of the same tool called with **identical arguments**. The second such failure
gets a note appended to the tool result saying that repeating will not change the outcome; the
fourth ends the round. Changing the arguments, or any call that succeeds, resets the count -- a
subagent taking the hint is making progress, not looping.

A round ended this way publishes nothing, so its parent is told the reason it stopped rather than
the bare "stopped without calling `outrig__set_result`". The same is true of a round that runs out
of tool calls.

This applies to subagents only. Nothing about it is specific to reporting -- any tool can be looped
on -- but the primary agent has someone sitting in front of it who can interrupt, while a subagent
loops unattended inside its parent's tool call.

### Reading is edge-triggered

Each subagent's inbox carries a version, and the parent keeps a read position against it.
`outrig__get_result` blocks until there is something newer than what the parent last saw, returns
it, and moves the read position past it. Reading twice with nothing new in between blocks rather
than returning the same answer again.

`outrig__wait_results` blocks on the same condition across several subagents and reports **names
only**. Results can be large, so a call that returned three of them at once is exactly the
oversized tool result worth avoiding; the parent pulls each one with `outrig__get_result` and can
stop once it has enough. Use `min_count` to react to whichever finishes first:

```
outrig__subagent({"name": "audit-config", "prompt": "..."})
outrig__subagent({"name": "audit-mcp",    "prompt": "..."})
outrig__subagent({"name": "audit-net",    "prompt": "..."})

outrig__wait_results({"names": ["audit-config", "audit-mcp", "audit-net"], "min_count": 1})
  -> ["audit-mcp"]

outrig__get_result({"name": "audit-mcp"})
  -> what it found
```

> A subagent that is ready and left uncollected stays ready. Keep passing its name to
> `outrig__wait_results` and every call returns it immediately and never blocks for the others --
> drop names once they have been collected.

### Subagents stay addressable

Finishing a round does not end a subagent. It goes idle with its history intact, and
`outrig__subagent_send` reopens it -- to follow up on a result, or to redirect one that is still
working. A running subagent sees the message at its next step, so the parent never has to know
whether it is busy. Idle subagents live until released or until the session ends.

`outrig__subagent_release` takes the whole list or none of it. If any name in the call is unknown
-- or named twice -- nothing is released and every subagent in that call stays live, with its
history and its unread results intact. Releasing is unrecoverable, so a bad list is better retried
than half-applied.

### Subagents can launch subagents, up to a depth limit

A subagent's toolset is the session's MCP tools plus `outrig__set_result`, and -- while there is
depth left -- the same launch tools the primary has. The primary agent is the root at depth 1; a
subagent it launches is at depth 2, one of theirs is at depth 3, and so on. An agent gets the
launch tools only while its depth is under `subagent-depth-max` (default `3`), so nesting stops
on its own rather than running away.

Each launching agent has its own private view: it sees only the subagents it launched, names them
in its own namespace, and collects their results independently. A mid-tree subagent both reports
upward with `outrig__set_result` and collects its own children with `outrig__get_result`.

Set `subagent-depth-max = 1` to switch subagents off entirely, or `2` to allow only the single
layer the primary launches -- see [Reference -> Config](../reference/config.md). Releasing a
subagent, or ending the session, tears down everything it launched with it.

## What you see

Nothing reaches stdout except the primary agent's reply, so `outrig run > out.txt` still captures
only the model's text. Subagent activity shows up two other ways:

- On stderr, with each trace labeled by name. Concurrent subagents interleave; filter by name.
- In `<session_dir>/logs/subagent-<name>.log`, beside the MCP servers' stderr logs, holding that
  subagent's prompts, replies, and published outcomes.

Ctrl-C behaves as it always has: it abandons whatever the parent was waiting on and returns you to
the prompt. Subagents keep running and are still collectable on the next turn -- the same way an
abandoned `shell__exec` keeps running to completion inside the container. A second Ctrl-C ends the
session, and everything shuts down with it.

## The shared workspace

Every subagent writes to the same bind-mounted `/workspace` as the parent and its siblings. Two
subagents told to edit the same file will fight over it, and OutRig does not stop them -- keeping
concurrent work disjoint is the parent's job. Read-only analysis fans out safely; parallel edits
want non-overlapping scopes.

## See also

- [MCP Servers](mcp-servers.md) -- where a subagent's tools come from.
- [MCP Trust Model](mcp-trust-model.md) -- why sharing the container keeps the boundary intact.
- [Providers, Models, and Agents](llm-providers.md) -- the `[agents.<name>]` block.
- [Usage -> outrig run](../usage/run.md) -- the REPL these run underneath.
