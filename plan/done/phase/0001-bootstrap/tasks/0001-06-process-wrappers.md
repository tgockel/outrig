# 0006 -- Subprocess wrappers

## Goal

Thin, structured wrappers around `buildah` and `podman` invocations. Subsequent tasks should
not call `tokio::process::Command` directly -- they go through these helpers so error messages,
stderr capture, and tracing are consistent.

## Deliverables

- `src/process.rs::Cmd { program: &'static str, args: Vec<OsString> }` builder-ish helper plus
  three call patterns:
  - `run_capture(cmd: Cmd) -> Result<Output>` -- spawn, capture stdout + stderr, return on
    non-zero with a structured error: argv, exit code, stderr tail (last ~2 KiB).
  - `run_streamed(cmd: Cmd, prefix: &'static str) -> Result<ExitStatus>` -- forward stderr
    line-by-line to tracing with `[<prefix>] <line>`. Useful for buildah's progressive output.
  - `spawn_stdio(cmd: Cmd) -> Result<tokio::process::Child>` -- spawn with `Stdio::piped()`
    on stdin/stdout/stderr; caller owns the child. Used by `podman exec -i`.
- `OutrigError::Process { program, argv, exit_code, stderr_tail }` variant.
- `tests/process.rs` using `/bin/echo` and `/bin/false` as stand-ins:
  - `run_capture(echo "hi")` succeeds and returns `"hi\n"`.
  - `run_capture(false)` fails with `exit_code = 1`, the argv preserved.
  - `run_streamed` forwards stderr to a tracing test subscriber.
  - `spawn_stdio` returns a child whose stdin/stdout are usable.

## Acceptance

- `cargo test process` passes.
- A non-zero exit produces an error containing the command, exit code, and stderr tail.
- The stderr-tail truncation is honest: tail not head, with a leading `... (truncated) ...`
  marker if elision happened.

## Dependencies

- 0001-cargo-skeleton

## Notes

- Use `tokio::process::Command`, not `std::process`.
- The tracing-subscriber-aware test for `run_streamed` can install a per-test subscriber via
  `tracing_subscriber::fmt::Subscriber` + a thread-local capture. Keep simple.
- This module shouldn't know about buildah or podman specifically; it's a generic subprocess
  helper. Image/container modules pass the program name in.

## Decisions

- **`Process` Display via free `format_process` helper, not inline thiserror format string.**
  The variant has multi-line output and a Some/None branch on `exit_code` ("code N" vs
  "signal"). thiserror's `#[error("...", expr)]` with `match .exit_code { ... }` works but
  reads worse than a small free function. Helper lives next to the variant in `error.rs`.
- **`argv` stores just args, not the program.** `Cmd` has `program` and `args` separately;
  the error variant mirrors that split, and `Display` prints `program` separately on its own
  line. Avoids the "is `argv[0]` the binary or the first arg?" ambiguity that comes with
  Unix-style argv.
- **`run_streamed` emits `tracing::info!` at target `outrig::process`.** info, not warn or
  error -- buildah's progressive output is informational. The explicit target makes the
  stream filterable and gives tests a stable handle to assert on.
- **Streamed-tracing test uses a synchronous `#[test]` with a `current_thread` tokio runtime
  and `tracing::subscriber::set_default`.** `set_default` is per-thread; `current_thread`
  keeps spawned tasks on the same OS thread so the log task sees the same subscriber. A
  `#[tokio::test]` (multi-thread) would race against the per-thread default. The test owns a
  shared `Arc<Mutex<Vec<u8>>>` buffer fed via a custom `MakeWriter` -- no new dev-deps.
- **Unbounded stderr in `run_capture` is accepted for v0.** `Command::output()` buffers all
  stderr before `tail_string` truncates. A 10 GB stderr would OOM. The task spec frames the
  contract as "last ~2 KiB" and a streaming-bounded read is a v1 follow-up if real callers
  hit it.
