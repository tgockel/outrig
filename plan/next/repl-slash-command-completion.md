# Tab does nothing at the REPL prompt

## Context

The REPL knows every slash command it accepts. `REPL_COMMANDS`
(`crates/outrig-cli/src/cli/run.rs:420-437`) is a `&'static [HelpEntry]` carrying the syntax of
each one, and `Repl` adds its own `/help` and `/quit`; that table already drives `/help` output
through `compose_help`. None of it is reachable from the line editor, so `Tab` at the prompt
inserts a literal tab.

The rustyline spike deliberately stopped short of this: it takes `default-features = false` and
builds `DefaultEditor`, i.e. `Editor<(), MemHistory>`, with no helper attached.

## Goal

`Tab` completes the slash commands the session actually has, from the table that already
describes them.

## Sketch

A `ReplHelper` type implementing `rustyline::completion::Completer`, attached with
`Editor::set_helper`; the editor becomes `Editor<ReplHelper, MemHistory>` and the reader thread
in `repl/editor.rs` carries it. `custom-bindings` is only needed if `Tab` is to be rebound --
the default keymap already routes it to completion.

Completion candidates have to reach the editor thread. The table is `&'static`, which makes this
easy today, but the honest design question is the shape of `HelpEntry`, not the plumbing:
`syntax` mixes the command name with argument placeholders (`"/sidecar add <name>"`), so a
completer fed those strings verbatim would offer `<name>` as a literal. Either parse the syntax
into (command, subcommand, placeholder) or give `HelpEntry` a separate completion form. The
first keeps one source of truth and risks the parser drifting from what `/help` renders; the
second cannot drift but can go stale.

Worth deciding at the same time: whether `/sidecar add <name>` completes the *declared* sidecar
names. They are in `SessionMcpPlan`, not in a static table, so that arm needs either a handle on
session state or a snapshot the REPL refreshes per prompt -- and a `/sidecar add` mid-session
changes the answer, which is the interesting case.

## Acceptance

- `Tab` on `/si` yields `/sidecar `; `Tab` on `/sidecar ` offers `add` and `list`.
- `Tab` on a non-slash line does not offer commands.
- Completion does not disturb history recall: `Up`, edit, `Tab`, `Enter` behaves.
- `/help` and completion cannot disagree about which commands exist -- either they share a
  source, or a test fails when they diverge.

## Dependencies

- **Landed with the rustyline spike on `prototype/rustyline`**, which introduced the editor.
- `plan/next/repl-multiline-input.md` wants the same `Helper` type. Do them together or in
  either order, but the second one should not have to reshape what the first built.
