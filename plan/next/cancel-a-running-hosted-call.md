# An interrupted hosted call keeps running on the host

## Context

RPyC 6.0.2's protocol has twenty request handlers (`rpyc/core/consts.py`), and none of them
cancels a request. So when agent code is interrupted during a hosted call, `0003-17` raises
where the call is awaited, saying the call's outcome on the host is unknown, and the binding
process keeps running the call until it returns; its reply is dropped when it comes, and
`0003-22` records the call's outcome then as `returned` or `raised`
(`plan/phase/0003-python/lifecycle.md`). At shutdown a running call is given until the drain
deadline, and then the binding's process group is killed.

Between an interrupt and shutdown, a call the user stopped can still act: a push still sending, a
long subprocess still running. What the agent is told -- outcome unknown -- is true, but nothing
stops the work. A cancel message would also need a thread to read it: RPyC runs a request on the
thread that read it from the connection.

## Options, none chosen

1. **A worker thread per call** in the binding process, so a thread stays free to read the
   connection, and a new cancel message that raises in the worker (`PyThreadState_SetAsyncExc`).
   That takes effect only at a bytecode boundary, so a call waiting on a subprocess or a socket
   stops when that wait ends.
2. **Kill what the call started.** Long host calls often wait on a subprocess, as the example
   library's do; killing it makes the Python call fail at once. The binding process has to know
   which call started which process, which concurrent calls from several kernels make hard.
3. **Kill and restart the binding process.** Stops everything, at once and with certainty. Every
   other kernel's calls on that binding stop too, every reference to its objects becomes invalid,
   and its factory runs again.

In each, the outcome stays `unknown` unless the call is known to have stopped before any effect,
since a cancel cannot establish that nothing happened. Whichever is chosen works the same for any
library.

## Acceptance

- After an interrupt, a hosted call waiting on a 60-second subprocess ends on the host within a
  stated bound, and its event records the cancel and the outcome.
- Calls from other kernels on the same binding are unaffected, or, under option 3, are reported
  as ended by the restart.
