# A session container's PID 1 reaps nothing, so orphans fill `pids.max`

Every session container runs `sleep infinity` as PID 1 (`container::mod`'s
`build_podman_run_cmd`). `sleep` never calls `wait`, so every process that outlives its
parent inside the container is re-parented to PID 1 and stays there as a zombie for the life
of the session. Zombies hold a PID each, and podman's default `pids.max` for the container is
2048.

## Evidence

Running this repo's own suite in `outrig-standard` reaches that ceiling. Each
`cargo test --workspace` leaves roughly a hundred orphans behind -- the detached cleanup
chains `supervise.rs` starts on purpose (`sh` + `sleep`), plus the `podman` and `buildah`
children the image tests spawn and abandon. After about twenty runs:

```
$ ps -eo stat= | sort | uniq -c | sort -rn | head -3
   2014 Z
      5 S
$ cat /sys/fs/cgroup/pids.max /sys/fs/cgroup/pids.current
2048
2023
```

From there every `fork` and every `pthread_create` fails with `EAGAIN`, which surfaces as
`OS can't spawn worker thread: Resource temporarily unavailable`, as `Spawn { program:
"git", ... WouldBlock }` in dozens of unrelated tests, as `ld terminated with signal 6`
during a build, and as one test hanging forever. Nothing in the container can clear it: only
PID 1 can reap, `pids.max` is read-only from inside, and the session has to be restarted.

Found while verifying #671 in-container, where the 50-run soak its acceptance criteria ask
for ran the container out of PIDs at about run 20.

## Correction

Give the container an init that reaps. `podman run --init` installs catatonit as PID 1,
which reaps orphans and forwards signals; it also removes the reason
`--stop-signal=SIGKILL` is passed, since catatonit does handle SIGTERM. The alternatives are
worse: raising `pids.max` only moves the ceiling, and asking every workload in a container
not to orphan processes is not something OutRig can enforce.

Check what `--init` costs: it binds a host binary into the container (`--init-path`), which
has to exist on the host and be the right architecture, and `doc/` would have to say what
happens when it does not.

## Acceptance

- A container started by OutRig reaps orphans: a process whose parent exits leaves no `Z`
  entry behind.
- Fifty `cargo test --workspace` runs in `outrig-standard` leave `pids.current` where they
  found it.
- `doc/` says which process is PID 1 and what it does with signals and orphans.
