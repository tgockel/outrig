# 0003-58 -- History has a format of OutRig's own

## Context

The conversation store is Rust's and rig-typed. `History` (`crates/outrig/src/agent/history.rs:
436-442`) holds a `Store` of `Turn`s whose `messages` are rig `Message`s (`:607-627`); `commit_in`
(`:529-541`) appends a turn, emits `turn.committed` with the messages as `rig_json` (`:351-353`),
and pushes a mirror into the interpreter (`push_turn`, `crates/outrig/src/python/host.rs:
1492-1500`). The mirror is a projection: `mirror` (`history.rs:1043-1092`) keeps the prompt, the
reply text, each call's source and result text and the incomplete flag, and drops reasoning,
tool-call ids and every other part, so `runtime.history.turns` (`crates/outrig/src/python/
interpreter.py:3306-3360`) cannot stand in for the store. Each model call's view is chosen from
the store (`history.rs:854-870`, `view`) and recorded as a manifest (`:143-172`), whose
`left_out` (`:177-181`, `:186-228`) names each reasoning part the call's provider cannot take, by
the per-wire rule `Wire::takes` applies (`crates/outrig/src/agent/budget.rs:187-217`): unsigned
reasoning is left out of a call to an Anthropic model, and an OpenAI-compatible model takes
everything. Promotions reach the store from `Context.promote` and `demote`
(`interpreter.py:3393-3410`) through `on_context` (`host.rs:1506-1510`) and `Store::change`
(`history.rs:719-742`). Nothing exports a store and nothing imports one: the only export is the
`turn.committed` event, and the store ends with its session (the code map, section 4).
`History::new` is made in `Agent::assemble` (`crates/outrig/src/agent/mod.rs:286`), before any
turn, which is where an import seeds.

OR-SUS5 asks for "a native, identified/versioned resumable format with full canonical model
history, relevant context-selection state, logical Session/agent relationships", validated on
import; OR-SUS7 asks that "saves belong to OutRig, not provider request JSON, dependency blobs or a
provider conversation handle", that canonical visible and tool-execution content and "typed
opaque parts with provenance" are preserved, that a Chat Completions to Responses continuation
keeps compatible history "while explicitly omitting or refusing incompatible opaque
reasoning/signatures", and that nothing is invented, rewritten or promised portable; it names
#716 as what supplies Responses routing with client-supplied context.

**This task reverses a recorded decision.** `doc/reference/events.md`, `turn.committed`, says of
`messages`: "the turn's messages, in rig's own JSON form, which can change when OutRig upgrades
rig" -- and the upgrade to rig 0.45 did change it: every item is tagged by `type`, ids are
objects, and a model's turn carries `origin` and `stop`, so `scripts/render-session.py` reads
both shapes; `plan/next/history-export-and-import.md` says the store "is crate-private, holds rig
`Message`s, and ends with the session." The validation (reversal 5) found that this extends
`embedding.md`'s rule -- no rig type in the public surface, so "a rig release cannot force an
`outrig` major" -- to the persisted form, which the deferred export entry already asked for, and
that the per-candidate eligibility of reasoning parts in `budget.rs` is the precedent for typed
opaque parts. The adversarial review that fed the requirements (issue 1) named export and import
at a quiescent boundary the "simplest spike", and `embedding-contracts.md`, section 1, found that
exporting `runtime.history` would lose provider reasoning, signatures and message structure.
`continuation.md`, "The format", designs what this task builds and absorbs
`plan/next/history-export-and-import.md`, whose open questions are this task's forks.

Downstream, for the record: `0003-59` fills the selection field and depends on this task.

## Goal

A conversation is exported from a session at a quiescent boundary in a versioned format of
OutRig's own, with typed opaque parts carrying their provenance, and imported into a new session's
builder under the same turn ids, with incompatible opaque parts omitted and recorded rather than
sent.

## Deliverables

- **The format.** A versioned, identified document of OutRig's own, with no rig type in it, in a
  public module of `outrig::harness` (the name is this task's): a turn is a round number, an
  incomplete flag and ordered messages; a message is a role and ordered parts; a part is text, a
  tool call (id, name, arguments), a tool result (the call's id, ordered content), or a typed
  opaque part with `provider`, `kind`, the provider's bytes or JSON, and its provenance -- the
  provider row, the model identifier and the call id that produced it. Beside the turns: the
  promoted ids, the round numbering, and optionally each call's manifest and per-agent usage
  totals (fork 4). Each agent's store is one entry under its agent id with its parent, as the
  children's registry knows them (`crates/outrig/src/agent/children.rs:99-112`). `provider` is the
  provider row's `style` value, so a style added later needs no format change.
- **Additive, with continuation metadata.** Within one version a reader keeps a field it does not
  know and a writer may add one, so a later task extends the document without a new version.
  Each agent's entry carries a continuation-metadata section with two fields: the agent's
  model-request counter and limit (`0003-50`, OR-A4-2: a resumed agent reports its spent count,
  and the owner may set a fresh limit at resume), which this task carries, since `0003-50` lands
  before it; and its alias selection state -- the alias, the member by name, and the order at the
  time (`0003-59`, OR-A5-2: restored by member name, so a catalog change does not shift the pin,
  and a member no longer present falls back to the head with a notice in the resume notice),
  which this task names and leaves empty for `0003-59` to fill. `model-policy.md` owns what both
  fields mean.
- **Conversions both ways**, in Rust: store to format at export and format to store at import. A
  part the store's type cannot hold is kept beside its turn and never sent (fork 2), and the
  manifest of a call that would have carried it records it as left out with a reason.
- **The compatibility rule**, as `continuation.md`, "The format", states it per provider pair:
  Anthropic signed or redacted reasoning goes back whole to Anthropic and is omitted and recorded
  toward any other provider; Chat Completions `reasoning_content` is left out toward Anthropic as
  today and goes whole to Chat Completions; Responses encrypted reasoning is omitted and recorded
  toward every provider, Responses included, while #716 leaves its eligibility unresolved; text,
  tool calls and tool results always go whole. The manifest's `left_out` gains the reason, and the
  omitted parts are counted for the resume notice `0003-64` composes. The rule covers a move an
  alias makes between providers in the middle of a session, not only a resume (OR-A5-3): the
  next call's `model.call` records each part omitted in `left_out` with its reason, visible and
  tool history go whole, and a cut taken afterwards records the omissions in its manifest. Fork 5
  is where the view's per-call rule and this one become one.
- **`Wire::takes` is provenance-aware.** The view's per-wire rule becomes the pair rule above, so
  one table decides an import and every later call, and an alias whose members cross providers
  (`0003-59`) applies the same table; the per-wire rule as it stands today is recorded in
  `potential/cost-aware-commentary-preamble.md`.
- **Export at a quiescent boundary.** `Session::export_history()` (the spelling is this task's)
  reads every agent's store under the store's lock and returns the document; it refuses, naming
  what runs, while a round is running or an execution holds a slot (`crates/outrig/src/harness/
  lifecycle.rs`, the session's state; `crates/outrig/src/python/host.rs:971-1012`, the slot) or a
  child is not idle (`children.rs:127-134`, `Life`). `0003-62` calls it inside the cut.
- **Import on the builder.** `SessionBuilder::history(document)` (the spelling is this task's)
  refuses an unknown version by name, validates every turn against the model, and seeds the store
  before the agent is built, so `Agent::assemble` finds the turns in place; once the kernel opens,
  each imported turn is mirrored through `push_turn` under its original id, promotions are restored,
  and round numbering continues from the last imported round. Which agents' stores are restored is
  fork 7. Import runs no source. When the root is owner-constructed (`0003-46`, landed before this
  task), the document's root store is attached to the `ConstructSpec` by the agent id the manifest
  names: the builder form seeds it as the root's kernel opens, before its orienting round;
  `Session::construct` on a started session names the import in its spec and seeds the same way; an
  import whose root no spec constructs is refused at the start, naming the id. This task builds the
  attachment and the refusal.
- **The Python owner.** `outrig.Session` (`crates/outrig/src/python/outrig/_host.py:661-800`)
  gains `export_history()`, answered over the owner connection as JSON and handed to the program
  as a dict, and `outrig.Session(history=document)`, sent with `session.start`
  (`crates/outrig/src/owner/mod.rs:12-28`); in the program the document is a Python object, and
  JSON crosses the connection only.
- **`turn.committed`**, per fork 1.
- **Events**: `history.exported` (agents, turns, the stream sequence) and `history.imported`
  (version, agents, turns, parts omitted and why), execution diagnostics, added to
  `doc/reference/events.md` with their category; the import's turns are not re-emitted as
  `turn.committed`.
- **`lifecycle.md`'s close table** gains a row: an export in progress at the close finishes under
  the store's lock or is refused; nothing is left waiting.
- **`## Decisions` records the reversal**: the two sentences quoted above and what replaced each;
  that the store's in-memory type stays crate-private and what it is; the compatibility rule as
  built, with the Responses column's status against #716; whether `turn.committed` changed; what
  travels beside the turns; which stores an import restores; and the continuation-metadata
  section's shape, with the counter carried here and the selection field left for `0003-59`.
- `crates/outrig/public-api.txt` regenerated.

## Acceptance

Each is a mock-driven test unless it says otherwise.

- **Round trip.** A session whose mock provider produced text, tool calls and results, an
  Anthropic signed reasoning block, a redacted one, an OpenAI `reasoning_content` part, an
  incomplete turn and two promotions is exported; a new session imports the document;
  `runtime.history.turns` has the same ids, rounds, prompts, texts, sources, results and
  incomplete flags; `runtime.context.promoted` is the same; and the next model call's `model.call`
  carries the turns the old session's would have.
- **An unknown version is refused with the version named**, before anything starts; a document
  whose turn fails validation is refused naming the turn and the field.
- **Opaque parts travel typed.** The exported document holds each reasoning part with its
  `provider`, `kind` and provenance; a part of a kind the importer does not know is kept opaque,
  never sent, and recorded as left out.
- **The compatibility rule.** Exported under an Anthropic mock and imported into a session on a
  Chat Completions mock: the signed block is not on the wire, the first `model.call` names it in
  `left_out` with its reason, and `history.imported` counts it. Imported into a session on an
  Anthropic mock: the block goes back whole. The `reasoning_content` part goes whole to the Chat
  Completions mock and is left out toward the Anthropic one. Text, calls and results are on the
  wire in every pairing.
- **The quiescent boundary.** An export during a round is refused naming the round; during an
  execution, naming its `execid`; with a child that is not idle, naming the child. One between
  rounds, with every child idle, succeeds.
- **Import runs no source.** No `exec.submitted` is recorded by an import, and a turn whose source
  writes a file leaves the file unwritten in the new session's workspace.
- **Relationships travel.** A session with a primary and two children exports three stores with
  the children's parent and depth; the import restores per fork 7 and records the rest.
- **Additive.** A document with an unknown field in a known version imports with the field kept
  and re-exported unchanged; the continuation-metadata section exists per agent with its two
  field names, the model-request counter and limit carried from `0003-50`'s state and restored
  into it on import, the selection field empty, and a document whose selection field holds
  values the task does not read imports without error.
- **A mid-session move.** An alias whose first member is an Anthropic mock and whose second is a
  Chat Completions mock fails over mid-session: the next `model.call` names the signed reasoning
  omitted in `left_out` with its reason, visible and tool history are on the wire, and an export
  taken after records the omission.
- **An import attaches to a construct spec.** A document whose root store names agent id `r` imports
  into a session whose builder-form `ConstructSpec` carries `r`: the root's orienting round opens on
  the imported history, numbered after its last round; `Session::construct` with the same import on
  a started session does the same; a document naming a root no spec constructs fails the start
  naming `r`.
- **The Python owner.** `export_history()` in a host program returns the document Rust would, and
  `outrig.Session(history=document)` starts a session whose `runtime.history.turns` matches.
- **`turn.committed`** as fork 1 decides, with the renderer's test fixture updated if its form
  changed.
- `crates/outrig/public-api.txt` regenerated; `cargo test --workspace`,
  `cargo clippy --all-targets`, `cargo fmt --check` pass.

## Design forks

1. **What `turn.committed` carries -- Recommended: the canonical form, in place of rig's JSON.**
   One schema then serves the event log, the renderer and the export, and the reversed sentence
   is replaced rather than left half true. The alternative keeps rig's JSON on the event and adds
   the canonical form nowhere in the log, which leaves the renderer reading a form that can change
   with rig.
2. **The store's in-memory type -- Recommended: it stays rig-typed, with a side table for parts
   rig cannot hold**, keyed by turn, message and part and recorded in `left_out`. The alternative
   makes the store canonical and converts to rig at assembly, which is the larger change and is
   not needed for the export to be OutRig's.
3. **Whole or incremental encoding -- Recommended: whole, one document per export.** `0003-12`
   measured about 16.7 MB of JSON per thousand 16 KiB turns (`history.md`, "The store"), which a
   save carries once. Incremental appends are the alternative, to be chosen on a measurement of
   save frequency times size, not now.
4. **What travels beside the turns -- Recommended: the per-call manifests and per-agent usage
   totals, each optional.** Manifests are ids and are "the only precise answer to 'what did this
   decision have available'" (`history.md`, "Promotion"); usage comes from the ledger as totals of
   unique attempts, with no price. The alternative carries turns and promotions alone.
5. **Whether `Wire::takes` becomes provenance-aware -- Decided on review: yes, a deliverable
   above**, so one rule decides both an import and every later call, and a chain whose members cross
   providers applies the same table; the mid-session move `model-policy.md` describes depends on it.
   The alternative, rejected, leaves the view's per-wire rule as it is and applies the pair rule at
   import alone, which lets a later failover send what the import would have omitted.
6. **Omit or refuse -- Open; recommended: omit and record by default, with a strict option that
   refuses.** OR-SUS7 allows either; omission keeps the conversation usable and the record honest,
   and a caller that wants fidelity or nothing asks for it.
7. **Which stores an import restores -- Recommended: the root's only** -- the primary's, or the
   owner-constructed root's, attached to its `ConstructSpec` by agent id as the import deliverable
   says -- with every other store carried as a record the resumed session does not restore.
   `0003-65` decides children's dispositions and may widen this.

## Dependencies

- **Hard: `0003-11`, `0003-12`.** The store, the view, the budget and `left_out`.
- **Hard: `0003-19`.** The session API the export and import join, and the event stream.
- **Hard: `0003-28`, `0003-29`.** Sessions with no primary agent, and the Python owner the
  export and import reach over the owner connection.
- **Hard: `0003-30`.** The children's registry, whose relationships the document carries.
- **Hard: `0003-50`.** The per-agent model-request counter and limit, which the
  continuation-metadata section carries; it lands before this task under the phase's numbering.
- **Hard: `0003-46`.** The owner-constructed root an import restores as the root, attached to its
  `ConstructSpec` by agent id.
- **Soft: `0003-35`.** A host-declared root child's store.

## See also

- `plan/phase/0003-python/continuation.md` -- "The format", "One continuation: a new
  session", "What a package holds, and what it never holds".
- `plan/phase/0003-python/history.md` -- the store, the view, promotion, and the unit of both.
- `plan/phase/0003-python/embedding.md` -- "Stable at 0.3.0, not before", "Not in this phase".
- `plan/next/history-export-and-import.md` -- absorbed by this task and `continuation.md`.
- `plan/phase/0003-python/potential/cost-aware-commentary-preamble.md` -- the per-wire rule as it
  stands.
- `crates/outrig/src/agent/history.rs`, `budget.rs`, `mod.rs`; `crates/outrig/src/python/host.rs`,
  `interpreter.py`, `outrig/_host.py`; `crates/outrig/src/owner/mod.rs`.
