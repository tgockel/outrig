# A line from the interpreter is trusted to name the agent that sent it

## Symptom

Every agent's code runs in one interpreter process and shares its one protocol pipe, and it can
reach the interpreter's own `_send` (`outrig._agents._launch.__globals__`, for one). So a line
can name any agent as its `agent`, and the host reads that field as the sender: `dispatch`
(`crates/outrig/src/python/host.rs`) hands it to the registry as given. What the registry then
decides from it is decided for whoever the line claims to be. A launch takes the claimed parent's
depth, so a child at `subagent-depth-max` that claims to be the primary launches past it; a
release, a request, a cancel and a call are the claimed agent's too; and a line claiming `owner`
reaches `Inner::line` as the program's would. `0003-50` met this with its reset -- a parent's
code was to lift its child's model-request limit, and the child could claim the parent -- and
made the reset the owner's alone, over the owner's own connection, rather than trust the field.
Found by `0003-50`'s review.

## Shape

Decide what the session holds a limit against. The limits `work.md` puts "outside generated
code" -- depth, children, spend -- are only as good as the sender's name, and nothing in one
process can prove it. Either such a decision moves off the interpreter's say-so (the host keeps,
per request it answers, what it may do), or the docs stop claiming a bound the agents' code can
step around, naming which limits hold against code that means to evade them. At the least, a
line from the interpreter that names `owner` is refused at `dispatch`.
