# 0003-01 -- A verified static CPython is mounted where the image has none

## Context

The phase's premise is that an agent acts by writing Python, and its constraint is that the
primary image may contain no Python at all -- `harness-components.md` says the interpreter is "a
static build mounted read-only, so the image needs nothing -- not a Python, not a shell, not a
libc of its own." Nothing else in the phase can be built or tested until that is true.

The prototype settled the artifact: `python-build-standalone`'s `+static` variant, pinned by
release and checksum. The `+static` suffix is not cosmetic. The plain musl builds link against a
musl runtime the *image* is expected to supply, which defeats the point; the prototype's fetch
script checks the ELF with `file` for exactly this reason and refuses anything that is not a
statically linked ELF64.

Fetching is a developer step, deliberately. `README.md` puts shipping the interpreter inside the
binary out of scope: nothing is downloaded at build time or at session run time.

## Goal

A developer can fetch the pinned interpreter once, and a session mounts it read-only into the
primary container at a known path, verified to be the artifact that was pinned.

## Deliverables

- `scripts/fetch-python-payload.sh`, ported from the prototype: pinned release and `PY_VERSION`,
  a per-architecture SHA-256, `curl --fail`, `sha256sum --check`, and the `file`-based assertion
  that the result is a statically linked ELF64 for the expected machine. It lands in
  `$XDG_CACHE_HOME/outrig/python/$ARCH`.
- **The checksum is checked, not just recorded.** A mismatch aborts and prints both digests. This
  is the one place a supply-chain substitution would enter, and the script is the only gate.
- A payload locator in `outrig` that resolves the cache path and the in-container path, so no
  caller spells either out.
- The read-only mount, added to the primary container's spec alongside the existing `/workspace`
  mount and `outrig-enter` materialization, which is the precedent for placing an OutRig-supplied
  binary into a session.
- **A missing payload is a startup error that says what to run**, naming the architecture and the
  script -- not a failure on the first tool call, and not a silent fall back to some `python3` the
  image happens to carry. `README.md` is explicit that the bundled library is the only one.
- `scripts/` gains an entry in `.claude/CLAUDE.md`'s tree description if the existing wording does
  not already cover a fetch script.

## Acceptance

- With the payload absent, starting a session fails with a message naming the architecture and the
  fetch command. Tested in `crates/outrig/tests/`, not by inspection.
- With the payload present, `podman exec <primary> /outrig/python/bin/python3 -I -c 'print(1)'`
  succeeds against an image containing no Python. The image used by the test must be one that
  genuinely lacks an interpreter, or the test proves nothing.
- The script refuses a corrupted archive: a unit-level check that a bad digest exits non-zero
  before extraction, so a partial extract cannot be left behind.
- The script refuses a dynamically linked build. Easiest as a check on the `file` assertion's
  logic rather than by fetching a second artifact.
- `cargo test`, `cargo clippy --all-targets`, `cargo fmt --check` pass.

## Design forks

1. **Where the payload is mounted in the container -- pick a path and fix it.** The prototype used
   a fixed path under `/outrig`. Anything under a directory OutRig already owns is fine; what
   matters is that it is one constant, referenced everywhere, and cannot collide with a workspace
   path a user chose.
2. **Whether the locator is public -- Recommended: no.** `crate-split-tradeoffs.md` budgets one
   public entry point for the whole phase. The locator is internal to the container setup.

## Dependencies

None.

## See also

- `plan/phase/0003-python/harness-components.md` -- "The container side", which decides the mount.
- `crates/outrig/src/container/enter/mod.rs` -- `materialize`, the existing precedent for placing
  an OutRig-supplied binary into a session directory and mounting it.
- `plan/next/primary-image-needs-no-sleep.md` -- what a minimal image still cannot satisfy, which
  this task does not address.
