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
     success, return an `Output`-shaped result (we already do this; the
     stderr field on success can stay full, since it travels nowhere --
     callers don't read it).
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
  *observed* tail is still ~2 KiB; that's what's testable). A
  `/proc/self/status:VmHWM` check is overkill.

### Files

- `src/process.rs` -- swap the `Command::output()` body of `run_capture` for
  the streaming variant. Likely keep `tail_string` for the success path's
  lossy decode but it's no longer the truncation primitive on stderr.
- `tests/process.rs` -- add the bounded-memory test.

## Acceptance

- A child writing >10 MB of stderr produces an error whose `stderr_tail` is
  still ~2 KiB and starts with the truncation marker, with no measurable
  memory hump on the host process.
- Existing five tests in `tests/process.rs` still pass unchanged.
- `cargo test process` passes.
- `clippy` clean; `fmt` clean.

## Dependencies

None.

## Notes

- v0 accepted the unbounded behavior. The contract -- "the error contains
  the last ~2 KiB of stderr" -- is honest about the *output*; it just
  doesn't cap *intake*. Real callers are buildah and podman, neither of
  which routinely emit hundreds of MB, so the v0 stance was "no observed
  pain yet."
- Keep the change scoped: don't expand the `Cmd` API or the `Process` error
  variant. This is a behind-the-back robustness improvement.
- Single self-contained task. No dependencies beyond 0006 (which is `done/`).
