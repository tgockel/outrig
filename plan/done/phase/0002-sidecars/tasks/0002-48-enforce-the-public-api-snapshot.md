# 0002-48 -- Gate the public-API snapshots instead of trusting the honor system

## Context

`crates/outrig/public-api.txt` and `crates/outrig-cli/public-api.txt` are the artifact the whole
0002-16 through 0002-18 hardening arc is measured against, and nothing enforces them. Their header
says "Regenerate after any intentional surface change and review the diff"; 0002-16's Decision 14
deliberately added no CI job, on the grounds that rustdoc's JSON format shifts between nightlies and
would break CI on tool churn rather than on real changes.

The cost showed up in 0002-18. Regenerating the snapshot dropped 87 lines that were never a surface
change: 0002-17 had left the whole `#[non_exhaustive]` block in the file twice -- once as a leading
block above the `pub mod outrig` header, once in its sorted position. It survived a full task and
was noticed only because a later task happened to regenerate the file.

The 0.2.0 audit measured the same rot again, from the other direction: regenerating found six
`std::io` versus `core::io` rendering differences. Those are not real breaks -- and that is the
finding. The snapshot cannot be assumed current, so a genuine break hiding among rendering noise
would not stand out. A snapshot nobody regenerates is a snapshot that silently rots, which is the
failure mode it exists to prevent, and 0.2.0 is the release these files are supposed to be the
record of.

## Goal

A surface change that forgets to regenerate the snapshot fails something, without CI breaking
every time nightly's rustdoc JSON moves.

## Deliverables

- **The pinned toolchain lives in exactly one place.** Today it is prose in two file headers, so
  a regeneration on a different `cargo public-api` or a different nightly is indistinguishable
  from a real diff -- which is how the six `std::io` lines got there. Pin both the tool version
  and the rustdoc/nightly, once, somewhere the check reads.
- **Something mechanical invokes it.** A script mentioned only in `RELEASING.md` fails nothing
  unless a human remembers to run it -- that is a better command, not enforcement, and the
  failure mode being fixed is precisely that nobody remembered. Once the exact nightly and the
  exact `cargo-public-api` version are pinned, the nightly-churn objection no longer applies,
  because the job no longer floats. So: a pinned-toolchain PR CI job, or an enforced release
  workflow that cannot be skipped. Fork 1 picks which.
- **The invocation is machine-readable configuration, not prose.** Exact nightly date,
  `cargo-public-api` version and how it is installed, target triple, and feature set, all in one
  file the job and a developer both read. Generate into a temporary file and diff against the
  checked-in one, so a failed run does not leave the tree dirty.
- **`RELEASING.md` gains the step** regardless, before the publish dry-run -- the snapshot has to
  be true at the moment a version is cut even if CI already checked it.
- **Regenerate both snapshots as part of this task**, on the newly pinned toolchain, so the
  checked-in files are the pinned tool's output rather than an older one's. Review the diff: the
  surface moves from 0002-41 through 0002-47 land in it, and the six rendering differences should
  disappear as noise rather than be committed as changes.

## Acceptance

- Deleting a `pub fn` from the library makes the check fail, naming the item. **Adding** one does
  too -- an additive surface change is still a surface change, and a check that only catches
  removals lets the snapshot drift in the direction it actually drifts.
- The check runs from a clean environment through its real entry point, with no pinned tooling
  preinstalled, so "works on a machine that already had it" is not the thing being verified.
- Running the check twice on an unmodified tree produces no diff -- the determinism claim the
  pinning exists to make.
- `RELEASING.md`'s checklist includes the step, positioned so a failure is caught before
  anything is published.
- The two `public-api.txt` headers no longer carry the pinned version as prose, or carry it as a
  pointer to the single source.

## Design forks

1. **Pinned PR job versus enforced release job -- Open, but one of them must exist.** A PR job
   catches the omission at the moment it is made and costs a nightly toolchain install on every
   run; because the nightly is pinned to a date, it breaks only when someone bumps that date
   deliberately, which is the churn 0002-16's Decision 14 was worried about and is no longer
   accidental. A release-workflow job is cheaper and lets a wrong snapshot live on `trunk`
   between releases. What is not on the table is a script nothing invokes.

2. **Whether a local opt-in test is also worth it -- Recommended: yes, cheap.** A test behind an
   off-by-default `surface-snapshot` feature lets a developer check their own work mid-task
   without pushing. It shares the pinned configuration, so it is a second entry point rather
   than a second source of truth.

## The downstream surface test

Gate item 17 asks for two things: snapshot enforcement *and* downstream-style compile and runtime
surface tests. This task owns the first. The second is
`plan/next/container-surface-test.md` -- a test that drives `container`/`image`/`mcp_proxy`/
`network` the way an external consumer does, so a breaking change fails a test rather than a
downstream build.

**Decided: deferred, as an accepted release exception.** 0.2.0 ships snapshot enforcement without
the runtime-core surface test. The rationale is that the surface is not untested, only untested *as
a whole*: 0002-41, 0002-42, and 0002-45 each add external, out-of-crate tests against the parts they
change, and 0002-47's sealing test pins the trait boundary. What is genuinely uncovered is the
composed path -- acquire an image, start a container from a spec, exec a server over it, aggregate
through `ProxyServer`, tear down -- driven the way a downstream crate drives it. A snapshot proves
that shape did not move; it does not prove the shape is still usable, and 0002-18's churn is already
in the tree waiting for something to exercise it.

That is a real gap and it is being accepted, not closed. Two consequences follow, and both are
obligations on this task rather than notes:

- **This task records the exception in its `## Decisions`**, with the reasoning above, so the
  waiver is recoverable later.
- **0002-54's release record says gate item 17 was *partially* met**, not met. If the final notes
  claim the release gate was completed in full, this decision has been quietly reversed.

## Dependencies

- **Soft: after 0002-41 through 0002-47.** Regenerating before the pre-freeze surface changes land
  means doing it twice. If this task is taken earlier, the regeneration step moves to whichever
  surface task lands last.

## See also

- `crates/outrig/public-api.txt`, `crates/outrig-cli/public-api.txt` -- the artifacts and their
  headers.
- `plan/done/phase/0002-sidecars/tasks/0002-16-shrink-reachable-surface.md` (Decision 14, the
  no-CI-job call this revisits),
  `plan/done/phase/0002-sidecars/tasks/0002-17-non-exhaustive-sweep.md` (where the duplicate block
  came from), `plan/done/phase/0002-sidecars/tasks/0002-18-options-structs-and-sealing.md` (where it
  was found).
- `scripts/audit-doc-style.py` -- the shape a `scripts/` entry would follow.

## Decisions

1. **Fork 1 -- a per-PR CI job.** The phase README's exit criterion reads "regenerated and
   enforced in CI" flatly, and this repo has no release workflow at all: publishing is manual
   through `RELEASING.md`, so "an enforced release workflow that cannot be skipped" would have
   meant inventing one. Decision 14's churn objection is answered by the pin rather than
   argued with -- the job no longer floats, so it breaks when someone bumps a pin deliberately.

2. **Fork 2 -- no `surface-snapshot` cargo feature; the script plus `CONTRIBUTING.md`.** The
   fork recommended a feature-gated test, and this is a deviation. A Rust test would have
   shelled out to a repo-root Python script, which inverts the dependency: `scripts/` is not in
   either crate's package, so the test could not run from a published or vendored crate and
   would need a third `exclude` entry to stay merely inert. It would also add an undeclared
   `python3` requirement to `cargo test --workspace`. Fork 2's actual ask -- a developer checks
   their own work mid-task without pushing -- is `python3 scripts/check-public-api.py`, which
   `CONTRIBUTING.md` now documents. A cargo feature wrapping a Python script wrapping a tool is
   three entry points to one check.

3. **The regression the fork wanted lives in the script, as `--self-test`.** It mutates the
   committed snapshot in memory -- one real `pub fn` line removed, one synthetic line added --
   and asserts the comparison rejects both and names them, driving the same `compare()` the
   real check uses rather than a parallel copy. It needs no toolchain, no tool and no network,
   so it runs as its own CI step, last, and a clean diff is never the only evidence that the
   comparison is capable of failing.

4. **`tool-version` is not a pin; `locked = true` is.** `cargo-public-api 0.52.0` (the newest
   release) depends on `public-api ^0.52.0`. That crate's 0.52.0 and 0.52.1 require
   `rustdoc-types ^0.57.3`; its 0.52.2, published 2026-09-12, requires `^0.59.0`. A
   `rustdoc-types` major *is* rustdoc JSON's `format_version`, and 57 and 59 are disjoint --
   so a locked and an unlocked install of one `cargo-public-api` version accept non-overlapping
   sets of nightlies while both reporting `cargo-public-api 0.52.0`. Two consequences: the
   tool's `--version` can never be the identity check, and `tool-version` and `toolchain` are
   one decision rather than two. Locked 0.52.0 speaks format 57, which nightly emitted from
   2025-11-22 (`rustdoc-types` 0.57.0) to 2026-06-24 (0.58.0); `nightly-2026-05-06` sits inside
   that window and was verified to emit 57 before anything else was written.

5. **The pin fails open unless something asserts it, so `toolchain-rustc` is recorded.**
   `cargo-public-api` selects rustdoc through `RUSTUP_TOOLCHAIN` and otherwise falls back to
   whatever the `nightly` alias points at. A named-only pin would therefore be silently
   ignored on any machine whose `nightly` had moved. The script sets the variable and
   pre-flights `rustup run <toolchain> rustc --version` against the recorded string, and
   installs the toolchain explicitly under `--install-missing` -- `RUSTUP_TOOLCHAIN` naming an
   absent toolchain makes rustup auto-install it mid-build with the default profile.

6. **No *pinned* target triple -- but `--target` is passed, resolved at run time.** Superseded
   in part; see the review pass below. The original call: `crates/outrig/src/lib.rs` refuses to
   compile off Linux and no public item is gated on `target_arch`, so a pinned triple would
   change nothing in the rendered surface. It would cost: on the `ubuntu-24.04-arm` runner,
   `--target x86_64-unknown-linux-gnu` would cross-build the whole graph, C dependencies
   included, for no benefit. A `host-os = "linux"` assertion carries the intent instead.

7. **The tool is installed into a version-stamped root under `$XDG_CACHE_HOME`.** Not
   `~/.cargo/bin`, because the same script runs on a developer's machine, where replacing a
   global install is not ours to do -- and cargo searches `$CARGO_HOME/bin` before `$PATH`, so
   a `PATH` prepend would not reliably win anyway; the binary is invoked by absolute path. Not
   under `target/`, because `Swatinem/rust-cache` deletes loose files anywhere beneath the
   target directory before saving, which would recompile the tool on every CI run. Encoding
   the pin in the directory name is what makes "is this the pinned build?" answerable at all,
   given Decision 4, and makes a bumped pin a cache miss by construction.

8. **Generated in memory rather than into a temporary file.** The deliverable asked for a temp
   file "so a failed run does not leave the tree dirty"; holding the text in memory satisfies
   that strictly, and the full unified diff is printed, which is more useful than a temporary
   file that is deleted. Every crate is generated before any is compared or written, so a
   fault part way through cannot half-write the tree and a two-crate drift is one run.

9. **Exit 1 for a surface difference, 2 for a tooling fault.** This is the mechanical form of
   Decision 14's worry. A rustdoc JSON format mismatch, a missing toolchain and a failed
   `cargo install` all exit 2 with a message that says so in words; only a real diff exits 1.
   A red job says which it is before anyone starts reading.

10. **The `rmcp` boundary rule stays in `tests/public_api_boundary.rs`.** That file predicted
    this task would "fold the assertion into whatever it generates". It should not be: now
    that the snapshot is enforced current, reading the committed file *is* reading fresh data,
    and the Rust test runs on every `cargo test` without a nightly, where a Python copy would
    run only where the pinned toolchain exists. Restating the rule would create the second
    copy it exists to prevent. Only its doc comment changed.

11. **The regeneration moved no item.** The diff is the new header, the seven
    `core::io::error::Error` to `std::io::error::Error` renderings the 0.2.0 audit flagged as
    noise, and the stray blank line `crates/outrig-cli/public-api.txt` carried since creation.
    Nothing from 0002-41 through 0002-47 appears, which is the evidence that those tasks did
    regenerate: the committed content was current, and only the nightly that rendered it had
    drifted. The pinned nightly is older than the one last used by hand, so the flip is toward
    `std::io`; what matters is that it is now fixed rather than whichever way it landed.

12. **`RELEASING.md` cross-references name step titles, not ordinals.** The new step renumbers
    old 4-9 to 5-10, and 0002-52 plans two more insertions at the same place, so by-ordinal
    references would need rewriting twice more. Every checklist item already carries a bold
    title, so the references in `## Pre-releases`, the closing paragraph, and
    `plan/todo/0002-52` and `0002-54` now name those instead. Ordinals survive only inside the
    markdown list. That is the half this does not fix -- the ordinals there are literal, so
    0002-52's two planned insertions still cost a hand-renumber, and a renamed title would rot
    eight references silently. `plan/next/releasing-steps-are-referenced-by-title.md` carries
    the rest. A title is still strictly better than an ordinal, which rots on every insertion
    rather than only on a rename. This also makes `0002-51`'s instruction to "regenerate the
    snapshot with the command `RELEASING.md` documents" true; it had no referent before.

13. **The downstream runtime-core surface test is deferred, as an accepted release exception.**
    Gate item 17 asked for snapshot enforcement *and* downstream-style compile and runtime
    surface tests. This task delivers the first. The second,
    `plan/next/container-surface-test.md`, is not being done for 0.2.0. The surface is not
    untested, only untested *as a whole*: 0002-41, 0002-42 and 0002-45 each add external,
    out-of-crate tests against the parts they change, and 0002-47's sealing test pins the trait
    boundary. What is genuinely uncovered is the composed path -- acquire an image, start a
    container from a spec, exec a server over it, aggregate through `ProxyServer`, tear down --
    driven the way a downstream crate drives it. A snapshot proves that shape did not move; it
    does not prove the shape is still usable. That is a real gap, accepted rather than closed,
    and `0002-54`'s release record must therefore state gate item 17 as **partially** met. A
    note claiming the 18-item gate was completed in full would quietly reverse this.

## Decisions from the /simplify pass

1. **The "run it twice" CI step was vacuous, and is gone.** The job ran the check a second time
   to test the determinism the pinning claims. On a warm `target/public-api` the doc-unit
   fingerprints are fresh from the first run, so cargo skips rustdoc entirely and the tool
   re-reads the bytes rustdoc already wrote -- the second run cannot observe nondeterminism in
   the thing it was watching. The acceptance criterion is met better by what run 1 already
   does: it compares a freshly generated surface against a file generated on another machine,
   on another day, by another cargo invocation. Verified locally instead, once from an empty
   `target/public-api` and once warm, byte-identical both times.

2. **`EXIT_DIFF` is returned from exactly one place.** The 0/1/2 split was enforced by
   enumerating the two exception types that had been anticipated, so everything else -- a
   mistyped key in the pin table, a `Cargo.toml` that will not parse, a snapshot file that has
   gone missing -- reached Python's default exit status, which is 1, and reported a broken tool
   as "the surface differs". Editing the pin table wrongly is the most likely way this check
   fails in its first year, so that was the wrong default. `main` now wraps everything and
   returns `EXIT_ENV` for anything that is not a computed diff, which makes the contract
   structural rather than a promise.

3. **`rustup toolchain install` failing no longer advises `--install-missing`.** The two
   pre-flights had opposite branch orders: `ensure_tool` guarded before installing,
   `ensure_toolchain` after. So a failed toolchain install under `--install-missing` told the
   user to re-run with the flag they had just passed. Both now guard first.

4. **`CARGO_PROFILE_DEV_DEBUG=0` on the generating child.** rustdoc reads rmeta, so nothing in
   this path ever reads the debug info the dev profile emits -- but the parts that do get
   codegen'd (23 proc-macro `.so`s, the syn rlibs, 183 MB of build-script output) carry it, and
   CI compresses, uploads and restores all of it. Measured: `target/public-api` falls from 737
   MB to 461 MB, with byte-identical snapshots from a cold rebuild.

5. **`Swatinem/rust-cache` had to be told which tree to cache.** The script sets
   `CARGO_TARGET_DIR` on the child it spawns, not on the job, so the action's default
   `. -> target` would have keyed and pruned the wrong directory and rebuilt the nightly's
   artifacts every run -- a cache step whose comment described a saving it never got.
   `workspaces: ". -> target/public-api"` names it.

6. **Declined: replacing `toolchain-rustc` with `rustdoc-format-version = 57`.** The argument
   for it is good -- format 57 is the axis Decision 4 identifies as the real one, and it is the
   one fact not derivable from either pin's name. It was declined because `cargo-public-api`
   already fails loudly on a format mismatch and the script maps that to exit 2 with a message
   naming both pins, so the check would be asserting preemptively what the tool enforces
   anyway. `toolchain-rustc` is kept for the case a dated name does not cover: a
   `rustup toolchain link` shadowing it. Its error text now says so instead of conceding
   redundancy.

7. **Declined: a `scripts/tests/` home for `--self-test`.** A test mode inside the production
   script is a special case, and a real test directory would give `audit-doc-style.py` the same
   home. But it is a new CI step and a new convention for one test, in a repo with two scripts,
   and fork 2 had already decided against multiplying entry points. Its exit code was the real
   defect and is fixed: a broken comparator is a tooling fault, so it exits 2, not 1.

8. **Kept as-is: the tool cache key, and the 8-line `report()` duplicated from
   `audit-doc-style.py`.** `hashFiles('Cargo.toml')` churns on any dependency bump, but
   `restore-keys` restores the newest prefix match and the version-stamped directory inside
   still matches, so no `cargo install` runs; keying on the pin exactly would need a
   `--print-pin` mode with no other consumer. Extracting `report()` would turn two
   deliberately standalone, stdlib-only scripts into a mini-package to save eight `print()`
   lines.

## Decisions from the review pass

1. **A configured cargo target made the gate pass on stale data. Fixed by pinning `--target`
   to the pinned toolchain's own host triple.** Decision 6 declined a target triple on the
   grounds that no public item is gated on `target_arch`, which is true and is why the
   rendering does not change -- but it missed that the flag is load-bearing for *which file
   gets read*. With `CARGO_BUILD_TARGET` set, or `build.target` in any cargo config, cargo
   writes the rustdoc JSON to `target/public-api/<triple>/doc/` while `rustdoc-json` computes
   the unqualified `target/public-api/doc/` to read back, because no `--target` was passed.
   On a warm tree it therefore reads the *previous* run's JSON.

   Reproduced before fixing: with a real `pub fn stale_probe()` added to
   `outrig::config::ConfigSource`, `CARGO_BUILD_TARGET=x86_64-unknown-linux-gnu` and a warm
   tree, the check reported `crates/outrig/public-api.txt: OK` and exited 0. The fresh JSON
   under `x86_64-unknown-linux-gnu/doc/` contained `stale_probe`; the unqualified file it
   actually read, from 52 minutes earlier, did not. That is the exact failure this task exists
   to prevent, arrived at from the other side -- not a stale snapshot, but a stale *surface*.

   The triple is resolved from `rustup run <toolchain> rustc -vV`, not written into the pin
   table, so it is native on every host and Decision 6's real objection -- that a hardcoded
   `x86_64-unknown-linux-gnu` would cross-build the graph on the aarch64 runner -- still
   stands. Clearing `CARGO_BUILD_TARGET` from the child's environment was rejected as the fix:
   it does not cover `build.target` in a config file, and an explicit `--target` outranks both.
   Verified after the change: the same scenario now names the added item and exits 1, and a
   cold run against the committed snapshots is byte-identical, so passing `--target` costs no
   rendering churn.

2. **The rust-cache key now hashes the root manifest.** `Swatinem/rust-cache` keys on the
   default toolchain's `rustc -vV` -- the stable installed by the step above, not the pinned
   nightly that builds every artifact in the tree -- and on the member manifests and the
   lockfile, none of which is the virtual root `Cargo.toml` where the pins live. A nightly bump
   alone would leave the key unchanged, so the job would restore the old compiler's artifacts,
   rebuild them, and then skip saving because the lookup was an exact hit, on every run until
   some unrelated input moved. `key: ${{ hashFiles('Cargo.toml') }}` gives a pin bump a cache
   to populate.

3. **A too-old Python exited 1 rather than 2.** `tomllib` is 3.11+, and an `ImportError` fires
   at module load, before `main` can classify anything, so Python's default status -- 1, the
   code this script reserves for "the surface differs" -- was what an Ubuntu 22.04 host got
   from a missing TOML parser. The version is now checked before the import and exits 2, and
   `CONTRIBUTING.md` states the minimum, which is newer than `scripts/audit-doc-style.py`
   needs and so is not implied by the existing `python3` invocations.
