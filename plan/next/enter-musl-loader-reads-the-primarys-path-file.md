# A musl sidecar's loader reads the primary's library-path file

## Context

`outrig-enter` runs the sidecar's payload through the sidecar's own dynamic loader, and tells
that loader where to look with `--library-path` (`src/container/enter/launcher.rs:561-568`,
each entry graft-prefixed). What it cannot tell the loader is where *not* to look.

For glibc there is a flag for exactly that, and the launcher passes it
(`launcher.rs:570-574`): `--inhibit-cache` stops `ld.so` consulting `/etc/ld.so.cache`, which
after the `setns` is the *primary's* -- a cache naming the primary's `.so` files by absolute
path, every one of which resolves outside the graft.

musl's loader has no equivalent flag, and the launcher's comment says so. It reads
`/etc/ld-musl-<arch>.path` instead, a plain list of search directories, and after the `setns`
that file is the primary's too. When the file exists, its contents *replace* the built-in
search path rather than extending it, so a primary that ships one hands the sidecar's loader a
list of directories in the primary's rootfs and nothing else.

This is the same class as the absolute-symlink escape the launcher's ordering contract now
describes (`src/container/enter/canon.rs`, and the CHANGELOG's *no longer dies on SIGSEGV*
entry) -- a path the sidecar's payload resolves *after* the namespace join, which therefore
means one of the primary's files -- but it is not reachable by the same remedy.
Canonicalization settles the paths the launcher hands over; this one the loader reads on its
own, after the exec, from a file the launcher never names.

## Why it might matter

Latent rather than active, and less likely than the symlink case: Alpine ships no
`/etc/ld-musl-x86_64.path` by default, so the loader falls back to its built-in
`/lib:/usr/local/lib:/usr/lib`, which the graft prefix covers by other means. It fires when the
*primary* image plants one -- something a `RUN echo ... > /etc/ld-musl-x86_64.path` in a
user's Dockerfile does, and something a musl-based primary that vendors libraries plausibly
does on purpose.

The failure it would produce is the bad kind: the loader finds *a* `libc.so` and silently
prefers the primary's, so the payload either dies the way the glibc pairing died -- SIGSEGV,
nothing on stderr -- or, worse, runs against a subtly different C library.

MCP images are very often Alpine-based (the launcher's own comment makes the point), so the
sidecar side of this is the common case; only the primary side is rare.

## Goal

A `view = "primary"` musl sidecar resolves its libraries out of its own image regardless of
what the primary's `/etc` contains.

## Deliverables

- **A decision on the mechanism** -- see the forks. Whatever it is, it must hold while the
  payload runs, not only while the launcher does: the loader reads the file after the exec.
- **A test with a primary that plants the file.** The e2e suite has no musl primary at all
  today; the closest is `crates/outrig/tests/fixtures/mcp-fs` (Alpine), used as a primary by
  `library_surface.rs`. A `RUN printf '/nowhere\n' > /etc/ld-musl-x86_64.path` on top of it is
  the whole reproduction.
- **A note in the launcher's ordering contract** if the answer is a mount, since it would join
  the pre-`setns` privileged phase.

## Design forks

1. **How the primary's file is kept out of view -- Open.** A bind mount of the sidecar's own
   `/etc/ld-musl-<arch>.path` over the primary's, from the graft, is the direct answer and
   costs one more `mount` in the privileged phase -- but the sidecar usually does not have the
   file either, so it would be a mount of a nonexistent source. Mounting the graft's `/etc`
   whole is wrong: the payload wants the primary's `/etc` for everything else, which is the
   point of the view. Writing a file into the sidecar's rootfs before the `open_tree` and
   binding *that* works but makes the launcher a writer, which it has never been.

2. **Whether to name the arch at all -- Open.** The file's name carries the same
   `<arch>` the launcher already keys `MULTIARCH` off (`launcher.rs:102/111`), so this
   interacts with `plan/next/enter-arch-mismatch.md`: both want one place that answers "what
   architecture is this helper for".

3. **Whether glibc's `--inhibit-cache` is actually sufficient -- Open.** Worth confirming
   before building anything for musl: glibc also reads `/etc/ld.so.preload`, which
   `--inhibit-cache` does not cover and which the primary likewise owns after the `setns`. If
   that is a hole too, both loaders need the same shape of answer and this entry covers both.

## Dependencies

- None. Independent of `plan/next/enter-arch-mismatch.md`, though fork 2 touches the same
  question.
