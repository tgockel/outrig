# A round's ending does not name the hosted calls it left running

## Context

A round ends when the model yields, not when its work is done. `RoundOutcome::still_running`
names the tasks still on the agent's event loop, and `run-new` prints them as the round closes, so
the user knows what is still at work. A hosted call is part of that work, and the line does not
show it: a call a background task awaits shows only as that task, and a call whose caller was
interrupted keeps running on the host (`cancel-a-running-hosted-call.md`) and shows nowhere.

The relay knows every request the bindings process was sent and has not ended: it is what the
shutdown's drain waits on (`python::hosted`). So the round's ending could name each one -- its
binding, its member, how long it has run -- the way it names tasks.

## Options, none chosen

1. **`StillRunning` gains the hosted requests in flight**, read from the relay as the round ends,
   and `run-new`'s closing line names them beside the tasks.
2. **Only their number**, beside the tasks: enough to say that something on the host is still at
   work, without a line per call.

## Why later

`0003-22` records every request and drains them at shutdown; what a round's ending tells the user
is `run-new`'s choice, and the line's shape -- one entry per call, or a count -- is the decision
left open here.
