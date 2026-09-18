# The crate has two answers to "is this an interactive terminal", and they disagree

## Context

`init::prompt::auto()` (`crates/outrig-cli/src/init/prompt/mod.rs:368-374`) asks
`std::io::stdin().is_terminal()` and nothing else. The REPL's `EditorSource::try_new`
(`crates/outrig-cli/src/repl/editor.rs`) asks the same question *and* rejects `TERM` values of
`dumb`, `cons25`, and `emacs`, because rustyline refuses to drive those and falls back to a path
that writes the prompt to stdout -- which would break outrig's stream separation.

So on a dumb terminal `outrig run` now correctly declines the line editor while `outrig init`
still hands you dialoguer, and dialoguer gates purely on `Term::is_term()`: it never consults
`TERM`. It will drive raw-mode arrow-key widgets on exactly the terminal rustyline just refused.

That is a pre-existing `init` bug, not one the rustyline spike introduced. But the spike is the
moment the crate acquired the knowledge to fix it, and it buried that knowledge in a private
function under `repl/` where `init` cannot see it.

## Goal

One place decides what counts as an interactive terminal, and both interactive surfaces consult
it.

## Sketch

A small `crate::term` policy module exposing something like `fn rich_terminal() -> bool`, called
by `init::prompt::auto()` and by `EditorSource::try_new`. Roughly twenty lines plus two call
sites and a test move.

The question to settle first is whether the two callers really want the *same* predicate.
rustyline's objection to `TERM=dumb` is specific and documented; dialoguer's tolerance of it may
be a bug or may reflect that its widgets degrade acceptably. Check what `Select` actually renders
under `TERM=dumb` before assuming `init` should decline too -- the answer decides whether this is
one predicate or two with a shared stdin check.

Worth folding in: `editor.rs`'s `UNSUPPORTED_TERMS` mirrors a private constant inside rustyline
(`src/tty/mod.rs`), and the tests next to it assert outrig's copy against itself, so upstream
drift is invisible. A single policy module gives that list exactly one home to audit on a
rustyline upgrade.

## Acceptance

- `outrig init` and `outrig run` classify the same terminal the same way, or the difference is
  written down where both can see it.
- `TERM=dumb outrig init` does not start an arrow-key picker.
- The terminal-capability list has one definition in the tree.

## Dependencies

- **Landed with the rustyline spike on `prototype/rustyline`**, which added the second answer.
- Related: `plan/next/init-picker-drops-option-blurbs.md` is the other open dialoguer question;
  if that one reopens `DialoguerPrompt`, do these together.
