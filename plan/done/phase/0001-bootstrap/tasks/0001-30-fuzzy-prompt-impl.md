# 0030 -- Rich-TUI `PromptSource` impl (FuzzySelect, etc.)

## Context

Task 0022 landed `init::prompt::PromptSource` (a trait) and `TerminalPrompt<R, W>`
(a literal-input impl rendering to `AsyncWrite` so tests can drive it through
`tokio::io::duplex`). For small finite lists ("openai" / "anthropic", `Y/n`,
free-text path) the literal-input UX is fine: the user types the value or
presses Enter for the default.

That falls down once a prompt's option list is **dynamic and large**, e.g.:

- "Pick a model" once `~/.outrig/config.toml` already defines several
  (`outrig run --model <pick>`, or `outrig init`'s "default-model for this
  agent" prompt when N>1 candidates exist).
- "Pick an agent" in repos that define multiple agents.
- "Pick a container-config" once a repo has more than one in `.agents/outrig/`.

The right UX for these is `dialoguer::FuzzySelect` (or equivalent) -- arrow-key
navigation, incremental filter, no need to type the literal value. The trait
abstraction already exists; this task adds a second impl alongside
`TerminalPrompt`.

## Goals and non-goals

**In scope:**

- New `PromptSource` impl driving `dialoguer` (or equivalent crate) for
  `ask_select` / `ask_multiselect`.
- Defaults for `ask_string` / `ask_bool` either delegate to
  `dialoguer::Input` / `Confirm` or fall through to the existing
  `TerminalPrompt` line-based path -- pick whichever is less work.
- A way for callers to choose the impl: probably a small factory in
  `init::prompt` that returns a boxed `dyn PromptSource` based on
  "is stdin a TTY?". Non-TTY (CI, piped scripts) gets the line-based
  `TerminalPrompt`; TTY gets the rich impl.

**Out of scope:**

- Replacing `TerminalPrompt`. It stays the test-friendly fallback and is what
  every `tokio::io::duplex`-driven test will keep using.
- Reimplementing `?`-help inside the FuzzySelect chrome. Either (a) the rich
  impl pre-renders the description above the picker, or (b) `?`-help is a
  TerminalPrompt-only feature and the rich impl shows description+doc-link
  inline. Decide during the task.

## Approach sketch

1. Add `dialoguer` back to `Cargo.toml` (it was dropped at the end of 0022
   precisely because it had no caller without this task).
2. New file `src/init/prompt/dialoguer.rs` with a `DialoguerPrompt` struct
   that impls `PromptSource`. Each method:
   - Builds the dialoguer widget from `field` + default.
   - Wraps `.interact()` in `tokio::task::spawn_blocking` (dialoguer is sync
     and reads `/dev/tty` directly).
   - Maps the `dialoguer::Result` into `crate::error::Result`.
3. `pub fn auto() -> Box<dyn PromptSource>` factory in `init::prompt`:
   ```rust
   if std::io::IsTerminal::is_terminal(&std::io::stdin()) {
       Box::new(DialoguerPrompt::new())
   } else {
       Box::new(TerminalPrompt::from_real_io())
   }
   ```
   Callsites in 0024/0031/0033 use `prompt::auto()` and stop caring which
   impl is live.
4. **Tests:** the rich impl can't be driven through `tokio::io::duplex`
   (dialoguer needs a real TTY). Test the trait contract with the existing
   `TerminalPrompt` tests; add at most a `cargo test --test dialoguer_smoke`
   gated behind `#[cfg(unix)]` + a `pty` crate (`portable-pty` or
   `expectrl`) if smoke coverage feels necessary. Otherwise rely on manual
   verification + a thin "constructs without panicking" test.

## Files

- `Cargo.toml` -- re-add `dialoguer` (and any chosen pty crate as a
  dev-dep).
- `src/init/prompt/dialoguer.rs` (new).
- `src/init/prompt.rs` -- promote to `src/init/prompt/mod.rs` if not already,
  add `pub mod dialoguer;` and the `auto()` factory.
- Possibly `tests/dialoguer_prompt_smoke.rs` if a pty-based smoke test is
  added.

## Acceptance

- Running `outrig run` (or any future flow with N>1 models) on a TTY shows
  a fuzzy-select picker.
- Running the same flow under a piped stdin (CI, script) falls through to
  the line-based `TerminalPrompt` and behaves exactly as today.
- All existing `tests/prompt_ux.rs` tests still pass unchanged (they test
  `TerminalPrompt` directly, not `auto()`).

## Dependencies

- 0022-prompt-ux

## Notes

- The trait shape from 0022 is the integration surface; no changes to
  `Field` or to existing `TerminalPrompt` behavior should be needed.
- An eventual `AiPrompt` impl (LLM produces structured answers) is a third
  variant in the same hierarchy, unrelated to this task but part of why
  the trait exists.
- If `dialoguer` proves too sticky (sync API, no async hooks, can't
  intercept keystrokes), fall back to building on `crossterm` directly.
  `dialoguer` is the cheap default; `crossterm` is the escape hatch.

## Decisions

- **`auto()` returns an enum, not `Box<dyn PromptSource>`.** `PromptSource`
  uses bare `async fn`-in-trait (with `#[allow(async_fn_in_trait)]`), which
  is **not** dyn-compatible. `AutoPrompt { Terminal(_), Dialoguer(_) }`
  with a four-method forwarding impl gives a single concrete return type,
  no allocations, no type erasure -- callers still take
  `&mut impl PromptSource`. Variants are `pub` for now; the only construction
  path that matters is `auto()`.
- **`?`-help is not reimplemented inside dialoguer.** Instead, every
  `DialoguerPrompt::ask_*` prints `field.description` to stderr before the
  picker (option (a) from the task sketch). Option blurbs are skipped --
  dialoguer renders the values itself -- and `field.doc_link` is dropped
  for the rich impl: it's noise on every prompt and remains a
  `TerminalPrompt`-only on-demand feature.
- **`tokio::task::spawn_blocking` for every dialoguer call.** Dialoguer
  reads `/dev/tty` synchronously; running it on the runtime worker would
  block other tasks even in the single-task `config init` flow. The
  per-call thread-pool overhead is irrelevant at human-paced prompt rates.
- **`allow_empty(true)` on `Input`.** Looks redundant next to `.default(s)`
  but isn't: dialoguer rejects an Enter on empty input when the default
  is `""`. Setting `allow_empty(true)` matches `TerminalPrompt::ask_string`'s
  "Enter returns the default, even if the default is empty" semantics.
- **`dialoguer` 0.12 with the `fuzzy-select` feature.** `FuzzySelect` is
  feature-gated; without it, only `Select` (no incremental filter) is
  available. Adds `fuzzy-matcher` and a few transitive deps -- acceptable
  for the UX win.
- **`AutoPrompt` and `auto()` are wired into `config::init::run()`.** This
  is the only live caller today; tasks 0031/0033 will use `auto()` the
  same way without additional plumbing.
- **No pty smoke tests added.** The trait contract is fully covered by
  `tests/prompt_ux.rs` against `TerminalPrompt`; `DialoguerPrompt` is a
  thin translation layer to dialoguer, and a pty crate would add weight
  for marginal coverage. TTY behavior is verified manually.
