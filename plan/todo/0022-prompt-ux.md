# 0022 -- Prompt UX wrapper

## Goal

A small wrapper around `dialoguer` that adds two features documented in `doc/usage/init.md`:

- `[default: ...]` rendering inline with each prompt, with Enter accepting the default.
- `?` interception: typing `?` and Enter prints a description + options + a doc link, then
  re-prompts.

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
- Public functions, all returning `Result<...>`:
  - `ask_string(field: &Field, default: &str) -> Result<String>`
  - `ask_bool(field: &Field, default: bool) -> Result<bool>` -- renders `[Y/n]` or `[y/N]`.
  - `ask_select(field: &Field, default_idx: usize) -> Result<usize>` -- assumes
    `field.options` is non-empty.
  - `ask_multiselect(field: &Field, default_indices: &[usize]) -> Result<Vec<usize>>` --
    comma-separated input.
- All four implementations:
  1. Render `? <field.name> [default: <default>]: `.
  2. Read a line.
  3. If line == `?`: print `\n  <description>\n` then each option as `  <value>  <blurb>`,
     then `\n  See: <doc_link>\n`, then re-prompt.
  4. If line is empty: return the default.
  5. Else: parse/validate; on parse error, print error and re-prompt.
- `Field` instances live as constants near where they're used (init.rs, init/container.rs);
  the prompt module just exposes the type + `ask_*`.
- `tests/prompt_ux.rs` driving with `tokio::io::duplex` to simulate stdin:
  - Empty line returns default.
  - `?` prints help and re-prompts.
  - Invalid input re-prompts.
  - Valid input returns it.
  - Multi-select parses comma-separated values, trimming whitespace.
- `tests/prompt_doc_sync.rs`: walks every `Field` instance (collected via `inventory` crate or
  a manual `&[&Field]` slice in tests) and asserts each `doc_link` resolves to a real file
  under `doc/`.

## Acceptance

- `cargo test prompt_ux` passes.
- `cargo test prompt_doc_sync` passes (every help link is live).

## Dependencies

- 0001-cargo-skeleton

## Notes

- `dialoguer`'s `Input`/`Select` already handle some of this, but we need full control to
  intercept `?` before dialoguer sees it. Simplest: don't use `dialoguer::Input` -- write a
  ~30-line read-line loop with `tokio::io::BufReader::new(stdin()).lines()`. Use
  `dialoguer` only if it gives clear value (e.g. nice TTY rendering for selects); otherwise
  drop it from deps.
- Keep the Field struct's lifetimes `&'static str` to make construction trivial in callers.
