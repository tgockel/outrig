# 0043 -- Wire `--verbose` global flag

## Goal

`doc/reference/cli.md` and `doc/usage/run.md` document a `--verbose` global flag
that is currently design-only -- both pages now carry a `TODO: Incomplete` note
saying so. Land the flag and drop the TODO markers.

The promised behavior is "adds buildah/podman command transcripts to stderr and
to `<session_dir>/logs/container.log` for `outrig run`. It does not change
behavior." Repeat-arg counting (`-v -v` / `--verbose --verbose`) escalates to
trace level.

## Deliverables

- Add `verbose: u8` (clap `action = ArgAction::Count`, `short = 'v'`,
  `long = "verbose"`, `global = true`) to `Cli` in `src/bin/outrig.rs`. Pass
  the count down to the subcommand handlers that care about it.
- Thread the verbosity through:
  - `src/image/...` -- buildah invocations stream their stdout/stderr to outrig's
    stderr when verbose >= 1; otherwise capture and only print on failure.
  - `src/container.rs` (or wherever podman commands are issued) -- same: stream
    podman command lines and their output to stderr when verbose >= 1, and
    additionally tee into `<session_dir>/logs/container.log`.
  - `src/cli/run.rs` -- pass the `verbose` count into the image build + container
    start paths.
- A second `-v` (verbose >= 2) should also bump the `tracing` subscriber to
  trace level for outrig's own modules. Today `OUTRIG_LOG` is the only knob;
  `--verbose --verbose` should match `OUTRIG_LOG=trace` for the duration of
  the run.
- Drop the `> **TODO: Incomplete** -- ... not yet ...` markers from
  `doc/reference/cli.md` (page header line, plus the trailing parenthetical on
  the `--verbose` paragraph) and `doc/usage/run.md` (the block under the run
  flag table).

## Acceptance

- `cargo run -- run --verbose ...` shows the buildah/podman command lines on
  stderr and writes them into `<session_dir>/logs/container.log`.
- `cargo run -- run -vv ...` additionally emits TRACE-level outrig logs.
- `cargo run -- run` (no verbose) does **not** print buildah/podman command
  lines except when a build/start fails.
- An e2e test under `tests/` (gated `#[cfg(feature = "e2e")]`) asserts that the
  container.log file gains buildah/podman lines when `--verbose` is set.
- The two doc TODO markers are gone; a re-grep of `doc/` for `--verbose` shows
  only the descriptive text.

## Dependencies

- Tasks 0007 (image build) and 0008 (container lifecycle) shape where the
  buildah/podman invocations live. Both are done, so this task is unblocked.

## Notes

- This was deferred during the Phase A-E doc/code reconciliation pass: the docs
  promised the flag before the CLI accepted it, and the audit chose to mark it
  TODO rather than implement it inside a doc-cleanup task.

## Decisions

- **Session setup now preallocates the session id before image build.** That
  lets `session.json`, `logs/container.log`, and the podman container name all
  use the same id while buildah/podman transcripts are emitted. Failed startup
  after the session row exists finalizes the session with exit `1`.
- **`outrig build` keeps the old streamed build output path.** The run/mcp
  startup path uses the new transcript-aware capture helpers so no-verbose runs
  stay quiet on success, while `outrig build` continues to show buildah progress
  as a build-focused command.
- **MCP server stdout is not copied into `container.log`.** Verbose mode records
  the backing `podman exec` command line, but the MCP protocol stream remains
  owned by rmcp so transcripts cannot corrupt stdio protocol traffic.
- **Doc style cleanup included adjacent wide tables.** The full
  `scripts/audit-doc-style.py` check was failing on existing wide tables, so
  those tables were converted to definition-list bullets while the verbose docs
  were updated.
