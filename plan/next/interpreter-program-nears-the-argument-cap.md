# The interpreter program nears the argument cap

## Context

`interpreter.py` reaches the payload's `python3` as its `-c` argument, compressed and
base64-encoded, and Linux caps one argument at 128 KiB (`crates/outrig/build.rs`,
`ARG_STRLEN_MAX`; the build fails past it). `0003-30` grew the bootstrap from about 88 KB to about
105 KB, 80% of the cap. The phase's next tasks -- the decorator, agent classes, and host-program
parents -- each add interpreter code of the same kind, and two more steps of that size reach the
cap.

## Shape

- Move a self-contained part of the program into the `outrig` package the payload already mounts
  (`payload::PACKAGE`), imported as the program starts, as `outrig/_agents.py` already holds the
  child agents' errors and input checks. The child-agent machinery in `interpreter.py` is the
  largest part this phase added.
- Or stage the program as a file in the payload, and keep only a bootstrap on the command line.
