# Diagnose a sidecar image whose architecture is not the launcher's

## Context

`outrig-enter` is compiled for the architecture of the machine that built OutRig
(`build.rs:41-56` keys off `CARGO_CFG_TARGET_ARCH`), and it carries that architecture in two
places that matter at runtime: the syscall numbers at `src/container/enter/launcher.rs:95-111`,
and `MULTIARCH`, which `lib_dirs()` (`launcher.rs:116-126`) expands into the loader search path
as `/lib/<multiarch>` and `/usr/lib/<multiarch>`.

Nothing checks that the sidecar image agrees. `elf_interp`
(`src/container/enter/elf.rs:66-69`) validates the ELF magic and `ELFCLASS64` and then goes
straight to the program headers; `e_machine` -- a `u16` at offset 18 of the ELF64 header, 62 for
x86-64 and 183 for AArch64 -- is never read. No `podman` or `buildah` invocation anywhere in the
crate passes `--platform`, so the image's architecture is whatever the registry served.

So an emulated foreign-arch sidecar parses as a perfectly good ELF64, and the failure surfaces
much later and in the wrong vocabulary: the launcher hands the payload's `PT_INTERP` an
`--library-path` built from `aarch64-linux-gnu` directories that an amd64 rootfs does not have,
and the user sees a missing-loader error about a path they never configured. `build.rs:86-93`
already cites the same number from the other side -- "Relocations in generic ELF (EM: 183)" is
what the host linker says when it is handed foreign-arch objects.

## Why it might matter

podman will happily run a foreign-arch image under `qemu-user` binfmt, and multi-arch registry
tags make it easy to pull one without noticing -- particularly on AArch64, where a good number
of MCP server images are still amd64-only. This is the failure a user is most likely to hit
without any way to guess the cause.

## Goal

A sidecar image whose architecture does not match the embedded launcher fails with a message
naming both architectures, before anything harder to read goes wrong.

## Deliverables

- **`e_machine` validation in `elf.rs`.** Read offset 18, compare against the launcher's own
  architecture, and return a new `ElfError` variant carrying both values. `elf.rs` is the single
  source of truth (`include!`d by `launcher.rs`, compiled as `cfg(test) mod elf` for host unit
  tests), so the test lands beside the existing `ELFCLASS32` case.
- **A `die()` message** naming the image's architecture and the helper's, in the style
  `launcher.rs:130` already uses for step plus errno.
- **A decision on failing earlier** -- see the forks.
- **Tests.** A synthetic header with a foreign `e_machine`, alongside the existing truncation and
  `ELFCLASS32` cases in `elf.rs`.

## Acceptance

- A `view = "primary"` sidecar on a foreign-arch image fails with a message naming both
  architectures rather than a missing-loader path.
- `cargo test -p outrig` covers the new `ElfError` variant.
- No behavior change for a matching image: the check is a comparison, not a new syscall.

## Design forks

1. **Where the check fires -- Open.** Host-side at validation time is the better message and
   happens before the container is created: `podman image inspect --format {{.Architecture}}`
   is already the shape `image.rs:578-586` uses. Launcher-side is the backstop that cannot be
   bypassed and costs two bytes of parsing. Doing both is defensible; doing only the host-side
   one leaves the launcher trusting input it can cheaply check.

2. **Diagnose or support -- Open.** The cheap answer is to refuse. The expensive one is to make
   cross-arch sidecars work by embedding a launcher per architecture and selecting at
   materialization time. That doubles the embedded payload (463 KB stripped, per
   `plan/done/0089-outrig-enter-helper.md`) and requires both musl targets installed to build,
   which `CONTRIBUTING.md` currently asks for only one of. Refusing first is not a decision
   against this; it is what makes the failure legible enough to judge whether anyone wants it.

3. **Whether `--platform` should be passed at all -- Open.** Pinning it on `podman create` would
   make the mismatch impossible rather than merely diagnosed, but it also overrides a
   deliberately-pulled emulated image. Interacts with `plan/next/user-image-library.md`.

## Dependencies

- None. Independent of the host-platform entries, though
  `plan/next/windows-host-support.md` and `plan/next/macos-host-support.md` both make
  foreign-arch images likelier by putting the engine in a VM.
