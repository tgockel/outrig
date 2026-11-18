# An instance's child can be unset after `ready()` returns

Seen once under the full workspace suite (it passes alone, 8 of 8):
`agent::self_tests::an_instance_left_alone_has_its_launch_round_and_goes_idle` printed

```text
AttributeError: 'NoneType' object has no attribute 'status'
```

from `await until(lambda: alone._outrig.child.status == 'idle')`, right after
`await alone.ready()` returned.

`_Runs.ready()` (`crates/outrig/src/python/outrig/_interpreter.py`) awaits `self.oriented`, a
`concurrent.futures.Future` settled from the reader thread when the child's first round ends.
`_Runs.launch` assigns `self.child` only once `self.kernel.runtime._spawn(...)` returns on the
kernel's loop. Nothing orders the two, so a loop that resumes `ready()`'s waiter before `launch`
resumes leaves `child` unset when the instance reads ready.

Fix: settle what `ready()` waits on only once the child is assigned (or have `launch` settle a
loop-side readiness after assigning `child`), so an instance that reads ready always has its
child. The test then needs no change.
