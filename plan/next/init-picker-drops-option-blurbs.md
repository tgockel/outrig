# The TTY picker renders option values only, so every `Field`'s blurbs are invisible

## Context

`Field::options` is a `&[(&str, &str)]` of `(value, blurb)` pairs, and the blurb is where each
option's help lives. On a TTY -- the path most users take -- it is never rendered.

`crates/outrig-cli/src/init/prompt/dialoguer.rs`, `DialoguerPrompt::ask_select`:

```rust
async fn ask_select(&mut self, field: &Field, default_idx: usize) -> Result<usize> {
    write_description(field.description).await?;
    let items: Vec<&'static str> = field.options.iter().map(|(v, _)| *v).collect();
    // ...FuzzySelect::new().items(&items)
```

The blurb is dropped on the `.map(|(v, _)| *v)`, and the only text printed alongside the picker
is `field.description`. `ask_multiselect` builds its items the same way. `auto()`
(`init/prompt/mod.rs`) selects this backend whenever stdin is a TTY.

The blurbs *are* rendered by `format_field_help`, but that is reachable only from
`TerminalPrompt::read_one`, which returns `RawLine::Help` when the user types `?`. That path
exists only on the piped / CI backend, and only for a user who thinks to type `?`. A
`FuzzySelect` returns a `usize` and has no equivalent branch.

Found while deprecating the in-process LLM backend: the `mistralrs` style's blurb was given a
`DEPRECATED (will be removed)` prefix and reached nobody on a terminal. That commit worked
around it by appending the notice to `STYLE_FIELD.description`, which both backends do print.
Removing the backend on the 0.3 line deleted that row and reverted the workaround with it, so
nothing now works around the bug: every blurb is invisible on a terminal again.

## Goal

Make a TTY user see the same per-option help a `?`-typing piped user sees.

## Deliverables

- `ask_select` / `ask_multiselect` build display labels that carry the blurb (e.g.
  `format!("{value}  --  {blurb}")`) while still returning the option *index*, so every caller's
  `field.options[idx]` lookup is unchanged.
- A test that a blurb reaches the rendered item list. `DialoguerPrompt` drives a real terminal,
  so the testable seam is whatever builds the label strings -- extract it as a free function and
  test that, rather than trying to drive `FuzzySelect`.

## Design forks

1. **Fuzzy matching would start searching blurb text -- Open.** `FuzzySelect` matches against the
   item strings it is given, so embedding blurbs means typing `local` could match an option whose
   *blurb* says "local server" rather than the one whose value is `local`. Options: accept it,
   switch these to plain `Select` (loses fuzzy filtering entirely), or keep the label short by
   truncating the blurb to its first clause. Check how long the real blurbs are first --
   `config_init::STYLES` blurbs are two sentences, which is already too long for a picker row.

2. **Whether `description` should carry option-level text -- Settled: no.** The one workaround
   that put option text there went with the in-process backend, and `description` describes the
   field again. Keep it that way once blurbs render.

## Dependencies

None.

## See also

- `crates/outrig-cli/src/init/prompt/dialoguer.rs` -- `ask_select`, `ask_multiselect`,
  `write_description`.
- `crates/outrig-cli/src/init/prompt/mod.rs` -- `format_field_help` (the renderer that does show
  blurbs), `TerminalPrompt::read_one` (the `?` branch), `auto()` (backend selection).
- `crates/outrig-cli/src/config_init.rs` -- `STYLE_FIELD`, carrying the workaround.
