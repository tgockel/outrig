# 0018 -- REPL skeleton

## Goal

A stdin/stdout REPL that handles slash commands, EOF, and SIGINT cleanly, parameterized over
an async callback that processes each user prompt. The actual agent wiring happens in 0019;
this task delivers the I/O loop in isolation.

## Deliverables

- `src/repl.rs::Repl` with a method:
  ```rust
  pub async fn run<F, Fut>(banner: &str, mut on_prompt: F) -> Result<()>
  where
      F: FnMut(String) -> Fut,
      Fut: Future<Output = Result<String>>,
  ```
  Behavior:
  - Print `banner` (multi-line) to stderr.
  - Loop:
    - Print `> ` to stderr, flush.
    - Read a line via `tokio::io::BufReader::new(tokio::io::stdin()).lines().next_line()`.
    - `None` (EOF) -> break and shut down.
    - Empty line -> continue.
    - Line starts with `/` -> handle slash command (see below).
    - Else -> `let reply = on_prompt(line).await?; println!("{reply}");`.
  - On SIGINT during `on_prompt` execution: cancel the future, print
    `\n[outrig] interrupted` to stderr, retain history, return to prompt.
  - On second consecutive SIGINT (no input typed in between): break out and exit.
- Slash commands stubbed:
  - `/help` -- print the slash-command list to stderr.
  - `/quit` -- break out of the loop (same as Ctrl-D).
  - `/tools` -- placeholder, prints `(no tools registered)` until 0019 wires it.
  - `/reset` -- placeholder, prints `(no history to reset)` until 0019 wires it.
- The slash commands' real data (tool list, history clear) come in 0019 via additional
  callbacks: `Repl::run_with_extras(banner, on_prompt, on_slash)` is a possible extension --
  designer's call. Simplest: pass a small `ReplCallbacks` struct.
- `tests/repl_io.rs` driving via `tokio::io::duplex`:
  - Multiple lines processed in order.
  - EOF cleanly exits.
  - Slash command `/quit` exits.
  - Empty line is ignored.
  - SIGINT mid-callback prints "interrupted" and returns to prompt.

## Acceptance

- `cargo test repl_io` passes.
- A stub callback echoing input back works end-to-end with a piped stdin.

## Dependencies

- 0001-cargo-skeleton

## Notes

- `tokio::signal::ctrl_c()` is the SIGINT future; combine with the prompt future via
  `tokio::select!`.
- For testability, abstract stdin/stdout behind a generic `AsyncBufRead`/`AsyncWrite` so the
  test can substitute `tokio::io::duplex`.
- Strict separation: assistant text -> stdout; everything else -> stderr.
