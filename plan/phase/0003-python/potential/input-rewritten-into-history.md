# The agent records a shorter form of long input

## Shipped

A design property, stated rather than built: user input is data the agent's code reads, not text
that enters the model's history by itself. A message waits on the user channel until the agent's
code calls `receive()`, and nothing enters the model's history except what the agent's code
observed -- the result of the expression it evaluated, clipped at 16 KiB (`messages.md`,
`execution-and-rounds.md`, `history.md`). What the model sees is therefore separated from what
the user typed by the agent's own code, and the separation already works: an agent that binds a
long message to a name and prints its first lines has put the first lines in its history and
kept the rest in the namespace.

## Alternative

Tooling that makes the shorter form deliberate instead of incidental. An agent that reads a long
or discursive message records a short form of it -- the instruction without the surrounding
text, or a summary the agent writes -- and that short form is what stands in its history, with
the full message kept in the namespace and in the store. The parts that do not exist yet: a way
for the agent to say that an observation stands for a message, so the history and the per-call
manifest show the substitution; and guidance in the preamble on when a message is worth
shortening. The user's words are never rewritten in the store, and the `Delivery` the agent
received stays the record of what arrived.

## Evaluation

Context cost and fidelity. Context cost is the tokens a long message costs across the rounds it
stays in the view, with and without a short form. Fidelity is whether the agent's later choices
match the full message: a short form that drops a constraint is the failure
`active-intent-record.md` is about, reached from the other direction. A model that shortens its
own instructions can also drop the part it did not want, so the trials include messages with
constraints the agent would rather not have.

## When

Not before the intent record. Both are about what the model keeps of what it was told, and a
short form of a message is a candidate item in such a record. Nothing in 0.3.0 depends on it.
