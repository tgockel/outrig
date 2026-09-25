# A `run-new` container carries no session label, so `clean`'s stray sweep cannot see it

## Context

`outrig run` names its primary `outrig-<sid>` and labels it `org.outrig.session=<sid>`
(`session_setup.rs`). `outrig clean` finds containers whose session record is gone by that label
(`clean.rs`, `LABEL_SESSION`) and removes them.

`outrig run-new` launches through `Outrig::launch`, which names the container
`outrig-<timestamp>-<hex>` itself and attaches no label. `0003-05` records the real name in
`session.json` through `PythonAgent::container_name`, so `discard` and `clean` tell a live
`run-new` session from a finished one correctly. What they cannot do is find a `run-new`
container whose record has been deleted. A `run-new` killed with SIGKILL leaves a running
container that only `podman rm` removes.

The session id and the container's suffix also differ, which `run`'s never do.

## Shape

`LaunchSpec` taking labels, and a name or name prefix, is the direct fix. It grows the facade's
public surface, which the phase's exit criteria hold to `outrig::PythonAgent` alone, so it waits
for a task that is allowed to widen `LaunchSpec`, or for the facade to grow a session identity of
its own.

## Acceptance

- A `run-new` container carries `org.outrig.session=<sid>`, and `clean` removes one whose record
  is gone, in a test shaped like `clean`'s existing stray tests.
