# After an endpoint refuses the thinking-binding beta, earlier thinking goes back as text

## Context

rig 0.45 asks Anthropic for `thinking.block_binding = drop_block`, under the
`thinking-binding-controls-2026-08-01` beta, for the models its catalog marks as binding their
signed thinking to the conversation. It keys that on the wire's dialect name and offers no
off-switch, so an endpoint behind `base-url` that refuses the beta is handled in OutRig: the
candidate's `Binding` (`crates/outrig/src/agent/{build,failover}.rs`) moves the rest of the
session to a twin model on a renamed dialect, `anthropic-unbound`, which rig does not bind.

rig stamps each reply's `origin.provider` with the dialect name, and replays a signed thinking
block natively only to the origin that wrote it; to any other it sends the block's text as plain
text. So a signed block written before the first refusal -- by the `anthropic` twin -- goes back
as text once the session is on `anthropic-unbound`, for the rest of the session. Blocks written
after the move replay whole. Only a binding model at a gateway without the beta is affected, and
only for the turns before the refusal, which is the first call that carries signed thinking at
all; the cost is the model re-reading that reasoning as prose rather than a refused call.

## Shape

- Upstream: a `Messages` setting that turns binding off for a dialect, so the twin is not needed
  and the origin does not change. Then `Binding` holds one model and a flag.
- Until then, nothing: the first refusal happens on the first call whose history holds signed
  thinking, so at most one turn's reasoning replays as text.

## Acceptance

- `a_refused_drop_block_is_sent_again_without_it` asserts the third call's thinking block is
  sent whole; a test of a session whose second turn is refused would show the first turn's
  block as text, and passes once the off-switch exists.
