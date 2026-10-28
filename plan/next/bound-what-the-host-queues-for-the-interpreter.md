# What the host queues for the interpreter is not bounded

## Context

The host writes to the interpreter's stdin through one unbounded queue (`python::host`'s writer).
Before `0003-22` everything on it was the host's own: executions, messages, answers the host
chose to send. With hosted calls, most of it is caused by what the interpreter sends:

- the bindings process's replies, which the relay passes on as they come, up to 128 MiB a frame;
- the relay's own answers to the interpreter's lines: a `refused` per request it refuses, a
  `closed` per frame it cuts or per line naming a binding the session does not hold.

The relay bounds what it holds for the bindings process (`python::hosted`), but nothing bounds
this direction. Agent code can stop its interpreter from reading stdin -- it can stop the process,
or hold a lock the reader thread needs -- after sending requests whose replies are large, or keep
sending lines the relay answers. The queue then grows in the host's memory for as long as the
session lives.

## Why not backpressure

The obvious bound -- stop reading the interpreter's stdout, or the bindings process's, while the
queue is over a limit -- can deadlock a well-behaved interpreter: its reader thread writes close
notices to stdout, and a full stdout pipe would block it, so it would stop reading stdin, and the
queue would never drain.

## Options, none chosen

1. **Count what the queue holds, and refuse past a bound.** The writer reports each line as
   written; past the bound the relay closes a connection whose reply frame does not fit, on both
   sides, as a reply toward the bindings process that does not fit closes its connection, and
   drops the answers it would otherwise send. A `refused` it drops leaves a call waiting, so
   either the bound is generous enough that only an interpreter that stopped reading reaches it,
   or the session ends the interpreter there.
2. **End the session's interpreter past a bound**, as a protocol violation: an interpreter that
   leaves the host's lines unread past the bound is treated as dead, which the session already
   handles (`plan/phase/0003-python/continuation.md`).

## Acceptance

- An interpreter that stops reading while its bindings answer, or while it sends lines the relay
  answers, holds the host's memory for it under a stated bound; an interpreter that reads keeps
  every call working.
