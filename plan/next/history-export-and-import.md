# Hand a session's conversation to a new session

## Context

The conversation store is host-side: every committed turn under an id that never changes,
mirrored into the interpreter as `runtime.history` (`crates/outrig/src/agent/history.rs`,
`plan/phase/0003-python/history.md`). It is crate-private, holds rig `Message`s, and ends with
the session.

The phase 0003 design brief (now in `plan/phase/0003-python/embedding.md`) asked for a versioned
export and import for an embedder: after an agent abdicates a task, the embedder ends that session
and continues the same conversation in a new one. It marked it "needed later, not for the one-shot
proof" and left open whether the first release has it. Phase 0003's embedding API does not.

## Shape

- An owner-side export on `Session`, returning the conversation in a versioned, serializable
  format of OutRig's own. No rig types, as the rest of the embedding API keeps them out.
- An import on the builder: the new session starts with those turns in its store, under the same
  ids, mirrored into its interpreter.
- What does not move: the namespace, references to bound objects, running work and child
  handles. The imported turns mention names the new interpreter does not hold, so the new session
  opens with the notice `plan/next/interpreter-restart-with-a-reset-notice.md` describes: what
  was lost, and that nothing was run again.
- Import runs no source. A conversation's executions are a record, not a script.

## Open questions

- Provider-specific content -- reasoning blocks and their signatures, tool-call ids -- and
  whether a conversation exported under one provider can be sent to another. `history.md`'s
  unverified note on cutting a conversation between turns is the same question.
- What an import does with a format version it does not know: refuse, naming the version, or
  convert.
- Whether promotions, the per-call view manifests and usage travel with the turns.
- The export holds message bodies and output the model saw. The format should say so; storing it
  is the embedder's responsibility.

## Acceptance

- Exported from one session and imported into a new one, `runtime.history.turns` has the same
  ids and content, and the next model call's view holds the turns the old session's would have.
- An unknown format version is refused with the version named.
- Import starts no execution.
