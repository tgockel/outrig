# Typed multi-line input is still not supported

## Context

`doc/usage/run.md` has carried a `TODO: Incomplete` for multi-line input since the REPL existed.
The rustyline spike narrowed it rather than closing it: pasting several lines now arrives as a
single prompt with the breaks intact, because rustyline enables bracketed paste by default and
inserts the whole payload as one edit. What is still missing is *deliberate composition* --
writing a prompt across several lines by hand, where `Enter` currently ends the turn.

This matters more for outrig than for a typical shell. A prompt to a coding agent is often a
paragraph with a list in it, and today the only way to get one is to compose it elsewhere and
paste.

## Goal

A prompt can be typed across several lines, and `Enter` still ends a single-line one without
ceremony.

## Sketch

`rustyline::validate::Validator` is the intended hook: `validate()` returns
`ValidationResult::Incomplete` to keep the editor open and paint a continuation prompt, and it
lives on the same `Helper` the completion task introduces. The editor becomes
`Editor<ReplHelper, MemHistory>` either way.

The open question is the continuation rule, and it is a UX call rather than a technical one:

- **Trailing `\`** -- familiar from shells, invisible in a paste, easy to typo.
- **A `"""` fence** -- unambiguous and paste-safe, but two extra lines of ceremony.
- **Alt-Enter inserts a literal newline, Enter always submits** -- what most chat clients do, and
  the only one that needs no sentinel in the text. Needs `custom-bindings` and a discoverable way
  to tell the user, since nothing on screen hints at it.

Two things to settle beyond the rule itself:

1. Whether anything downstream of `on_prompt` cares about embedded newlines. rig passes the
   string through, so probably not -- but the spike only ever produced them via paste, and that
   path has no test.
2. How a multi-line entry is stored in and recalled from history. `MemHistory` is a
   `VecDeque<String>` with no notion of an entry spanning lines, so `Up` on a three-line prompt
   has to either restore all three into the editor or silently flatten them.

## Acceptance

- A prompt composed over three lines reaches `on_prompt` as one string with the breaks intact.
- A single-line prompt still submits on one `Enter`, with no new keystroke to learn.
- `Up` recalls a multi-line prompt in a state that can be edited and resubmitted.
- The `TODO: Incomplete` marker in `doc/usage/run.md` is removed, not merely narrowed again.

## Dependencies

- **Landed with the rustyline spike on `prototype/rustyline`**, which closed the paste half.
- `plan/next/repl-slash-command-completion.md` introduces the `Helper` this hangs off.
