# The embedded CPython is built for the target architecture, not the image's

`crates/outrig/build.rs` embeds the `python-build-standalone` payload for the *target*
architecture, as it builds the `outrig-enter` launcher, and every library session mounts it at
`/outrig/python`. podman runs a foreign-arch image under emulation without saying so, so an
emulated foreign-arch primary gets a host-arch interpreter. What that does is unverified. The
host kernel can exec a host-arch static binary without `qemu-user`, so the likely outcome is
that it runs -- beside image binaries that are emulated, so the agent's Python and the programs
it spawns disagree about the machine.

The launcher has the same blind spot; that half is #285. Whatever check lands there should
cover the payload too: `podman image inspect --format {{.Architecture}}` answers both, and
`build.rs` already pins both architectures.

## Why deferred

Found while embedding the payload (`0003-01`), before any foreign-arch image had been run under
it. Nothing fails while an image matches the host's architecture, which is the common case and
every CI row.
