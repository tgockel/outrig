# A cancelled REPL leaves its reader thread eating keystrokes through teardown

## Context

When the session watcher sees the primary container die, `cli/run.rs:374-384`
cancels the REPL by dropping its future. If the reader thread is parked inside
`readline` at that moment it stays there: `readline` is not interruptible, and
the thread is detached precisely so tokio never waits on it.

The terminal itself is safe. `TerminalModeGuard` waits until the reader is
provably idle or provably inside `readline`, then restores termios, and the
`stopped` flag keeps any queued read from starting afterwards -- so the user's
shell is canonical again whatever the ordering. What is left is narrower: for
the duration of teardown (`subagents.shutdown`, `drop(agent)`, `teardown` --
seconds, since it stops and removes containers) that abandoned reader is still
blocked on a `read` of `/dev/tty`. Anything typed in that window is consumed by
a thread that will never do anything with it, instead of being queued for the
shell the user is about to be returned to.

Cosmetic, and bounded by teardown, but it is the kind of thing that reads as a
dropped keystroke rather than as a design decision.

## Sketch

Two shapes, neither obviously right:

- **Make the read interruptible.** Nothing in rustyline 18 offers this.
  `ExternalPrinter` wakes the reader's `select` but only so it can print and
  carry on; `enable_signals` changes who sees `Ctrl-C`, not whether `readline`
  can be made to return. It would mean driving the terminal below rustyline, or
  an upstream change.
- **Do not cancel mid-read at all.** Race the death token only around
  `on_prompt`, so a death during a prompt is noticed at the next Enter. Removes
  the class outright -- no abandoned reader, no guard needed for this path --
  at the cost of a dead session sitting at a live prompt until the user presses
  a key, and of plumbing the token into `Repl`. This was the alternative
  considered and rejected when the guard was written; it is worth re-opening if
  the swallowed keystrokes turn out to matter more than the delay.

## Acceptance

- Keys typed between a primary-container death and process exit reach the
  user's shell, or are demonstrably discarded by the terminal rather than by an
  outrig thread.
- The terminal is still canonical afterwards, with no `stty sane` needed --
  whatever replaces the guard must keep the property the guard bought.

## Dependencies

- **Landed with the rustyline spike on `prototype/rustyline`**, which
  introduced the reader thread and the guard that bounds this.
