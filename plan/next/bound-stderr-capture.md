# Bound stderr capture in `run_capture`

> **Status:** preliminary spec. Carved into a numbered task in `plan/todo/` when ready.

## Context

`src/process.rs::run_capture` (added in task 0006) calls
`tokio::process::Command::output()`, which buffers the child's full stderr in memory
before `tail_string` truncates it to the last 2 KiB. A child that emits 10 GB of stderr
would OOM the host process before the tail ever runs.

v0 accepted this. The contract -- "the error contains the last ~2 KiB of stderr" -- is
honest about the *output*; it just doesn't cap *intake*. Real callers will be buildah
and podman, neither of which routinely emit hundreds of MB of stderr, so the v0 stance
is "no observed pain yet."

This task replaces the buffering with a streaming reader that writes into a fixed-size
ring buffer (or equivalent), so peak memory use is bounded by `STDERR_TAIL_LIMIT`
regardless of how much the child writes.

## Goals and non-goals

**In scope:**

- `run_capture` peak memory use stays O(`STDERR_TAIL_LIMIT`) plus stdout size.
- Same observable error surface: `OutrigError::Process { stderr_tail, .. }` still
  contains a truncation-marker-prefixed tail.
- Same handling of stdout: still fully buffered. Stdout is what callers want to inspect
  on success; bounding it would change the success-path API.

**Out of scope:**

- Bounding stdout. Callers that need bounded stdout can add a separate entry point
  later (`run_capture_bounded_stdout`?). Not needed by buildah/podman.
- Changing `run_streamed` -- it already streams stderr line-by-line through tracing
  with no buffering.

## Approach sketch

Drop `Command::output()`. Instead:

1. `Command::stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()`.
2. Concurrently:
   - `tokio::spawn` a task that reads stdout to a `Vec<u8>` (unchanged from today).
   - `tokio::spawn` a task that reads stderr in chunks into a fixed-size ring buffer
     of `STDERR_TAIL_LIMIT` bytes. When the ring wraps, set a "truncated" flag.
3. `child.wait().await` for the exit status.
4. Join both reader tasks.
5. On failure, materialize the ring into a `String` with the same `... (truncated) ...`
   marker semantics as `tail_string` today. On success, return an `Output`-shaped
   result (we already do this; the stderr field on success can stay full, since it
   travels nowhere -- callers don't read it).

The ring buffer can be a `VecDeque<u8>` with an explicit byte cap, or a fixed `[u8; N]`
with a wrap pointer. `VecDeque` is simpler; the `[u8; N]` form avoids any heap allocs
on the hot path. Pick whichever reads cleaner.

## Files

- `src/process.rs` -- swap the `Command::output()` body of `run_capture` for the
  streaming variant. Likely keep `tail_string` for the success path's lossy decode but
  it's no longer the truncation primitive on stderr.
- `tests/process.rs` -- add a test that pipes ~10 MB of stderr and asserts peak memory
  stays bounded. Use `/proc/self/status:VmHWM` or just a heuristic length-of-buffer
  assertion (the *observed* tail is still ~2 KiB; that's what's testable).

## Acceptance

- A child writing >10 MB of stderr produces an error whose `stderr_tail` is still
  ~2 KiB and starts with the truncation marker, with no measurable memory hump on the
  host process.
- Existing five tests in `tests/process.rs` still pass unchanged.

## Notes

- This is a single self-contained task. No dependencies beyond 0006.
- Keep the change scoped: don't expand the `Cmd` API or the `Process` error variant.
