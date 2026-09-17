# 0045 -- Bound stderr capture in `run_capture`

## Goal

Replace the unbounded `Command::output()` body of `src/process.rs::run_capture`
(added in task 0006) with a streaming reader that writes into a fixed-size
ring buffer, so peak memory use is bounded by `STDERR_TAIL_LIMIT` regardless
of how much the child writes. Today a child that emits 10 GB of stderr would
OOM the host process before `tail_string` ever truncated it. Real callers are
buildah and podman, neither of which routinely emit hundreds of MB, so this
is robustness rather than fixing observed pain.

## Deliverables

- `src/process.rs::run_capture` -- replace the `Command::output()` body with:
  1. `Command::stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()`.
  2. Concurrently:
     - `tokio::spawn` a task that reads stdout to a `Vec<u8>` (unchanged
       behavior; stdout is what callers want to inspect on success).
     - `tokio::spawn` a task that reads stderr in chunks into a fixed-size
       ring buffer of `STDERR_TAIL_LIMIT` bytes. When the ring wraps, set a
       `truncated` flag.
  3. `child.wait().await` for the exit status.
  4. Join both reader tasks.
  5. On failure, materialize the ring into a `String` with the same
     `... (truncated) ...` marker semantics as `tail_string` today. On
     success, return an `Output`-shaped result. The stderr field carries the
     same bounded tail so stderr intake stays capped on every exit path.
- The ring buffer can be a `VecDeque<u8>` with an explicit byte cap, or a
  fixed `[u8; N]` with a wrap pointer. `VecDeque` is simpler; the `[u8; N]`
  form avoids any heap allocs on the hot path. Pick whichever reads
  cleaner.
- Same observable error surface: `OutrigError::Process { stderr_tail, .. }`
  still contains a truncation-marker-prefixed tail. No callers need to
  change.
- Stdout handling unchanged: still fully buffered. Bounding stdout would
  change the success-path API and isn't needed for buildah/podman. Callers
  that need bounded stdout can add a separate entry point later
  (`run_capture_bounded_stdout`?).
- `run_streamed` is untouched -- it already streams stderr line-by-line
  through tracing with no buffering.
- `tests/process.rs` -- add a test that pipes ~10 MB of stderr and asserts
  peak memory stays bounded. Use a heuristic length-of-buffer assertion (the
  *observed* tail is still capped at 1 MiB; that's what's testable). A
  `/proc/self/status:VmHWM` check is overkill.

### Files

- `src/process.rs` -- swap the `Command::output()` body of `run_capture` for
  the streaming variant. Likely keep `tail_string` for the success path's
  lossy decode but it's no longer the truncation primitive on stderr.
- `tests/process.rs` -- add the bounded-memory test.

## Acceptance

- A child writing >10 MB of stderr produces an error whose `stderr_tail` is
  still at most 1 MiB and starts with the truncation marker, with no measurable
  memory hump on the host process.
- Existing five tests in `tests/process.rs` still pass unchanged.
- `cargo test process` passes.
- `clippy` clean; `fmt` clean.

## Dependencies

None.

## Decisions

- `run_capture` now keeps only the bounded stderr tail even when the child
  exits successfully. Current callers inspect stdout on success, and retaining
  full success-path stderr would violate the goal that stderr intake stays
  capped regardless of how much the child writes.
- `try_capture` and `run_capture_logged` keep their existing full-output
  behavior; this task is scoped to `run_capture` and its structured process
  error surface.
- The stderr tail limit is now 1 MiB instead of the original 2 KiB. Buildah
  and podman failures can need more than a few final lines of context, and
  1 MiB remains a small, explicit bound for process-error diagnostics.

## Notes

- v0 accepted the unbounded behavior. The old contract -- "the error contains
  the last ~2 KiB of stderr" -- was honest about the *output*; it just
  didn't cap *intake*. Real callers are buildah and podman, neither of which
  routinely emit hundreds of MB, so the v0 stance was "no observed pain yet."
- Keep the change scoped: don't expand the `Cmd` API or the `Process` error
  variant. This is a behind-the-back robustness improvement.
- Single self-contained task. No dependencies beyond 0006 (which is `done/`).
