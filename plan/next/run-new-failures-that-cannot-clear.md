# `run-new` advises resending after a failure no resend can fix

## Context

`outrig run` tells two kinds of model failure apart. One that may clear -- a rate limit that
outlasted the retry budget, an unreachable host, an exhausted alias chain with at least one such
reason -- ends the turn, and the prompt can be sent again. One that cannot -- a `401` from every
model of an alias, say -- ends the session, because no resend satisfies credentials refused
everywhere (`handle_prompt_error` in `crates/outrig-cli/src/llm.rs`).

`0003-15` brought retry and failover to `PythonAgent` and kept `run-new`'s rule instead: every
failure ends the round, and the session goes on. That was the maintainer's call. The error keeps
the wording every failed round uses: the messages it was told of are still waiting, so send
another to try again, or, after Python ran, to continue. For a failure that cannot clear, that
advice is wrong. The user sees the `401` in the reason, but sending another message fails the
same way.

## Why it waited

- The CLI cannot tell the kinds apart. `PythonAgent::round`'s error is a boxed `dyn Error` over
  the crate-private `AgentError`, and `0003-15` was not to widen the public API.
- Inside the library the classification exists: `agent::retry::is_recoverable` is what a chain's
  exhaustion uses to say "failed" or "failed terminally". Nothing reads it for a lone model's
  failure.

## Shape

Two routes, which compose:

- **Honest advice, no new surface.** Classify where `round.rs` turns a `PromptError` into an
  `AgentError`, and word a terminal failure's advice as "resending will not help until the config
  changes". It changes the existing wording for that case only.
- **End the session, as `run` does.** Needs something public that says the failure cannot clear:
  a method on a public error type, or a richer return from `round`. That is a deliberate addition
  to `PythonAgent`'s surface, best made with whatever next reshapes it.

## Acceptance

- A `401` from every model reads as not worth resending, in `run-new`'s output.
- A `429` that outlasted the budget still says to send another message.

## A typed exhaustion would serve both routes

A chain's exhaustion is rendered into `CompletionError::ProviderError(String)`, and a chain of one
returns its candidate's error unwrapped only so that the round still sees the HTTP status. A
typed `Exhausted { abandoned }`, boxed into `CompletionError::RequestError` the way `TooLarge`
already is, would let the round downcast it once: to word the advice by class, and to read each
refusing candidate's status. Today `refusal_hint` cannot fire for an alias of two or more models,
because `provider_response_status()` reads nothing from a `ProviderError`, so
`doc/reference/cli.md`'s "when a refused call carried one, the error says where" holds only for a
lone model.
