# A preamble that tells the model what its commentary costs

## Shipped

The orientation's one sentence about the model's own text: "What you write yourself is commentary;
send what you mean the user to have." (`orientation.rs`; `discovery.md`, "What goes in the
preamble"). It names the two channels and their audiences, and says nothing of the commentary's
lifecycle: that it is committed to the store, re-sent on every later call the window carries (the
first 2, the recent 6, anything promoted), and readable from code at no context cost through
`runtime.history.turns`.

The harness already accounts for the other channel the sentence leaves unnamed. Reasoning arrives
two ways: as a usage field (`Usage.reasoning_tokens`, recorded per attempt in the ledger) and as
content (`AssistantContent::Reasoning`, whose blocks are signed, encrypted, or redacted).
`budget.rs`'s `Wire` decides which of the latter travels on re-send: Anthropic's signed or redacted
blocks go back whole, unsigned reasoning is left out of an Anthropic call, and an
OpenAI-compatible wire takes everything. So both the commentary and the re-sent reasoning cost
input on every call that carries them.

## Alternative

A preamble that tells the model the cost and the reader: that its commentary is re-sent on every
later call in the window, so it should keep it short -- notes to its future self, not prose for a
reader. The cost fact passes the preamble's own test (`discovery.md`): it cannot be learned by
looking, and it is needed every round.

The reader is "its future self" because standard replies condition later reasoning: the model
treats its own earlier text as a commitment the next round's reasoning builds on, and when a turn's
reasoning does not travel -- unsigned, or the provider returned none -- the plain-text reply is the
only surviving trace of what the model thought. The commentary is exactly this harness's
"standard reply" channel: a reasoning commit, functionally distinct from user communication, which
the model makes by tool call. Keeping it short is therefore not only a token saving; it changes
what the model's future self commits to, and it can erase the lossless record of turns whose
reasoning did not travel.

The stronger variant, "a private scratchpad not user-visible, where extreme shorthand is fine," is
inaccurate in this harness (line mode streams the commentary to stderr, screen mode keeps it in the
detail view) and invites the model to write into a channel the operator can read what it would not
send. The boundary between the two channels is enforced by routing and prompt convention, not by
capability: nothing inspects what the model writes, so the prompt is the only thing keeping unsent
thoughts out of the user channel. And human-oriented shorthand is not low-token conditioning text:
a dense variable dump is often more tokens than a short sentence, and a note cryptic to the model's
own future self costs re-derivation.

## Evaluation

A scripted task run under two or three preambles, compared on the ledger per field, not in
aggregate: output tokens per turn, input tokens across the window (the compounding part), and the
split into `reasoning_tokens` and re-sent reasoning content. A terse preamble may move the model's
internal monologue from the commentary into the reasoning channel, and re-sent reasoning costs
input too; a commentary saving offset by a reasoning increase is not a win. The manifest, which
records what each call carried, is what makes that shift visible.

Parseability is the second criterion, for the commitment reason above: a later round re-reading a
file or re-running a command is what shorthand cryptic to the model's own future self costs, and a
preamble that thins the commentary may also thin the record of turns whose reasoning did not
travel.

Size the expectation accordingly: the commentary is a small slice of the context (tool results
dominate), and the compounding is a per-turn saving times the calls that still carry it -- a 50
token saving re-sent on the six or seven calls that do is a few hundred tokens, a single-digit
percentage of input, not a structural change. The task needs enough turns to show the compounding,
and enough repeats that run-to-run noise does not swallow it.

The cheap first step is the first half of `plan/next/calibrate-the-token-estimate.md` -- recording
the estimate and the reported usage together -- since the estimate is what decides what each call
is sent in the first place, and the A/B reads the same events. The evaluation needs a live model:
the mock-based e2e suite measures plumbing, not register.

## When

A 0.3.1 candidate, once the ledger's per-attempt usage is readable by an owner
(`plan/next/usage-summary-from-the-ledger.md`); until then the comparison is summed from the
`model.attempt` events, which the stream drops under a slow consumer.
