# 0003-01 -- A verified static CPython is mounted where the image has none

## Context

The phase's premise is that an agent acts by writing Python, and its constraint is that the
primary image may contain no Python at all -- `harness-components.md` says the interpreter is "a
static build mounted read-only, so the image needs nothing -- not a Python, not a shell, not a
libc of its own." Nothing else in the phase can be built or tested until that is true.

The prototype settled the artifact: `python-build-standalone`'s `+static` variant, pinned by
release and checksum. The `+static` suffix is not cosmetic. The plain musl builds link against a
musl runtime the *image* is expected to supply, which defeats the point; the prototype's fetch
script checked the ELF for exactly this reason and refused anything that was not a statically
linked ELF64.

**`cargo build` and `cargo run` work with no setup step.** This task was first written with a
developer fetch script and embedding deferred to release work; the maintainer rejected that, and
the rule stands for everything OutRig supplies at run time. `build.rs` fetches, verifies, and
embeds the interpreter, as it already builds and embeds `outrig-enter`.

## Goal

A plain `cargo build` produces an OutRig whose every session mounts a verified static CPython
read-only at a known path, in an image that has no Python of its own.

## Deliverables

- **`build.rs` fetches and verifies the pinned archive.** Release, version, and a per-architecture
  SHA-256 from the release's own `SHA256SUMS`, downloaded once per machine into the user's cache.
  `OUTRIG_PYTHON_ARCHIVE` names a local copy for a build without network.
- **The checksum is checked, not just recorded.** A mismatch fails the build and prints both
  digests, before a byte of the archive is decompressed. This is the one place a supply-chain
  substitution would enter, and the build is the only gate.
- **The interpreter is checked to be a static ELF64 for the target machine** -- no `PT_INTERP`,
  the right `e_machine` -- using the ELF parser `outrig-enter` already has.
- **The verified archive is embedded**, and a session's first start unpacks it under the user's
  cache directory, atomically, so concurrent first sessions cannot see a half-written tree.
- A payload locator in `outrig` that resolves the cache path and the in-container path, so no
  caller spells either out.
- The read-only mount, added to the primary container's spec alongside the existing `/workspace`
  mount in `Outrig::launch`.
- **A build that could not fetch the archive degrades as a missing musl target does**: a cargo
  warning, an empty artifact, and a session-start error that says why -- never a silent fall back
  to some `python3` the image happens to carry. `README.md` is explicit that the bundled library is
  the only one. CI makes the degradation a build error.

## Acceptance

- A plain `cargo build`, then a session start, yields a working interpreter with no other step.
  Tested in `crates/outrig/tests/`, not by inspection.
- With the payload embedded, `podman exec <primary> /outrig/python/bin/python3 -I -c 'print(1)'`
  succeeds against an image containing no Python. The image used by the test must be one that
  genuinely lacks an interpreter, or the test proves nothing.
- The build refuses a corrupted archive: a unit-level check that a bad digest is refused before
  anything is decompressed.
- The build refuses a dynamically linked interpreter, and one for the wrong machine.
- A build without the payload produces a session-start error naming what would supply it.
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

## Decisions

- **Embedded by `build.rs`, not fetched by a script.** The first cut followed this file as written:
  a `scripts/fetch-python-payload.sh` the developer ran once, with embedding deferred as release
  work. The maintainer rejected it -- `cargo build` and `cargo run` must work with no external
  step -- and the script, its tests, and the phase README's out-of-scope line are gone.
  - `build.rs` downloads the pinned archive with `ureq`, hashes it with `sha2`, and reads it with
    `ruzstd` and `tar`. These are all Rust, so the build needs no `curl`, `zstd`, or `file` on the
    host.
  - The download is cached once per machine in `$XDG_CACHE_HOME/outrig/downloads` (else
    `~/.cache/...`, else `OUT_DIR`). Every target directory, profile, and feature set gets its
    own `OUT_DIR`, so caching there alone would repeat a 36 MB download several times over.
  - The embedded archive adds about 36 MB (x86_64) to any binary that links `outrig`. It is
    embedded as released, `python/build` included. Stripping that would mean re-encoding at build
    time, and was not worth the build cost.

- **The build is the gate, and its checks are shared by `include!`.** `src/python/archive.rs`
  holds `verify_archive` and is `include!`d by `build.rs`, beside `src/container/enter/elf.rs`,
  whose `elf_interp` now also decides whether the interpreter is static. The crate compiles
  `archive.rs` under `#[cfg(test)]` to run its tests; `elf.rs`'s items became `pub(crate)` so
  that module can reach them.
  - The digest is checked before anything is decompressed. The test proves the order by handing
    over bytes that are not a zstd stream: a check that ran after decoding would report a decode
    error instead.
  - The static check also compares `e_machine`, which `elf_interp` never reads
    (`plan/next/enter-arch-mismatch.md`).
  - A digest mismatch or a non-static interpreter fails the build whatever
    `OUTRIG_REQUIRE_PYTHON` says: that is a wrong payload, not a missing one.

- **Degrades like the launcher, and retries by itself.** An unreachable network or an unreadable
  `OUTRIG_PYTHON_ARCHIVE` gives a cargo warning, an empty artifact, and
  `OUTRIG_PYTHON_UNAVAILABLE_REASON`, which the session-start error quotes.
  - `OUTRIG_REQUIRE_PYTHON` turns this into a build error, and CI sets it wherever it sets
    `OUTRIG_REQUIRE_ENTER`.
  - A degraded build also declares a never-existing path as an input, so cargo reruns the script
    on the next plain `cargo build`. Regaining network is then the whole fix.
  - Measured here: the second build with the same failing input showed `Running
    build-script-build`, while a successful build was `Fresh` on its second run. `cargo package`
    with both `REQUIRE` variables set built the packaged crate, so the shipped `build.rs` finds
    its includes.

- **Unpacked on a session's first start, atomically.** `payload::host_dir` unpacks
  `python/install` into a sibling temp directory using `tar`'s `unpack_in`, which refuses members
  that would land outside it, then renames the finished tree into place, on `spawn_blocking`.
  - The rename is what makes the directory's existence mean "complete", so there is no stamp
    file. The script's stamp did not come across: it existed to notice a changed pin, which a
    directory named for the archive already does.
  - A session that loses the race to a concurrent one uses the winner's tree.
  - The directory is named for the archive, so a new pin unpacks beside the old one rather than
    over it.
  - The archive has no directory entries and 1,047 symlinks (terminfo), all relative and inside
    the tree. Measured on the real archive.
  - `ruzstd` needs its window limit lifted: the archive's window is 128 MiB, past the 100 MiB
    default. Both decoders take `u64::MAX`, because both only ever see bytes already pinned by
    digest.
  - `ruzstd` and `sha2` are built at `opt-level = 3` in dev profiles. The first launch in a
    debug-built test unpacks and runs the interpreter in about 1 s.

- **The mount is unconditional, and it lives in `Outrig::launch`.** A session on this line is a
  Python session, because the model's only tool submits Python. There is no flag and no opt-in.
  The prototype gated the mount behind `if args.llm_session` only because it bolted Python onto
  the legacy CLI path, where `outrig mcp` has no agent; that gate did not come across. The CLI's
  `session_setup` (legacy `run` and `mcp`) is untouched because it is the 0.2 system, kept as-is
  for merges. It is not a Python-free mode of this one.

- **No public surface.** The locator (`python/payload.rs`) is crate-private. `Outrig::launch`
  appends the payload to the spec's ordinary `mounts`, built with a crate-private
  `ContainerMount::shared_read_only`, so `public-api.txt` does not move. A build without the
  payload fails at session start with `OutrigError::Configuration` rather than a new variant, for
  the same budget.

- **`/outrig/python`, and all of `/outrig` reserved.** `PAYLOAD_MOUNT` and `OUTRIG_ROOT` are
  defined once in `python/payload.rs`.
  - The whole directory is reserved, not just `/outrig/python`. A caller's mount at `/outrig`
    itself would make the runtime create `python/` inside that caller's host directory, or fail
    on a read-only one.
  - Destinations are compared by path component after resolving `.` and `..` textually, so
    `/outrigger` passes and `/tmp/../outrig/python` does not. A symlink in the image, such as
    `/data -> /outrig`, is not resolved: that needs the image's filesystem.
  - It is checked before the payload is unpacked, and enforced in `Outrig::launch` rather than in
    config validation, since validation also serves the CLI's legacy sessions, which mount nothing
    there.

- **`ro,z` under SELinux, not `,Z`, as a property of the mount.** `,Z` gives the source a label
  private to one container, and the payload is one cache directory that every concurrent session
  binds. `ContainerMount` gained a crate-private `shared` flag, and `append_bind_mount` picks `z`
  or `Z` from it.
  - `/simplify` moved this choice onto the mount. The first cut had a Python-named field on
    `ContainerLaunchSpec` with its own hand-built `-v`.
  - The sidecar workspace bind has the same hazard and could use the flag. It was left alone.
  - Unmeasured: this machine and CI are both Ubuntu without SELinux. The argv unit test pins the
    rendering.

- **Built for the target architecture**, like `outrig-enter`. What an emulated foreign-arch image
  does with it is recorded as unverified in `plan/next/enter-arch-mismatch.md`.

- **Both architectures pinned; aarch64 so far only by hand.** The aarch64 digest `d6d5838c…ed7b9`
  comes from the release's `SHA256SUMS`.
  - Measured here: the download matched it, and `file` reports
    `ELF 64-bit LSB executable, ARM aarch64, version 1 (SYSV), statically linked`.
  - Not yet built or run on an aarch64 host; CI's arm64 rows will be the first.
  - x86_64 was fetched, verified, embedded, unpacked, and run on this machine, and reports
    3.13.15.

- **The tests.**
  - `tests/python_payload.rs` runs a first launch against an empty cache and a fake podman, then
    runs the unpacked interpreter on the host. The launch having reached podman shows the unpack
    came first.
  - The e2e test in `library_surface.rs` first proves `alpine` has no Python; that probe was
    confirmed to catch a planted `python3` and a planted `/usr/lib/python3.12`. It then runs
    `/outrig/python/bin/python3 -I -c 'print(1)'` and checks the mount is read-only.
  - The build-side checks are unit tests in `archive.rs`, on synthetic tar.zst archives built
    with `ruzstd`'s encoder.

- **After review, five fixes.** An external review of the landed commit found five low-severity
  defects. All five were fixed, and each fix's test was mutation-checked.
  - **`..` aliased the reservation.** `/tmp/../outrig/python/bin` passed `starts_with` while
    podman mounted it over the interpreter. `reject_reserved` now resolves the destination
    lexically first.
  - **Concurrent first launches each unpacked.** The review measured 16 at about 4 GiB peak RSS
    and 2 GiB of temporary disk. The unpack now takes an exclusive `flock` on
    `.<payload>.lock` beside the tree and rechecks under it. The lock is taken inside the
    blocking task, so a cancelled launch cannot release it early, and it serializes separate
    processes as well as threads. The empty lock file stays behind. The test hands the waiter
    bytes that are not an archive, so an unpack it attempted anyway would fail.
  - **A noexec cache made the interpreter unrunnable.** `statvfs` now checks for `ST_NOEXEC`
    before unpacking and on every launch, and the error names the path and `XDG_CACHE_HOME`.
    That the bind inherits `noexec` and rootless podman cannot clear it is the review's finding;
    it was not reproduced here, since no writable noexec filesystem large enough exists without
    root. The test uses `/proc`, which is mounted noexec everywhere.
  - **A read-only download cache defeated the fallback.** `create_dir_all` succeeds on an
    existing read-only directory. The cache is now used only if the staging file can be created
    in it, and `OUT_DIR` otherwise. Measured here: a read-only `downloads` directory with
    `OUTRIG_REQUIRE_PYTHON=1` built, with the archive downloaded into `OUT_DIR`.
  - **The integration test leaked the unpacked tree,** about 170 MB per run, into the temp
    directory. A drop guard now removes it on success and on unwind, leaving about 20 KB, as
    `cancellation.rs` does.

- **A second review found one more.** A download that fell back to `OUT_DIR` was never looked for
  there again. A rerun of the build script then fetched it a second time; offline, that failed
  in strict mode and embedded an empty payload otherwise. `cached_archive` in `archive.rs` now
  checks every place a download can land -- the cache first, then `OUT_DIR` -- against the pin
  before the network is touched.
  - Reproduced with real cargo before the fix, and confirmed fixed after: a read-only cache,
    then a rerun with the proxy pointed at a dead port. It panicked under
    `OUTRIG_REQUIRE_PYTHON`, and after the fix it re-embedded the full archive from `OUT_DIR`.
  - The unit test fails if the lookup stops at the first place.

- **Left for later.**
  - The launcher's musl target is now the one manual step before `cargo build` works:
    `plan/next/musl-target-is-a-manual-step.md`.
  - `python_payload.rs`'s fake `podman`/`buildah` setup is a third copy of a shape in
    `cancellation.rs` and `container_cancellation_e2e.rs`. It was added to
    `plan/next/test-helper-consolidation.md`.
  - Excluding the stdlib's `test` package (36 MB, 1,714 files) would shrink the unpacked tree, but
    it changes what an agent can import. That is `discovery.md`'s call.
  - `OUTRIG_ROOT` and `reject_reserved` sit in `python/payload.rs` while Python is the only thing
    under `/outrig`.

- **`doc/` is unchanged.** The subsystem pages describe the CLI, whose sessions do not mount the
  payload, and `0003-16` owns documenting what `run-new` does. The library's README (which is its
  crate docs), `CHANGELOG.md`, and `CONTRIBUTING.md` carry the change.
