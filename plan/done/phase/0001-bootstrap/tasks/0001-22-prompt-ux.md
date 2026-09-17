# 0022 -- Prompt UX wrapper

## Goal

A small wrapper around interactive Q&A for `outrig init` /
`outrig config init` / `outrig container add` that delivers the two UX
features documented in `doc/usage/init.md`:

- `[default: ...]` rendering inline with each prompt, with Enter accepting
  the default. (`ask_bool` uses the shorthand `[Y/n]` / `[y/N]` per the doc.)
- `?` interception: typing `?` and Enter prints a description + options +
  doc link, then re-prompts.

The interface is a **trait**, so the same callers can later be driven by an
AI/LLM answer source (which produces structured values, not bytes on a pipe)
without re-architecture. The trait pivot replaces the originally-filed
"four bare `ask_*` functions" deliverable; see `## Decisions` below.

## Deliverables

- `src/init/prompt.rs::Field`:
  ```rust
  pub struct Field {
      pub name: &'static str,             // "Pick a provider style"
      pub description: &'static str,      // shown on `?`
      pub options: &'static [(&'static str, &'static str)],   // (value, blurb); empty = free-text
      pub doc_link: &'static str,         // "doc/concepts/llm-providers.md"
  }
  ```
- `src/init/prompt.rs::PromptSource` trait, four async methods:
  ```rust
  pub trait PromptSource {
      async fn ask_string(&mut self, field: &Field, default: &str) -> Result<String>;
      async fn ask_bool(&mut self, field: &Field, default: bool) -> Result<bool>;
      async fn ask_select(&mut self, field: &Field, default_idx: usize) -> Result<usize>;
      async fn ask_multiselect(
          &mut self, field: &Field, default_indices: &[usize],
      ) -> Result<Vec<usize>>;
  }
  ```
  `Result` = `crate::error::Result`. Validation failures re-prompt internally;
  only I/O / EOF surfaces as `Err`.
- `src/init/prompt.rs::TerminalPrompt<R, W>` -- the production impl backed by
  generic streams. Provides `TerminalPrompt::new(stdin, stderr)` and a
  convenience `TerminalPrompt::from_real_io()` for
  `BufReader<Stdin>` / `Stderr`.
- All four trait methods on `TerminalPrompt`:
  1. Render `? <field.name> [<default-render>]: ` to stderr.
  2. Read a line.
  3. If line == `?`: print `\n  <description>\n` then each option as
     `  <value>  <blurb>`, then `\n  See: <doc_link>\n`, then re-prompt.
  4. If line is empty: return the default.
  5. Else: parse/validate; on parse error, print error and re-prompt.
- `Field` instances live as constants near where they're used (init.rs,
  init/container.rs in later tasks); the prompt module just exposes the
  type, the trait, and the impl. No `Field` constants are added in this
  task -- they arrive with 0023/0024/0026.
- `tests/prompt_ux.rs` driving `TerminalPrompt` with `tokio::io::duplex` to
  simulate stdin:
  - Empty line returns default.
  - `?` prints help and re-prompts.
  - Invalid input re-prompts with an error message.
  - Valid input returns it.
  - Multi-select parses comma-separated values, trimming whitespace.
  - EOF returns an `Io(UnexpectedEof)` error.
- `tests/prompt_doc_sync.rs`: walks a manual `&[&Field]` slice (seeded with
  one example `Field` defined in-test) and asserts each `doc_link` resolves
  to a real file under `doc/`. Tasks 0023/0024/0026 add their constants to
  the slice.

## Acceptance

- `cargo test prompt_ux` passes.
- `cargo test prompt_doc_sync` passes (every help link is live).

## Dependencies

- 0001-cargo-skeleton

## Notes

- `dialoguer` is a current dependency but is unused after this task; drop it
  from `Cargo.toml` if no other code references it.
- The terminal impl is a ~30-line read-line loop using
  `tokio::io::AsyncBufReadExt::lines`, with prompts written to stderr to
  match `Repl::run_with` (`src/repl.rs:118`).
- Edition 2024 enables async-fn-in-trait directly, so the trait needs no
  `#[async_trait]` macro. Callers downstream (0023/0024/0026) take
  `&mut impl PromptSource`.

## Decisions

- **Trait abstraction over bare `ask_*` functions.** `PromptSource` lets
  terminal, AI/LLM, and scripted-test answer sources all satisfy the same
  contract. Originally filed as four bare async fns; pivoted during the
  /next-task planning conversation when the user pointed out that AI-driven
  init flows are an expected v1 follow-up, and a stream-based interface
  fits that poorly.
- **Manual `&[&Field]` slice for `prompt_doc_sync.rs`.** No new dependency.
  `inventory` was considered (auto-registration via linker tricks) and
  declined; future tasks add their constants to the slice explicitly.
- **Validation errors do not surface as `Err`.** Re-prompt is internal; the
  trait contract is "produce a valid answer, retrying as needed." Only
  I/O failures (including EOF -> `UnexpectedEof`) escape.
- **Prompts written to stderr, not stdout.** Matches `Repl::run_with` so
  stdout stays clean for any data output.
