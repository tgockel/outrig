# 0110 -- Model aliases: one name for a model, or for an ordered set of equivalents

## Context

A `[models.<name>]` entry is terminal today. `doc/concepts/llm-providers.md` says it "picks a
specific identifier on a specific provider", and the resolver agrees: `resolve_agent_with_overrides`
(`crates/outrig-cli/src/llm.rs:248-419`) does one `cfg.models.get(model_name)`, one
`cfg.providers.get(&model.provider)`, and one match producing a single
`(resolved_provider, model_weights, model_identifier)`.

That layer already exists to buy indirection, and the doc says so outright:

> The model layer exists so that agents can refer to a stable name (`fast`, `smart`) and swap the
> underlying API model without touching every agent. If OpenAI renames a model, you edit one
> identifier; every agent using that name picks up the change.

It buys exactly one hop, and the hop is spent on the *wire identifier*. Two things it cannot
express:

- **A name for a name.** `opus` meaning `opus-5` today and `opus-6` next quarter. Editing the
  identifier under `[models.opus]` works only while `opus` and `opus-5` are the same row. The
  moment you want both names to exist -- `opus-5` pinned for reproducing a result, `opus` floating
  for daily work -- you are copying a block and keeping two rows in sync by hand.
- **A name for a set of equivalents.** The same weights are sold by three vendors:
  `opus-5-bedrock`, `opus-5-anthropic`, `opus-5-azure`. They are one model to the user and three
  `[models.<name>]` rows to outrig, differing only in which `[providers.<name>]` they sit on. There
  is no way to say "this one, or that one if the first is unavailable", and no way to say it once
  for every agent.

The second case is where the real cost sits, and it has two distinct triggers that get conflated:

- **Which vendor am I credentialed for?** A laptop with `ANTHROPIC_API_KEY` set and a CI runner
  with a Bedrock role want different rows out of the same committed config. Today the answer is to
  edit `default-model`, or keep divergent configs, per machine.
- **Which vendor is up right now?** A rate-limit window on one endpoint ends the turn
  (`handle_prompt_error`, `crates/outrig-cli/src/llm.rs:1092`) even though two other endpoints
  serve the same weights.

These want different mechanisms, and they are separable. This entry calls them the **static half**
(select a candidate once, at session start, from what this build is configured to reach) and the
**runtime half** (move to the next candidate when one fails mid-session). Only the static half is
in the first pass; both are specified under Runtime behavior, because the config surface has to
serve both.

## Goal

Let a `[models.<name>]` entry name one or more other models instead of a provider, so a fleet name
can be repointed in one place and a set of provider-equivalent rows can be named once and selected
between.

## Deliverables

- **An `alias` key on `[models.<name>]`**, taking one model name or an ordered list of them, and
  mutually exclusive with `provider` and every provider-shape field. An alias is a model, in the
  models table, with one namespace and one lookup -- see Config surface.
- **`Model` reshaped on the `ImageConfig` pattern**: `provider` becomes `Option<String>`, `alias`
  joins it, validation enforces exactly one of the two shapes, and a discriminated accessor
  (`Model::source() -> ModelSourceRef`) is how readers ask which shape they have. **This is a
  breaking change to a public field** -- `Model::provider` is published as `String` in
  `crates/outrig/public-api.txt:430` -- which is affordable only while the 0.2.0 window is open;
  see Risks and Design forks §1.
- **`Model::alias(...)` beside `Model::new(provider)`**, so each shape has a constructor and
  neither needs a struct literal `#[non_exhaustive]` forbids anyway. `Model::new`'s signature does
  not change.
- **Validation** in `crates/outrig/src/config/validate.rs`, beside the existing model rules: every
  target names an existing model; no cycles; a non-empty target list; no provider-shape field on
  an alias and no `alias` on a provider-shape model. Five new `ConfigValidationError` variants;
  proposed names under Validation below.
- **Resolution** that walks the alias graph to an ordered list of concrete model names, then
  selects one, in `crates/outrig-cli/src/llm.rs`. The selection rule is the static half below.
- **`ResolvedAgent` records both names.** `model_name` keeps meaning the concrete row -- it is the
  `LlmRegistry` key, see Risks -- and a new `alias_name: Option<String>` carries the name the
  caller asked for, `None` when no alias was involved.
- **`usable_model_names` restructured**, not merely extended -- see Subagents for why the change is
  larger than it sounds.
- **Attribution** wherever a model name is already printed: the banner (`print_banner`,
  `crates/outrig-cli/src/cli/run.rs:746`), the subagent launch trace and transcript header
  (`ModelLabel`, `crates/outrig-cli/src/subagent/mod.rs:585`). Formats under Attribution below. A
  direct model name prints exactly as it does today, byte for byte.
- **Docs**: `doc/concepts/llm-providers.md` (a real file -- the layering prose and the mermaid
  diagram both assume one provider edge per model), `doc/reference/config.md` and
  `doc/concepts/subagents.md` (**symlinks** into `crates/outrig-cli/src/mcp_self/docs/` -- edit the
  targets), `doc/reference/cli.md` (a real file -- `--model` and the `--device` interaction).
- **`crates/outrig/CHANGELOG.md` and `crates/outrig/public-api.txt`**, the latter regenerated
  deliberately.
- **Not in the first pass**: the runtime half. Specified under Runtime behavior because the config
  surface has to be designed for it, but it is separable and much larger -- see Design forks §2.
  It is now queued as `plan/todo/0113-model-alias-failover.md`, which executes that specification;
  this entry stays its authority on the config surface and on the two loose ends it hands over
  (Design forks §4 and the `warn_fallback_ceiling` risk).

## Config surface

```toml
# The simple case: one name for one model.
[models.opus]
alias = "opus-5"

# The interesting case: one name for a set of provider-equivalent rows, in
# preference order.
[models.smart]
alias = ["opus-5-bedrock", "opus-5-anthropic", "opus-5-azure"]

# Aliases may name aliases. This flattens to opus-5-bedrock, opus-5-anthropic,
# opus-5-azure, haiku-5.
[models.default]
alias = ["smart", "haiku-5"]

[models.opus-5-bedrock]
provider   = "bedrock"
identifier = "anthropic.claude-opus-5-v1:0"

[models.opus-5-anthropic]
provider   = "anthropic"
identifier = "claude-opus-5"
```

An alias is a model. It lives in the models table, it is named the way every other model is named,
and every place that already accepts a model name accepts it with no change: `--model`,
`default-model`, `[agents.<name>].model`, and the subagent `model` argument. There is one
namespace and one lookup.

That is not only the friendlier surface, it is the smaller one. A separate `[model-aliases]` table
would need a rule for a name declared in both tables and a documented precedence between them;
inside one table the collision is impossible by construction, because TOML cannot express two
entries with the same key. The rule does not need to be decided, documented, tested, or explained
-- it does not exist. Everything already keyed on `cfg.models` -- `UnknownDefaultModel`,
`UnknownAgentModel`, `usable_model_names`, `LlmResolveError::UnknownModel` -- keeps working on
membership alone.

### Shape

`[models.<name>]` becomes a table with two mutually exclusive shapes, which is a problem this
config has already solved once. `[images.<name>]` is exactly it: `image-name` (pull) XOR
`dockerfile` + `context` (build), every field `Option`, exactly-one-shape enforced by validation
(`ImageSourceMissing` / `ImageSourceConflict`, `crates/outrig/src/config/validate.rs:84-93`), and a
discriminated accessor `ImageConfig::source() -> ImageSourceRef` (`config/mod.rs:1468`) as the way
readers ask which shape they got. Constructors `from_dockerfile` / `from_image_name` give each
shape a construction path.

`Model` follows it exactly:

- `provider: String` becomes `provider: Option<String>`.
- `alias: Option<Vec<String>>` joins it, deserialized by `deserialize_string_or_vec_string`
  (`config/mod.rs:1707`) -- the helper `model-file` already uses, so accepting both a bare string
  and an array has in-tree precedent rather than being a special case.
- `Model::source() -> ModelSourceRef<'_>` discriminates the two shapes, borrowing as
  `ImageSourceRef` does (`config/mod.rs:1373`):

  ```rust
  #[non_exhaustive]
  pub enum ModelSourceRef<'a> {
      #[non_exhaustive]
      Provider { provider: &'a str, identifier: Option<&'a str>, /* weight fields */ },
      #[non_exhaustive]
      Alias { targets: &'a [String] },
  }
  ```

- `Model::alias(...)` joins `Model::new(provider)`, whose signature is unchanged.

The value being a string *or* an array matters for the motivating case: "point `opus` somewhere
else" should cost `alias = "opus-5"`, not an array with one element in it.

The cost is that `provider` changes type, which is breaking. It is worth it here -- one table is
the whole point of the feature -- and it was not worth it for the rejected alternatives, whose
reasoning is under Design forks §1. The release window is open; see Risks.

### Validation

Five rules, five new `ConfigValidationError` variants. Names and messages follow the
`ImageSourceMissing` / `ImageSourceConflict` pattern they are modeled on, so they are proposed here
rather than left to the implementer:

- `ModelSourceMissing { model }` -- neither `provider` nor `alias` is set.
  *"model {model:?}: neither `provider` nor `alias` is set"*
- `ModelSourceConflict { model, fields }` -- `alias` set alongside `provider` or any
  provider-shape field, `fields` naming every offender the way `ImageSourceConflict` does.
  *"model {model:?}: conflicting fields {fields:?} -- set either `alias` or `provider`, not both"*
- `ModelAliasEmpty { model }` -- `alias = []`.
  *"model {model:?}: `alias` must name at least one model"*
- `UnknownModelAliasTarget { model, target }` -- a target naming no `[models.<name>]`.
  *"model {model:?} has alias target {target:?} which does not match any [models.<name>]"*
- `ModelAliasCycle { cycle }` -- `cycle` rendered as `a -> b -> a`.
  *"model alias cycle: {cycle}"*

The existing per-model loop (`validate.rs:605-623`) cannot simply grow an early return. It does one
`cfg.providers.get(&model.provider)` per row, and a cycle is a whole-graph property rather than a
per-row one, so validation becomes **two passes**: first classify every row and check the alias
graph (targets exist, no cycles, non-empty), then run the existing provider-shape checks over the
provider-shape rows only. The second pass is the current loop with its lookup fed by
`ModelSourceRef::Provider` instead of a bare field.

## Runtime behavior

### Flattening

An alias resolves to an ordered list of concrete model names by depth-first walk, splicing a
nested alias's targets in at its position and keeping the first occurrence of a repeated name.
`alias = ["smart", "haiku-5"]` above flattens to
`[opus-5-bedrock, opus-5-anthropic, opus-5-azure, haiku-5]`. Order is the config's, so the list is
stable across runs. That matters for the subagent tool schema:
`plan/done/0101-subagent-model-selection.md` made the schema's model `enum` byte-stable across
launches -- "it is a `BTreeMap`, so the enum is sorted and byte-stable" (0101, under *The enum
lists only models that would work*) -- because an unstable tool schema churns the parent agent's
context for no reason. A user reading a banner deserves the same stability.

Cycles are rejected at validation, not at resolution, because a cycle is a static property of the
config and the earliest honest place to report it is the file that has it. The resolver still
carries a visited-set: it must not hang on a `Config` built by hand in a test or by a library
embedder, neither of which goes through `validate`.

The walk is **one function**, in `outrig`'s config module beside the other `Config` accessors, used
by both the validation pass and the resolver. Two implementations of a graph traversal that must
agree on ordering and on cycle handling would be two chances to disagree, and the ordering is
load-bearing for the tool schema.

### Selecting a candidate: the static half

The first pass selects **at resolve time**, once per session, and the resolved agent is a concrete
model exactly as it is today. A candidate is *selectable* when this build could actually reach it:
its provider exists, its style is one this build has a client for (`Mistralrs` only under
`local-llm`), and -- for a remote style -- the `api-key` env var it names is set and non-empty.
The first selectable candidate wins.

That last clause is what makes the static half worth shipping on its own.
`resolve_agent_with_overrides` already resolves the api key eagerly, via `ApiKeyRef::resolve`
(`crates/outrig/src/config/api_key.rs:42`), and already fails the session when it is unset. So
"which vendor am I credentialed for" is answerable with no network I/O and no new runtime types
beyond what Deliverables already lists -- in particular no change to `RigAgent`, `RebuildingAgent`,
or `build_agent`. One committed config then serves the laptop with `ANTHROPIC_API_KEY` and the
runner with a Bedrock role, and each picks the row it can actually use. `usable_model_names`
already reasons this way for the subagent schema, minus the credential check -- the two predicates
should become one function so they cannot drift.

Building a remote client does no network I/O (`build_agent`, `crates/outrig-cli/src/llm.rs:551` --
"The remote arms do no I/O"), so static selection is deliberately blind to whether the endpoint is
*up*. It answers "am I configured for this" and not "is this working", and the docs must say so, or
the failover use case will be assumed to be covered when it is half-covered.

An alias with no selectable candidate fails the session, naming each candidate and why it was
skipped. A list of three that all fail for different reasons is the case where a single-line error
wastes the user's afternoon, so the message is per-candidate:

```text
no usable model for alias "smart"; tried:
  opus-5-bedrock   -- api-key env var AWS_BEDROCK_KEY is not set
  opus-5-anthropic -- provider "anthropic-eu" is not defined under [providers.<name>]
  opus-5-local     -- provider style mistralrs needs --features local-llm
```

### Selecting a candidate: the runtime half

Not in the first pass; queued as `plan/todo/0113-model-alias-failover.md`. Specified here because
it constrains the config surface above and because the layer it belongs at is not the obvious one.
Where 0113 and this section disagree, 0113 is newer and says why.

The obvious placement -- try the whole turn against candidate 1, retry the turn against
candidate 2 -- is wrong, and `crates/outrig-cli/src/llm/retry.rs`'s module doc already says why:

> rig runs tools *between* `completion()` calls, never inside one, so replaying either a single
> request or a single model call re-executes no container tool call. Retrying the whole
> `agent.prompt(...)` would -- a turn that fails on a *later* model call has already run the tool
> calls from the earlier ones -- which is why neither layer is up there.

Failover inherits that constraint exactly. It has to happen *inside* one `completion()` call, as a
third sibling to `RetryingHttpClient` and `RetryingModel`, or it re-runs container side effects.

Inside one call it is mechanically viable, and two facts checked against rig 0.40 are what decide
it:

- `CompletionRequest` carries `model: Option<String>` and `max_tokens: Option<u64>` as public
  fields (`completion/request.rs:668-694`). A wrapper can retarget the same request at the next
  candidate's wire identifier and ceiling. Without this, candidates would have to be identical
  beyond the endpoint, and `opus-5-bedrock`'s `anthropic.claude-opus-5-v1:0` is not
  `claude-opus-5`.
- Nothing in outrig ever *reads* `CompletionResponse::raw_response` -- it is constructed
  (`llm/mistralrs.rs:591`, `llm.rs:1719`) and never consumed. So candidates with different
  `Response` associated types can be erased to a common one, which is what lets a single
  `CompletionModel` impl stand in front of a heterogeneous set.

`CompletionModel` is not object-safe -- `Clone` supertrait, associated types, `impl Future`
returns, a generic `make` -- so the candidates cannot be `Box<dyn CompletionModel>` directly. It
needs a small object-safe shim (boxed futures, `CompletionResponse<()>`) implemented over the
concrete arms, with `FailoverModel` holding the `Vec` and implementing `CompletionModel` in terms
of it. `type Client = ()` and an unreachable `make`, on the reasoning `RetryingModel::make`'s own
comment already gives (retry.rs:206-211) -- outrig never constructs a model through rig's client
path. That one still delegates to `M::make` rather than panicking, because it has a real `Client`
to delegate with; `FailoverModel` erases the client type and so has nothing to build from.

Two further constraints:

- **Streaming is excluded.** Unifying `StreamingResponse` costs more and buys nothing here:
  outrig's remote turns are non-streaming, and the streaming arm is mistralrs-only, which is the
  one style with no endpoint to fail over from. `FailoverModel::stream` delegates to the first
  candidate, matching the precedent in `plan/next/streaming-path-has-no-http-retry.md`.
- **The retry budget must be shared across the chain, not per candidate.** Three candidates at
  the default `retry-budget-secs = 600` is a thirty-minute turn against a total outage. Worse,
  most of that is spent retrying endpoints already known to be down. This is why the dependency on
  `plan/todo/0112-connect-failures-are-not-really-transient.md` is close to hard: that entry's
  pre-first-byte budget is exactly the signal "move to the next candidate now" as opposed to "keep
  waiting on this one", and without it failover's worst case is worse than no failover at all.

### Everything downstream is unchanged

Because selection produces one concrete model, `ResolvedAgent`, `RigAgent`, `RebuildingAgent`, and
`build_agent` keep their present shape in the first pass -- none of them learns what an alias is.
The runtime half changes exactly one of them (`build_agent` gains a fourth construction path) and
still hands back a single `RigAgent`.

`--device` is **rejected** for an alias that flattens to more than one candidate, reusing
`MistralrsDeviceOverrideUnsupported` and landing beside the existing check at
`crates/outrig-cli/src/llm.rs:306-312`. It selects hardware for one in-process model, and an alias
may span styles; picking a device for whichever candidate happened to win is a silent surprise on a
multi-GPU host. A single-target alias to a mistralrs model accepts it, since there is exactly one
model to mean.

### Attribution

An alias is invisible unless it is printed, and Risks explains why that matters more than it
sounds. Three surfaces, all of them already printing a model name:

- **The banner** (`print_banner`, `crates/outrig-cli/src/cli/run.rs:746`), which today reads
  `agent: coding (model: smart / provider: openai / gpt-4o)`. The model field gains the hop:
  `(model: opus -> opus-5 / provider: anthropic / claude-opus-5)`.
- **The subagent launch trace**, today `[outrig] subagent audit-config started (model: fast)`,
  becoming `(model: opus -> opus-5)`.
- **The transcript header**, written by `ModelLabel::detail`
  (`crates/outrig-cli/src/subagent/mod.rs:602-608`), whose format string is
  `"model: {} / provider: {} / {}"`. The alias hop goes in the first slot only:
  `=== subagent audit-config (model: opus -> opus-5 / provider: anthropic / claude-opus-5) ===`.

In every case the arrow form appears only when an alias was actually involved. A direct model name
prints exactly what it prints today, which keeps the default path byte-for-byte unchanged in the
logs -- the property 0101 established for its own attribution and worth preserving.

### Subagents

An alias is a model name, so `outrig__subagent`'s `model` argument takes one with no schema change
-- `doc/concepts/subagents.md` already promises "a key under `[models.<name>]`", which one table
keeps literally true rather than approximately true. An alias is usable when **at least one** of
its candidates is. That falls out well: `alias = ["opus-local", "opus-anthropic"]` stays offerable
in a build without `local-llm`, where naming `opus-local` directly would not be.

`usable_model_names` (`crates/outrig-cli/src/subagent/mod.rs:557-576`) needs restructuring, not a
new filter clause. It is a flat `cfg.models.iter().filter(...)` doing one
`cfg.providers.get(&model.provider)` per row; with aliases it has to walk the (validated, therefore
acyclic) graph per name and reduce over "any leaf reachable". It is small but it is existing,
tested, order-sensitive code, and it is the same predicate the static half's credential check
wants, so the two should land as one function.

Aliases are also what 0101 argued for without being able to build. Its design section noted that
the `model` argument gets intent-naming for free "where users want it", because names like `fast`
and `smart` are user-chosen; aliases are what let a user have those names *without* duplicating a
model row to get them. They also make 0101's decision 2 -- omit the `model` property from the
schema when only one model is usable -- fire less often, which is fine and worth a line in the
tests that pin it.

## Acceptance

- `[models.opus] alias = "opus-5"` makes `--model opus`, `default-model = "opus"`, and
  `[agents.<n>].model = "opus"` all run the `opus-5` row.
- Repointing that one line moves every agent naming `opus` with no other edit.
- An alias naming an alias flattens depth-first, in config order, first occurrence kept.
- Each of the five validation rules fails with its own variant: a cycle (naming the cycle), a
  dangling target (naming it), an empty list, `alias` together with `provider` or any
  provider-shape field (naming both keys), and a model with neither.
- A hand-built `Config` with a cycle that never went through `validate` fails resolution rather
  than hanging.
- An alias listing three provider-equivalent rows selects the first whose provider is reachable in
  this build and whose api-key variable is set; with only the second variable set, the second is
  chosen and the session runs.
- An alias with no selectable candidate fails with a message naming every candidate and the
  distinct reason each was skipped.
- The banner shows the `opus -> opus-5` hop for an alias and is byte-for-byte unchanged for a
  direct model name.
- The subagent launch trace and the transcript header both show the hop for a subagent launched
  under an alias, and are byte-for-byte unchanged for one launched under a direct model name.
- Two names for the same in-process model -- an alias and its target -- share one loaded engine.
- `--device` with a multi-candidate alias is refused; with a single-target mistralrs alias it
  applies.
- `outrig__subagent` accepts an alias; the schema `enum` lists alias names; an alias whose only
  reachable candidate is remote is offered in a build without `local-llm`.
- `Model::new("openai")` still compiles unchanged, and `crates/outrig/tests/config_schema.rs`'s
  provider assertions pass with the documented one-line migration.
- A config with no `alias` key anywhere behaves identically to today, including the exact text of
  every existing resolve error.

## Design forks

Each item leads with its status: **Resolved** (committed here), **Recommended** (a lean a prototype
should confirm), or **Open** (deferred).

1. **Where aliases live -- Resolved: an `alias` key on `[models.<name>]`, `Model` reshaped on the
   `ImageConfig` pattern.** One table, one namespace, one lookup, and no cross-table precedence
   rule to invent. The alternative -- a separate `[model-aliases]` table -- avoids the breaking
   field change, and that is its only advantage. It buys type-level convenience at the cost of a
   second table users must learn, a collision rule that exists only because the second table
   exists, and a permanent split between two things that are the same thing to the person writing
   the config. `[images.<name>]` already carries two mutually exclusive shapes in one table and is
   the pattern to copy rather than a reason not to. Also rejected: `Model` as an untagged enum, the
   shape `McpServerSpec` uses (`config/mod.rs:1487`). It models the domain best and breaks the most
   -- every field access becomes a match -- for no gain over the `Option`-fields form, which
   validation already has to check either way.

2. **Static half vs. runtime half -- Resolved: static first, and they are separable.**
   The credential-selection case is most of the value at a small fraction of the cost, needs no new
   runtime types, and is what makes one committed config serve a laptop and a CI runner. The
   runtime half needs an object-safe shim, a shared retry budget, and the connect-failure split
   before it is a net improvement. The config surface is identical for both, so the second half is
   additive to the first -- which is the property that makes splitting them safe rather than merely
   convenient. The second half is `plan/todo/0113-model-alias-failover.md`, which resolves the
   shared-budget question as a chain-scoped deadline and takes 0112 as a hard dependency.

3. **The key name -- Resolved: `alias`.** It reads correctly for the case that motivated the
   feature (`alias = "opus-5"`) and acceptably for the list, where the entry is a set of
   equivalents rather than a second name for one thing. Rejected: `models = [...]`, which reads
   better for the list and produces the absurd `models.smart.models` for the singular case;
   `prefer = [...]`, which reads well for the list and wrongly implies a soft preference in the
   singular case. One key covering both shapes is deliberate -- two keys would take the validated
   shapes from two to four. If writing `doc/concepts/llm-providers.md` proves the word genuinely
   does not fit the layering prose, changing it is a rename confined to this feature's own surface,
   but it is not a question the implementation should wait on.

4. **Per-candidate overrides -- Open.** Candidates may legitimately want different `max-tokens`.
   They already can: each candidate is a full `[models.<name>]` row and carries its own. What is
   *not* settled is that `resolve_agent_with_overrides` folds `model.max_tokens` into the agent at
   resolve time (`llm.rs:413`), so under the runtime half the ceiling would stay the first
   candidate's while the identifier moved. This is the concrete reason `FailoverModel` must rewrite
   `request.max_tokens` per candidate and not only `request.model`. Nothing to decide for the
   static half; do not let the runtime half forget it.

5. **`max-tokens` on the alias entry itself -- Open, and deliberately forbidden for now.** An
   alias carrying a ceiling for whichever candidate wins is coherent and occasionally useful. It
   is excluded in the first pass because "an alias has no provider-shape fields" is a rule with
   one clause, and "an alias has no provider-shape fields except `max-tokens`" is a rule with an
   exception in it. Relaxing later is additive; tightening later is not.

6. **Whether an alias may name a mistralrs model -- Recommended: yes, with no special case.** A
   local model in a candidate list is the natural spelling of "use the local one if this build has
   it, otherwise the hosted one", and the "usable if any candidate is usable" rule already handles
   it. The cost is that `--device` gets refused for such an alias (above) and that a cold weight
   load can be selected implicitly. Confirm the second against 0101's decision 7, which added a
   one-line announcement before a cold in-process load for exactly this surprise.

7. **`Model::source()` on an unvalidated config -- Recommended: panic, as `ImageConfig::source()`
   does (`config/mod.rs:1465-1481`).** Consistency with the pattern being copied, and every real
   path goes through `Config::load`, which validates. The alternative -- returning an `Option` --
   pushes a branch onto every caller for a state validation has ruled out. Note this does not cover
   the cycle case, which is why the resolver keeps its visited-set regardless.

8. **Alias-aware allowlists -- Open, but note the ordering.**
   `plan/next/subagent-model-allowlist.md` proposes constraining which models a subagent may be
   launched under. With aliases, an allowlist naming `opus` and one naming `opus-5` are different
   policies. The allowlist should match **the name as written by the caller, before resolution**:
   allowing `opus` does not allow `opus-5`, and an operator who aliases a cheap name onto an
   expensive model has been fooled by their own config, which is fair. Post-resolution matching
   would make aliases unusable in allowlists, since every alias would have to be spelled out as its
   leaves. Whichever of the two lands second owns this.

9. **`outrig config init` -- Open.** The wizard writes `[models.<name>]` blocks directly
   (`crates/outrig-cli/src/config_init.rs:425`, inside `prompt_models_loop`). Offering to write an
   alias is a natural extension and entirely independent; a generated config with no aliases stays
   valid.

## Risks

- **`Model::provider` changing type is a breaking change to a published field.**
  `crates/outrig/public-api.txt:430` records it as `String`. The window is open -- the workspace is
  at `0.2.0-rc.1` and `crates/outrig/CHANGELOG.md`'s `[Unreleased]` section already carries one
  breaking removal -- so this rides an existing break rather than forcing a new one, but it must be
  taken *before* 0.2.0 final or wait for the next major. That is the one scheduling constraint
  this entry has. Mitigations: `Model::new(provider)` is unchanged, so the common construction path
  does not move; in-tree readers are few (`validate.rs:610`, `llm.rs:299-404`,
  `subagent/mod.rs:562`, and the assertions in `crates/outrig/tests/config_schema.rs`); and the
  migration is one line per reader.
- **`deny_unknown_fields` makes the rollback asymmetric.** `Model` carries it
  (`config/mod.rs:489`), so a config using `alias` is rejected outright by an older outrig rather
  than degrading. That is the correct behavior and worth stating in the docs, since a shared repo
  config with an alias in it breaks collaborators who have not upgraded -- the same hazard
  `plan/next/user-toolsets.md` raises for user-local image references.
- **`LlmRegistry` is keyed by the model name, and that name is about to be ambiguous.** It caches
  one loaded engine per key (`crates/outrig-cli/src/llm/registry.rs:27`). If the alias name reaches
  it, `opus` and `opus-5` naming the same GGUF load the same multi-gigabyte weights twice, in one
  process. This is the reason `ResolvedAgent.model_name` must keep meaning the *concrete* row and
  `alias_name` must ride beside it, rather than the alias simply overwriting the name for display
  purposes. It is a one-line mistake with a several-gigabyte symptom; the acceptance list pins it.
- **Static selection is silent by construction.** Picking a candidate from a list because of an
  environment variable is exactly the kind of decision that is invisible until it is wrong -- a
  user with both keys set gets the first-listed vendor and no signal. The Attribution section is
  not cosmetic here, it is the mitigation.
- **An empty environment variable is not an unset one.** `std::env::var` returns `Ok("")` for
  `FOO=`, so `ApiKeyRef::resolve` succeeds and the request fails at the endpoint with a
  provider-side auth error. Selection should treat empty as unset. Deliberately stricter than
  `resolve` rather than a change to it: making `resolve` reject empty values changes an existing
  error path for direct model names, which is a separate decision.
- **Aliases widen what a config typo can silently do.** Today `--model opus5` fails with "not
  defined". With aliases, a config in which `opus` points at a stale row still resolves, runs, and
  bills -- the failure moves from a startup error to a wrong-model session. The banner hop is what
  makes it visible, which argues for printing it every time an alias resolves rather than only when
  the target looks surprising.
- **`warn_fallback_ceiling` fires once per process** (`llm.rs:682`), keyed on the resolved model.
  Under the runtime half a turn can move to a candidate with a different published ceiling after
  the warning has already been spent on the first. Small, and only reachable in the second half.

## Dependencies

- **Scheduling: before 0.2.0 final**, or the `Model::provider` change waits for the next major.
  See the first Risks item. Nothing else in this entry is order-sensitive.
- **Not a dependency of this entry, but of its second half:
  `plan/todo/0112-connect-failures-are-not-really-transient.md`.** Failover across three candidates
  multiplies the retry budget by three unless connect failures are separated from transient ones.
  That entry's short pre-first-byte budget is the signal failover needs to move on quickly; without
  it, a total outage takes thirty minutes to report instead of ten. 0113 takes it as a hard
  dependency; the static half here needs nothing from it.
- **Soft: `plan/next/subagent-model-allowlist.md`.** Design forks §8 -- the two compose cleanly but
  the second to land owns the pre- vs post-resolution matching rule.
- **Soft: `plan/next/public-api-snapshot-gate.md`.** This change edits the public surface, and the
  snapshot is currently maintained by hand.
- **Soft: `plan/next/model-path-runtime-unjoined.md`.** It proposes stamping `ConfigSource` onto
  `Model`, which is the same struct this reshapes. Alias validation errors have the same provenance
  gap -- an alias declared globally and a target declared in the repo report as bare names with no
  file. If both land, they should share the stamping and the reshape rather than touch `Model`
  twice.
- **Landed: `plan/done/0101-subagent-model-selection.md`.** It added the `model` argument to
  `outrig__subagent`, `usable_model_names`, and `ModelLabel` -- the three surfaces this extends.

## See also

- `crates/outrig/src/config/mod.rs` -- `ImageConfig` (1218), `ImageSourceRef` (1373), and
  `ImageConfig::source` (1468): the two-shapes-one-table pattern this copies. `Model` (491),
  `Config` (155), and `deserialize_string_or_vec_string` (1707), the helper `alias` reuses.
- `crates/outrig/src/config/validate.rs` -- `ImageSourceMissing` / `ImageSourceConflict` (84-93)
  and `validate_image_source` (1317), the exactly-one-shape check to mirror; the per-model loop at
  605-623 that becomes two passes.
- `crates/outrig-cli/src/llm.rs` -- `resolve_agent_with_overrides` (248), `ResolvedAgent` (193),
  `build_agent` (551), the `--device` rejection (306-312), `print_banner`'s inputs.
- `crates/outrig-cli/src/llm/retry.rs` -- the module doc's argument about which layer may replay a
  call, which is what places the runtime half inside `completion()`.
- `crates/outrig-cli/src/llm/registry.rs` -- keyed by model name; see Risks.
- `crates/outrig-cli/src/subagent/mod.rs` -- `usable_model_names` (557), `ModelLabel` (585),
  `unusable_model_message` (617), `resolve_launch_model` (636).
- `plan/done/0101-subagent-model-selection.md` -- the subagent `model` argument, the schema-enum
  stability rule flattening reuses, and the intent-naming argument aliases complete.
- `plan/done/0094-non-exhaustive-sweep.md` -- why a field *type* change is not covered by the
  sweep that makes field *additions* free.
- `doc/concepts/llm-providers.md` -- the provider/model/agent layering and the mermaid diagram,
  both of which assume one provider edge per model.

## Decisions

1. **The resolver branches on the model's *shape*, not on how many candidates an alias
   flattens to.** `resolve_agent_with_overrides` matches `model.source()`; the provider arm is
   textually the code that was there before. This makes the acceptance criterion "a config
   with no `alias` key behaves identically to today" true by construction rather than by an
   argument about list length -- and the 34 pre-existing `llm_resolve.rs` tests passing
   untouched is the evidence.

2. **A single-target alias skips candidate selection entirely.** Discovered by a failing test,
   not by design: selection had rejected `alias = "local"` in a build without `local-llm`,
   replacing `MistralrsFeatureDisabled` -- the message that says *rebuild with --features
   local-llm* -- with a "no usable candidate" list of one. An alias over one target is
   renaming, not choosing, so it resolves its target exactly as if the user had typed it and
   keeps that target's own error *and remedy*. Selection filters only where there is a real
   choice. This is narrower than 0110's original rule and is what the acceptance criterion
   "`--device` ... with a single-target mistralrs alias it applies" requires.

3. **`ModelSourceRef` carries only the discriminant** -- `Provider { provider }` and
   `Alias { targets }`, not the weight fields the task sketched. Every reader already holds
   the `&Model` and reads those fields directly, so restating them would be published surface
   with no consumer. Per-variant `#[non_exhaustive]`, as `ImageSourceRef` has, keeps adding
   one additive.

4. **The flattening walk is `Config::model_candidates`, public, returning
   `ConfigValidationError`.** Public because both crates walk the graph and the task forbids
   two implementations; returning the existing validation error rather than a new public type
   because the three failures it reports (empty, dangling, cycle) *are* config-validity
   statements. `From` does not chain, so the CLI call site spells
   `.map_err(|e| CliError::Outrig(e.into()))` rather than widening `CliError`.

5. **The shape and graph rules validate ungated**, outside the `validate_llm` block that
   `outrig build` turns off, for two different reasons worth separating. `validate_model_source`
   is a shape rule that establishes the invariant `Model::source` panics on -- and until
   `provider` became optional, serde's own "missing field" enforced half of it on every path,
   builds included. `model_candidates` is a cross-reference check, but one resolving entirely
   within `[models]`: an alias target is another row, not a provider or a credential, so it
   says nothing about whether this build can reach an LLM. A build therefore still accepts a
   typo'd `provider` while rejecting a typo'd alias target. That looks inconsistent and is not.

6. **`--device` over a multi-candidate alias gets its own error variant.** The task said to
   reuse `MistralrsDeviceOverrideUnsupported`, but its message names the provider the model
   uses, and an alias spanning candidates has no single provider -- the sentence would be
   false. `LlmResolveError` is crate-private, so a variant is free. A *single-target* alias
   still reuses the existing variant, where the message is true.

7. **`usable_model_names` and the alias selector share both the predicate and the traversal.**
   `selectability` answers "could this build reach this row", `first_selectable` walks a
   candidate list with it. Sharing only the predicate would have let a future selection rule
   land in one and not the other. The user-visible consequence is that a model whose api-key
   variable is unset is no longer advertised in the subagent tool schema -- a behavior change
   beyond aliases, and the right one: the function's own doc set the bar at "not a guaranteed
   failure", and an absent key is on the guaranteed side.

8. **`Unselectable` carries the resolver's own errors rather than restating them.** The first
   draft re-authored four messages that already existed as `LlmResolveError` /`ApiKeyError`
   variants, so the same misconfiguration would have read differently depending on whether the
   user named the model directly or reached it through an alias -- precisely the drift the
   shared predicate exists to prevent. Only "set but empty" is new text, because `ApiKeyError`
   has no variant for it.

9. **An empty api-key variable counts as unset for selection, and only for selection.**
   `std::env::var` returns `Ok("")` for `FOO=`, so `ApiKeyRef::resolve` accepts it and the
   request fails later at the endpoint. Making `resolve` reject empty values would move an
   existing error path for direct model names, which is a separate decision. The two differ on
   purpose, in the safe direction: under-advertise, never over-advertise.

10. **`Model::alias` takes an `IntoIterator`.** `Model::alias(["opus-5"])` is marginally
    noisier than the `alias = "opus-5"` it mirrors; a second constructor for the singular case
    would be a second published method meaning the same thing.

11. **`public-api.txt` took only this feature's lines.** A full regeneration also renders
    `std::io::Error` as `core::io::Error` in six places -- a toolchain shift `plan/todo/README.md`
    already identified as non-semantic and deliberately did not take. Folding it into a feature
    commit would be unrelated churn.

12. **`crates/outrig-cli/CHANGELOG.md` got an entry too.** The banner hop, the new resolve
    errors, and the narrowed subagent schema are all user-visible from the binary, and only the
    library changelog was in the task's deliverables.

### Not done, and why

- **The `local-llm`-gated end-to-end "one alias, one loaded engine" test** the plan sketched for
  `llm_registry.rs`. The criterion is covered by composition instead:
  `an_alias_and_its_target_resolve_to_the_same_registry_key` proves an alias and its target
  produce the same `LlmRegistry` key (the one-line mistake with a several-gigabyte symptom),
  and the pre-existing `two_subagents_on_one_local_model_share_one_engine` proves one key
  yields one engine.
- **Unifying `selectability` with `build_agent`'s own preconditions.** Design review's deepest
  finding: `selectability` predicts two later stages (the api-key resolution here, the
  `local-llm` check in `build_agent`) and nothing links them, so a precondition added to either
  goes stale silently. The fix reshapes `build_agent`, which `plan/todo/0113` also rewrites.
  Documented at the definition instead; worth folding into 0113.

### Review follow-ups

Seven defects found in review, all fixed on the same branch.

13. **`SubagentContext` must not derive `Debug`.** A blanket `sed` over `#[derive(Clone)]`
    intended for `ModelLabel` also hit `SubagentContext`, whose `local-llm`-only
    `Arc<LlmRegistry>` field is not `Debug` -- so the default build compiled and the feature
    build did not. The lesson is the editing method, not the type: a pattern-substitution
    across a file cannot see which struct it lands on. The `local-llm` lane is now part of
    verification, and reproducing the derive under it reproduces the exact `E0277`.

14. **Selection has to check the device backend, not just the feature.** `selectability`
    accepted any mistralrs row whenever `local-llm` was on, but a row asking for `cuda` in a
    build without `--features cuda` is rejected by `parse_mistralrs_device` a moment later. A
    multi-candidate alias therefore stranded itself on a model that could not run while a
    hosted candidate sat behind it unused, and the subagent schema advertised it. Now
    delegates to the resolver's own parser, so the device rules are stated once.

15. **The traversal was exponential.** Deduplicating leaves is not enough -- a diamond
    re-enters the shared *alias* node once per path reaching it, so `aN = [aN-1, aN-1]` costs
    2^N visits: 26 rows is ~134 million calls on a valid config, walked on every load. An
    `expanded` set makes it linear and changes no output, since re-expanding could only
    re-emit names `flattened` already holds. Note the original plan specified this set and the
    first implementation silently dropped it while keeping the comment that described it.
    Guarded by a wall-clock test that hangs past 60s without the fix.

16. **Recursion depth was config-controlled.** `on_path` bounds depth at `models.len()`, which
    is not a bound -- a long enough chain aborts the process instead of reporting a bad
    config. `MODEL_ALIAS_DEPTH_MAX` (32) plus a `ModelAliasTooDeep` variant makes it legible;
    tests pin both sides of the boundary.

17. **`alias_with_no_selectable_candidate_names_every_reason` was written for one build.** Its
    third candidate is in-process, so under `local-llm` the alias resolves rather than
    exhausting, and the unconditional `unwrap_err` would have failed that lane. Split by
    feature: the exhaustion assertions where nothing is reachable, selection of the local
    candidate where it is. The split is worth more than the fix -- the exhaustion message is
    only interesting because the selector demonstrably would have taken a reachable candidate.

18. **`init`'s global summary offered shapeless models.** Moving the missing-source check from
    deserialization to validation meant `load_global_summary` (which parses without
    validating) could surface a row with neither `provider` nor `alias`, and choosing it wrote
    a repo `default-model` that the next validated load rejects. Filtered at the source.

19. **The resolver called `Model::source()`, which panics.** Its own doc promises it does not
    assume `validate()` ran, and a hand-built or mutated `Config` reaches it through the
    library API -- so the defensive `ModelHasNoProvider` branch below was unreachable, because
    the panic fired first. It now classifies from the raw fields. `Model::source()` keeps its
    panic, which Design fork §7 chose deliberately; the rule is that callers promising
    totality do not call it.

### Second review round

Six more findings, all fixed. Two were structural; four were contract gaps.

20. **The walk was cubic across roots, and quadratic within one.** Two separate
    costs. Within a root, `flattened.contains` is a linear scan per leaf, so `N` leaves
    cost `N^2` comparisons -- the `emitted` set removed in a simplification pass was
    load-bearing after all, and is back beside the output vector rather than instead of
    it. Across roots, validation called `model_candidates` per row, re-flattening a
    shared subtree once per name pointing at it. `Config::validate_model_alias_graph`
    now walks the whole table once with `expanded` carried across roots, which is sound
    because a node is recorded only after completing without error. Measured: 800 rows
    over one shared alias went from ~1.1s to ~0.01s.

21. **The depth bound depended on sibling order.** The `expanded` short-circuit ran
    before the depth check, so a shared node already expanded via a short branch was
    returned early without its depth being counted -- the same structure passed or
    failed depending on the order its targets were written in. The check moved above the
    short-circuit.

    Worth recording how close this came to shipping unverified. The first test written
    for it passed under mutation, because the chain was long enough to trip the bound on
    its own in *both* orders; the second mutation attempt was a silent no-op, because the
    patch asserted two code blocks existed individually and then replaced their
    concatenation, which a comment between them prevented from matching. Only a probe
    printing the actual error per ordering located the true boundary (a chain of exactly
    `MODEL_ALIAS_DEPTH_MAX - 2`). **A mutation test that does not fail is not evidence
    until the mutation is confirmed to have applied.**

22. **The depth limit bounds recursion, not path length.** With memoization a subtree
    reached through a shorter route is not re-entered, so a graph whose notional longest
    path exceeds the limit can be accepted -- having never recursed that deep. That is
    exactly the property that protects the stack, so the rustdoc now says this rather
    than claiming something stronger.

23. **A row that is both shapes resolved silently.** The resolver treated any row with
    `alias` set as an alias, discarding a `provider` its author meant. The check went
    into the walk rather than the resolver, which catches it for nested rows too, and
    reuses `ModelSourceConflict` rather than adding a variant.

24. **`init` still advertised broken alias rows.** The first fix filtered only shapeless
    rows; empty, dangling, cyclic, over-deep, and both-shapes rows survived. Now filters
    on the full contract -- exactly one shape, and a graph that flattens.

25. **Two rustdoc contract errors.** `model_candidates` documented three errors and
    returns five; the `provider` field's link to `alias` resolved to `Model::alias` the
    constructor rather than the field (`field@` disambiguates).

### Third review round

Three findings. One was a real inconsistency the previous round's own note had
waved through.

26. **Cross-root memoization let an over-deep chain load and then fail at
    resolve.** Round two documented "the depth limit bounds recursion, not path
    length" as a benign consequence of memoizing. It was not benign. With
    bottom-up lexical names (`a00 -> leaf`, `a01 -> a00`, ...) every suffix is
    already expanded when its parent is walked, so validation never recursed
    deep enough to notice a 33-hop chain -- `Config::validate` returned `Ok`
    while `model_candidates("a32")` returned `ModelAliasTooDeep` on the *same
    config*. A config that loads and then dies at resolve time is worse than one
    that fails at load.

    The fix is to memoize each node's subtree *height* rather than a bare
    "seen", and check `depth + height` when re-entering. That makes the bound
    exact and order-independent, so the two entry points agree. The lesson is
    the first-round note, not the code: documenting a known divergence is not
    the same as establishing it is harmless, and "bounds recursion, not path
    length" quietly conceded a load-time/runtime split that nobody would want.

27. **`init`'s filter was still weaker than validation.** The round-two fix
    checked shape XOR plus a flattening graph, which still admitted an alias
    carrying `identifier` / `max-tokens` / `device`, an alias onto a shapeless
    leaf, and a `default-model` naming a row that had itself been filtered out.
    Rather than re-deriving the rules a third time, the walk now enforces the
    *whole* model contract -- shapeless leaves included -- so
    `model_candidates(name).is_ok()` is the single question init asks, and a
    default that does not survive it is dropped instead of inherited. This is
    the third attempt at this filter; each earlier one re-stated a subset of the
    rules instead of calling the one that owns them.

28. **The nested conflict named two fields out of nine.** The walk hard-coded
    `["alias", "provider"]` while the error contract promises every offending
    key. Both sites now share `Model::provider_shape_fields`, which also
    subsumes the `mistralrs_weight_fields` helper the first round extracted.
