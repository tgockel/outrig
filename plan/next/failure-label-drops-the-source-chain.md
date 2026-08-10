# A transport failure's label shows reqwest's outer message, not the cause

## Context

`failure_label` (`crates/outrig-cli/src/llm/retry.rs`) renders a transport failure as
`connection error: {inner}`, where `inner` is the boxed `reqwest::Error`. `reqwest`'s `Display`
names the request and stops there -- `error sending request for url (http://...)` -- and puts the
part a user needs (`tcp connect error: Connection refused`, `dns error: failed to lookup address
information`, a TLS mismatch) in the `source()` chain, which nothing walks.

So the two lines a user sees for a typo'd `base-url` and for a provider that dropped a live
connection are near-identical, and neither says which happened. 0112 made that gap matter more: it
gave the two cases *different budgets*, so the retry line now counts down against a bound whose
choice the message does not explain.

## Sketch

Walk `std::error::Error::source()` to the innermost cause and append it, deduplicating when the
outer message already contains it. One helper beside `failure_label`, used by both it and
`exhausted_transient_label` -- the REPL's end-of-turn message has the same problem.

Bound the output: a chain is arbitrarily long and this is a status line, so take the innermost
link rather than joining all of them.

## Why not in 0112

0112 is a change to how long the loop waits, and it did not touch the classification or the
message. Rewriting what every transport failure prints -- including read timeouts, which are the
common case -- is a separate change with its own before/after to look at.

## See also

- `crates/outrig-cli/src/llm/retry.rs` -- `failure_label`, `exhausted_transient_label`.
- `plan/done/0112-connect-failures-are-not-really-transient.md` -- the split that made the
  distinction visible in the wait but not in the words.
