# 0089 -- `outrig-enter`, the static in-sidecar launcher

## Context

A sidecar cannot be given the primary container's filesystem view by any podman flag. The
namespace-sharing flags cover PID, user, network, IPC, UTS, and cgroup; there is no
mount-namespace flag, and `--volumes-from` only re-binds the *host* sources behind the primary's
mounts. The prototype at <https://github.com/tgockel/prototype-podman-shared-fs> closes that gap
by running a launcher as the sidecar's entrypoint, which `setns`es into the primary's mount
namespace and then execs the real server.

The launcher has one hard constraint: it is the first thing that runs inside an image OutRig
does not control, before any graft exists, so it resolves against *that image's* libc. A
glibc-linked launcher aborts with `GLIBC_x.y not found` inside an Alpine sidecar. It must be
statically linked, and it must depend on nothing in the image.

The prototype's `sidecar-enter.c` is the proven version -- 203 lines, syscalls only, verified
against a real `docker.io/mcp/filesystem` (Alpine/musl node 22) serving a Debian/glibc target.
This task ports it to Rust and decides how OutRig ships it. 0090 consumes it; nothing else does.

## Goal

A statically linked launcher binary, produced by `cargo build`, that joins a target mount
namespace, grafts its own rootfs aside so its payload's runtime stays reachable, and execs the
payload -- with no new prerequisite for a default `cargo build` or `cargo install`.

## Deliverables

- A new workspace member `crates/outrig-enter` with a single bin target.
- CLI, matching the prototype so its README stays a usable reference:

  ```
  outrig-enter [--target PID | --ns-file PATH] [--graft DIR] [--cwd DIR] -- PROGRAM [ARGS...]
  ```

  Defaults: `--graft /mnt`, `--cwd /`, `--target 1`.
- Embedding: the built artifact reaches the CLI through `crates/outrig-cli/build.rs`, which
  already exists (it warns on `cuda`/`metal` without `local-llm`).
- Materialization: written into the session directory mode `0755` at sidecar start and
  bind-mounted read-only. 0090 owns the mount; this task owns the bytes and the write.
- No panic-unwinding dependency on the target image: the binary must run correctly with an empty
  environment and an unreadable `/proc`.

## Runtime behavior

Ordering is forced, and getting it wrong fails in ways that read like permission bugs:

1. `open_tree(AT_FDCWD, "/", OPEN_TREE_CLONE | AT_RECURSIVE)` -- snapshot the **sidecar's** root
   *before* leaving its mount namespace. After the `setns` this fd is the only way back to the
   payload's interpreter and libraries.
2. `setns(ns_fd, CLONE_NEWNS)` -- join the target's mount namespace. No user-namespace work
   here: 0090 launches the sidecar with `--userns=container:<primary>`, so the process is
   already in the namespace that owns the target's mount namespace.
3. `unshare(CLONE_NEWNS)` -- take a private copy, so the graft is invisible to the primary.
4. `mount(NULL, "/", NULL, MS_REC | MS_SLAVE, NULL)` -- stop propagation before grafting.
5. `move_mount(tree_fd, "", AT_FDCWD, graft, MOVE_MOUNT_F_EMPTY_PATH)` -- graft the sidecar
   rootfs at `--graft`.
6. `chdir(--cwd)`, then exec the payload.

Payload exec follows the prototype's two rungs, selected by reading the payload's ELF
`PT_INTERP`:

- **No `PT_INTERP` (static).** `execveat(prog_fd, "", argv, environ, AT_EMPTY_PATH)` on an fd
  opened before the switch. Nothing resolves through the target -- no interpreter, no libraries.
- **Dynamic.** Exec the sidecar's own loader under the graft prefix with an explicit
  `--library-path`. Library directories are a fixed list, as in the prototype -- a static
  launcher has no business shelling out to `ldd`. Pass `--inhibit-cache` only when the
  interpreter path does not contain `ld-musl`; musl's loader accepts `--library-path` but not
  `--inhibit-cache`.

Reject shebang scripts explicitly, as the prototype does: the interpreter line would resolve
inside the target and quietly mean something other than what the caller asked for.

Two improvements over the prototype are cheap and worth taking:

- **Architecture.** The prototype hardcodes x86-64 syscall numbers and rejects non-ELF64. Use
  `libc::SYS_open_tree` / `libc::SYS_move_mount` with `libc::syscall`, which the `libc` crate
  defines per architecture, and support aarch64 alongside x86-64.
- **Diagnostics.** Each failure names the step and the errno. The two capability failures in
  particular are indistinguishable from ordinary bugs without help: EPERM from
  `setns(CLONE_NEWNS)` means `CAP_SYS_ADMIN` is missing, and EACCES opening the nsfs file means
  `CAP_SYS_PTRACE` is missing.

## Packaging

This is the decision the task exists to make, and it is constrained: `outrig-cli` declares
`default = []` with `mistralrs` behind the opt-in `local-llm` feature, so a default build needs
no C toolchain today. That must not regress.

**Recommended: build for `<arch>-unknown-linux-musl` and `include_bytes!` the result.** Rust's
musl targets are self-contained -- they ship `libc.a` and link with `rust-lld` -- so
`rustup target add x86_64-unknown-linux-musl` is the only prerequisite, with no C compiler and
no external linker. When the target is not installed, `build.rs` emits a `cargo:warning` and
compiles in an empty artifact, so the build still succeeds and `view = "primary"` fails at
runtime with "this outrig was built without the filesystem-view helper". A `cargo install` user
who never uses the feature is unaffected; one who wants it gets a one-line fix in the error.

Alternatives, to record as rejected rather than unconsidered:

- **`cc` crate compiling the prototype's C.** Reuses proven code, but adds a C toolchain and a
  static libc to the default build. Rejected on that alone.
- **`#![no_std]` freestanding Rust.** No target to install, but a custom `_start` needs
  per-architecture assembly, and linking a static binary on the gnu target still goes through a
  C linker driver by default -- so it does not actually remove the toolchain dependency.
- **Building the helper with buildah on first use.** Zero build-time prerequisites and fits
  OutRig's existing image build-and-cache idiom, but a several-hundred-megabyte compiler image
  pull the first time a user tries the feature is a worse first impression than a `rustup`
  hint.

Nested `cargo build` from `build.rs` is the known-fragile part of the recommendation (lock
contention, `CARGO_ENCODED_RUSTFLAGS` leaking into the child). If it proves unworkable, fall
back to making the helper a normal workspace member built by CI for release artifacts, and keep
the runtime "built without the helper" path as the `cargo install` story.

## Security

The launcher runs with `CAP_SYS_ADMIN` and `CAP_SYS_PTRACE` inside the rootless user namespace
(0090 grants them). It should hold them for as little as possible: it does no privileged work
after step 6, and exec into the payload is the last thing it does. It must not read, log, or
copy anything from the namespaces it joins -- it is a shim, not a tool.

`--graft` must name a directory that already exists in the sidecar image. Creating it would
write through to the target container's real filesystem. `/mnt` is the default because it is
conventionally present and empty; fail with that explanation rather than calling `mkdir`.

## Acceptance

- `cargo build` on a machine with the musl target produces a binary that `file` reports as
  statically linked, and `cargo build` without the target succeeds with a warning.
- Run by hand against an OutRig-launched session container, reproducing the prototype's checks
  \#11 and #12:
  - an Alpine sidecar sees a Debian primary's rootfs, including a path no bind mount provides
    (the prototype uses `/usr/local/cargo/bin/cargo`);
  - unmodified `docker.io/mcp/filesystem` completes an MCP handshake over stdio and lists files
    from the primary's workspace path.
- Both negative tests reproduce with the diagnostics above: dropping `--cap-add=SYS_ADMIN` fails
  at `setns` with EPERM, and dropping `--cap-add=SYS_PTRACE` fails opening the nsfs file with
  EACCES.
- The graft is invisible to the primary: `podman exec <primary> ls -A /mnt` is empty.
- A shebang script as the payload is rejected with a message naming the reason.

## Open questions

- **Whether the helper should ever run on the host.** The prototype's `enterfs.py` does the same
  thing from outside any container, which would let OutRig run a host-side MCP server with the
  primary's view. There is no consumer today -- OutRig ships no filesystem MCP server of its own
  -- and the host-process form loses the container's sandbox entirely. Out of scope; the
  prototype's README documents the tradeoff if it ever comes up.
- **`--host-bind` equivalent.** The prototype needs it because Debian's `node` loads runtime
  assets by absolute path and silently picks up the *target's* copy after the graft. A
  self-contained runtime avoids it. Leave the flag out until a real image needs it, but keep the
  hazard in mind when writing 0090's docs -- silently loading the wrong JavaScript is worse than
  failing.
- **Whether to vendor the prototype's C alongside the Rust port** as a reference for review. It
  would go stale. Cite the URL instead.

## Dependencies

None.

## See also

- <https://github.com/tgockel/prototype-podman-shared-fs> -- `sidecar-enter.c` is the reference
  implementation; the README's "Gotchas found the hard way" is the list of things that cost time.
- `plan/todo/0090-primary-view-sidecars.md` -- the only consumer.

## Decisions

- **Packaging deviates from the "recommended" plan above.** The launcher is *not* a separate
  `crates/outrig-enter` workspace member built by a nested `cargo build -p ... --target musl`.
  That design cannot satisfy a hard requirement: `cargo install outrig-cli` from crates.io must
  be able to build the helper (no prebuilt binaries allowed). A sibling crate is absent from the
  published tarball (`cargo package` ships only the package dir), so `build.rs` would have
  nothing to compile. Instead the launcher source ships *inside* a published crate and `build.rs`
  compiles it with a **direct `rustc --target <arch>-unknown-linux-musl`** on a single file --
  no `cargo`, no dependency resolution, no package-cache/workspace lock (which also removes the
  nested-cargo fragility the plan flagged). Spike + the acceptance test confirm this yields a
  `static-pie` musl binary with no `PT_INTERP`.
- **Lives in the `outrig` library crate, not `outrig-cli`.** The plan said `outrig-cli/build.rs`,
  but the only consumer -- 0090's sidecar wiring -- is in `crates/outrig/src/container/sidecar.rs`,
  so the bytes + `materialize` belong beside it under `crates/outrig/src/container/enter/`. A new
  `crates/outrig/build.rs` hosts the compile+embed (`include_bytes!` must be in the crate that
  owns the `build.rs`). `outrig` is also published, so the crates.io story holds.
- **The launcher depends on nothing.** It self-declares the `extern "C"` musl symbols
  (`setns`/`unshare`/`mount`/...) and the arch-specific syscall numbers (`SYS_execveat` 322 on
  x86_64 / 281 on aarch64; `open_tree`/`move_mount` 428/429 shared), so no `libc` crate and no
  workspace/`Cargo.toml` changes. Variadic `syscall()` args are cast to `c_long` to avoid
  vararg-width UB. aarch64 supported alongside x86_64 via `cfg(target_arch)`.
- **Graft only in the dynamic branch** (faithful to the prototype): a static payload needs
  nothing from either rootfs, so it `execveat`s its fd directly after `setns`; only a dynamic
  payload triggers `open_tree`/`unshare`/`MS_SLAVE`/`move_mount`.
- **Single-source ELF parser.** `elf.rs` is the truth: compiled as `#[cfg(test)] mod elf` for
  host unit tests, and `include!`d by `launcher.rs` for the musl build. Its header must be plain
  `//` comments (inner `//!` docs are illegal mid-file); this is documented in the file.
- **Stripped** (`-C strip=symbols`): 4.5 MB -> 463 KB embedded.
- **Graceful degradation.** No musl target -> `build.rs` warns and embeds an empty artifact;
  `is_available()` is false and `materialize` returns `OutrigError::FilesystemHelperUnavailable`
  with a `rustup target add` hint. `build.rs` emits `rerun-if-changed` for the target's sysroot
  lib dir (named by `rustc --print target-libdir` even when absent), so a later `rustup target
  add` auto-triggers a rebuild -- verified by removing and re-adding the target.
- **Container-level acceptance is manual / exercised by 0090.** 0089 alone cannot launch a
  sidecar, so the setns/graft/MCP-handshake and the SYS_ADMIN/SYS_PTRACE negative cases are run
  by hand against a live session (per the prototype's `21-`/`22-` scripts). Automated coverage:
  static-ELF embedding + `0755` materialization + the ELF/shebang parser.
- **No `doc/` changes:** the helper is internal; the user-facing `view = "primary"` surface lands
  with 0090.
