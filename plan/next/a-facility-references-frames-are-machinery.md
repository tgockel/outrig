# A facility reference's frames are machinery

Found while landing `0003-42`.

An operation on an `outrig.Facility` runs through `_agents._on_target`'s `method`,
`_agents._target` and `_kernel._resolve` before it reaches the kernel's stub. Agent code then sees
those frames in a traceback: a refused attribute read through a reference showed `File
".../outrig/_agents.py", line 819, in method`. The same operation on a stub shows none of
`HostedRoot`'s, because `_hosted_frame` (`_interpreter.py`) treats a frame as hosted machinery only
when its globals are the interpreter's and its code's qualified name starts with a name in
`_HOSTED_CODE` (`_on_root`, among others).

`_hosted_frame` also decides where an interrupt may land (`_in_hosted_work`). So a reference's
frames count as agent code there too. That is harmless before the stub is reached, but it should
be a decision, not an accident.

## What would decide it

- Whether to recognize the package's facility frames, for example by code object, as
  `_INTERRUPTIBLE_CODE` is recognized, or to move the forwarding table where `HostedRoot`'s lives,
  so one table serves both. Moving it means installing it from `_load_outrig`, since `_agents` is
  not loaded when the interpreter module defines its classes.
- A test: a refused read through `outrig.facility("fx")` whose traceback names only the agent's
  line, the way a stub's does.
