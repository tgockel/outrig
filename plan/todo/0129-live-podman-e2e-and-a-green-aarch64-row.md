# 0129 -- Run the e2e suite for real, on both architectures

## Context

0092 existed because a whole feature-gated test suite had rotted unnoticed: `e2e` was declared on
both crates and no CI job compiled it. 0092 added a matrix row, and that row runs `cargo test
--no-run`. So the suite compiles and links; it has never been executed against a live podman in
CI.

The 0.2.0 audit inherited that limit and said so. Among the things it explicitly does **not**
claim: live podman e2e execution (compiled only, as in CI), sanitizer and Miri cleanliness,
native AArch64 runtime and syscall behavior, and CUDA/Metal runtime coverage.

That matters more for this release than usual, because the queue ahead of it changes exactly the
code a compile-only suite cannot exercise: 0116 changes subprocess ownership and kill semantics,
0117 changes namespace-entering rollback and nft teardown, and 0114 changes what the interceptor
lets through. Every one of those is a runtime property. `--no-run` proves none of them.

AArch64 is the second half. outrig's container work is syscall- and namespace-heavy -- `nsfork`,
`container/enter`, the nft rules -- and none of it has a green native run on ARM. Claiming both
architectures are ready without one is a claim nobody has checked.

## Goal

A live podman e2e run on x86-64 and a green native AArch64 row, both before 0.2.0 is described as
ready on either architecture.

## Deliverables

- **One standard, applied to both architectures.** Fork 1 picks whether the evidence is a CI row
  or a recorded manual run; whichever it is, x86-64 and native AArch64 produce the *same* kind of
  evidence. The earlier draft alternated between "CI row", "CI run", and an acceptable one-off
  manual ARM run, which makes the acceptance criteria unfalsifiable.
- **A live e2e run on x86-64**: podman available, `cargo test --features e2e` actually run rather
  than `--no-run`. Expect to fix what it finds; a suite that has never run is a suite with unknown
  failures, and budget for that rather than treating a red first run as a blocker discovered late.
- **A native AArch64 run.** Native, not emulated -- the point is syscall and namespace behavior,
  which is what emulation is worst at.
- **One exact invocation, on both architectures.** Both crates declare `e2e`, so the feature has
  to be named per package or one crate's suite silently stays out:

  ```sh
  cargo test --workspace --locked --features outrig/e2e,outrig-cli/e2e
  ```

  Recorded verbatim, so "we ran e2e" cannot mean two different things on the two machines.
- **Durable evidence**: test counts, per-test names, the environment (podman version, kernel,
  arch), and an explicit list of anything skipped and why.
- **Whichever findings are real, fixed or filed.** Anything that is an environment artifact gets
  written down in the job, not silently retried.
- **The claim, once earned.** `README.md` / `doc/` say which architectures have a green live run.
  Until then they should not imply both.
- **Do not fold in the rest of `plan/next/ci-configuration-coverage.md`.** Its `cargo hack
  --each-feature` job, its cache-bucket and sccache cleanups, and its MSRV check are independent
  and stay in the buffer; its `macos-latest` x `local-llm,metal` item is contingent on 0123's
  decision and may evaporate. Cross-reference both ways so neither is done twice.

## Acceptance

- Evidence that the e2e suite **executed against a live Podman on each architecture** -- x86-64
  and native AArch64 both, with test names and pass counts, not a link step. `--no-run` anywhere
  in the live invocation is a failure of this task, and so is an ARM row that only compiles.
- Both architectures ran the exact invocation above and produce the same evidence shape, per
  fork 1's choice.
- **The lifecycle regressions from 0116 and 0117 run under live podman**, not only against fakes.
  Fakes prove the ownership logic; podman proves the container actually went away, that no
  buildah working container or temporary tag survived a canceled build, and that a `detach`
  really ended its bridges.
- **0114's live tier runs here too**: a forged `Host`/SNI connection is denied against a real
  interceptor, and a container that resolved the name through outrig's own DNS is allowed. Those
  are the two halves of the security fix that policy-level tests cannot reach, and omitting them
  would leave the release's headline blocker verified only in unit tests.
- Anything skipped is listed with a reason; a silent skip reads as a pass.
- Nothing in the repo claims an architecture is ready that does not have this evidence.

## Design forks

1. **CI rows versus recorded manual runs -- Open, but pick one for both architectures.** GitHub's
   ARM runners or a self-hosted box give a repeatable row that protects the *next* release and
   cost setup. A recorded manual run is cheap, satisfies this gate, and protects nothing
   afterward. Mixing them -- a CI row on x86-64 and a manual run on ARM -- is defensible only if
   the asymmetry is written down as a deliberate choice with an expiry, rather than arrived at
   because ARM was harder.
2. **Whether the live job gates every PR -- Recommended: no.** Live podman is slow and flaky in
   shared CI. A scheduled run plus a release-time run gets the coverage without the per-PR tax,
   matching 0125's posture on the snapshot gate.

## Dependencies

- **Soft: after 0116 and 0117**, whose regressions this is meant to exercise for real.
- Independent of 0128; it can run against the rc.3 tree or before it, but the release should not
  be described as ready on an architecture until this lands.

## See also

- `crates/outrig/src/nsfork.rs`, `crates/outrig/src/container/enter/`,
  `crates/outrig/src/network.rs` -- the syscall- and namespace-heavy code an AArch64 row exists
  to exercise, and which `--no-run` proves nothing about.
- `crates/outrig/tests/library_surface.rs`, `crates/outrig-cli/tests/e2e_quickstart.rs`,
  `crates/outrig-cli/tests/primary_view_e2e.rs` -- the `e2e`-gated suites that have never run.
- `plan/done/0092-e2e-imageconfig-sidecars-bitrot.md` -- where the `--no-run` row came from, and
  why the class of gap it left is still open.
- `plan/next/ci-configuration-coverage.md` -- the sibling entry, deliberately not absorbed.
- `.github/workflows/ci.yml` -- the matrix this extends.
