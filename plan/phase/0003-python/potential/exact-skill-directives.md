# Exact executable directives, parsed literally

## Shipped

A `/name text` line reaches the main agent as text on its user channel, with the resolved skill
named on the `Delivery`, and the agent's code calls `outrig.skills.invoke(name, text)`
(`skills.md`, "From `/name` to a call"; `0003-29`). `invoke` derives a parameter dataclass,
`<Skill>Params`, from the entry's signature, and a typed agent call, `parse_params(text)`, maps
the text to an instance under the entry's docstring and parameter help; `invoke(name,
params=<Skill>Params(...))` skips the model. The model maps the user's words to the skill's
arguments, for `/review-diff a.py --max-parallel 2` as for prose; where the words do not
determine a required field, or the mapping would widen scope -- `/review-diff just the auth
changes` must not become every changed file -- the parser returns a `Clarification` and the main
agent asks the user before anything runs. There is no deterministic parser and no fallback from
one path to the other: the maintainer removed the parse-then-interpret-prose path because text
that parses is not proof the user meant arguments.

## Alternative

Two intentions, told apart explicitly. An exact executable directive is parsed literally,
validated against the entry's signature, scheduled once, and answered with usage on a parse
error, with no interpretation of prose at all. A conversational request goes through the model
as now. The exact directive is still not an execution path of its own: it is scheduled in the
agent's kernel through the same admission, interruption, history and accounting as a call the
agent makes, and its origin is recorded as a human directive, not as a model-authored call. Its
result is surfaced on the next model interaction. Instruction-only skills stay model-mediated.

What it costs: a second entry path to execution, less uniformity in who owns a round, scheduling
while the kernel is busy, and a representation of host-authored execution in the history that
says who wrote it. How a user marks a line as exact rather than conversational is undecided.

## Evaluation

The reviewer's own criterion: twenty identical directives, each under a different conversation
history, produce identical resolved calls, and each directive is invoked exactly once. Identical
outputs are not the measure, since the files change between runs. Against it, the shipped path
under the same twenty histories: how often the typed call resolves
`/review-diff a.py --max-parallel 2` to anything other than `paths=["a.py"], max_parallel=2`,
and what the model turn costs in latency and tokens.

## When

After `0003-29` lands and the typed mapping has been used for a while. If its variance on exact
lines is near zero, the alternative is not worth a second execution path.
