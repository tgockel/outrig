# A turn abandoned to a broken endpoint loses the tool calls it already ran

`handle_prompt_error`'s exhausted-endpoint arm returns `Vec::new()` as its chat
history, because `rig::completion::PromptError::CompletionError` carries no
`chat_history` (rig 0.40, `src/completion/request.rs:148-189`) -- unlike
`PromptCancelled` and `MaxTurnsError`, which both do.

The unusable-response arm beside it has the same shape and the same loss; both
want whatever fix this gets.

For a turn that dies on its *first* model call that is exactly right: nothing
happened, and "history unchanged -- send the prompt again" is true. But a turn
that dies on a *later* model call has already executed container tool calls,
and those are dropped along with the model's intent. Resending the prompt
re-runs them. For a read they are wasted work; for a write they are a repeat.

This is why the recovery message says "history unchanged" rather than the
"partial history retained" the other two arms use -- the wording is honest
about what is kept, but the underlying loss is real.

## Sketch

Two routes, neither cheap:

1. Upstream: give `PromptError::CompletionError` a `chat_history` the way the
   other loop-ending variants have one. rig already threads it through the
   prompt loop; this is a variant-shape change, so it wants a PR and a release.
2. Local: accumulate history in `OutrigPromptHook` as the turn runs, so outrig
   holds its own copy independent of what rig hands back on the way out. The
   hook already observes tool calls and results. The risk is divergence -- two
   sources of truth for the same message list, and the splice in
   `extend_history_with_new_suffix` exists precisely because they can disagree.

Route 2 is self-contained and probably right, but only if the hook's copy
becomes *the* copy rather than a second one.

A Ctrl-C mid-turn has the same shape. Since #170, `run_repl`'s
`TakenHistory` guard puts the prior history back when the REPL drops the
turn, but the interrupted turn's own tool calls are lost with rig's copy, so
the next prompt may repeat them. Route 2 could serve both if the hook's copy
lives outside the future the REPL drops. A lost turn also strands any
subagent result it read: `a-lost-turn-consumes-its-subagent-result.md`.

## Acceptance

- A mock-HTTP test with a script of `[tool_use, 429, ...]` and retries off:
  after the turn ends, `history` holds the assistant's tool-call message and
  the `tool_result` that followed, in protocol-valid order.
- The recovery message switches to the "partial history retained -- send
  another prompt (e.g. \"continue\")" wording when the history is non-empty,
  and keeps the "history unchanged" wording when it is not.
