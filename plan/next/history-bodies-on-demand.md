# Past its warning, the conversation mirror still holds everything

## Context

`history.md` chose to mirror the whole conversation into the interpreter and named the fallback:
mirror each turn's shape and fetch bodies when they are read. `0003-12` measured the cost -- the
mirror holds about what it carries, 16.9 MB for a thousand 16 KiB turns -- and decided the
response: past an eighth of the memory ceiling, `History._tell_of_size` says so once on stderr and
keeps every turn.

So a long enough session still grows until it takes the ceiling from the agent's own work, and
from every agent co-hosted in the interpreter once there is more than one.

## Shape

- Past a threshold, the host pushes a turn's calls without their `result` text, and `Call.result`
  becomes a property that asks the host for it -- a query like `cpu`, answered on the reader
  thread. Older turns can be thinned the same way, oldest first.
- What a scan costs changes: reading every result becomes one round trip per call. That is the
  trade `history.md`'s "Rejected: a proxy store" warned about, bounded here to bodies only, so
  expressions over `id`, `round`, `text`, and `source` stay free.
- The host store keeps everything either way; it is outside the ceiling.

Worth doing when subagents make a second kernel share the ceiling, or when a session is seen to
reach the warning.
