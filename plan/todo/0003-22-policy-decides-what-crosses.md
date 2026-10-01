# 0003-22 -- Policy decides what crosses, and the user can be asked

## Context

`0003-21` sends every hosted request through Rust and records it, and decides nothing: with no
policy configured, every request runs and is evented. That is the default `boundary-policy.md`
settles on -- allow, published as events -- so an operator can see what crosses before choosing
what should not. This task adds the choosing.

What a rule can match on is what `0003-21` already decodes for its event: the binding, the
host-side type of the object the request is made on, the member, and the operation. That is the
whole vocabulary. A rule cannot see what a call goes on to do on the host. An allowed method of a
hosted library may run programs, read credentials or start processes, and `hosted-objects.md`
records that as part of what a binding grants, not as something policy inspects.

Two settled rules shape the rest. A repo's `[policy]` may only add restrictions. That mirrors
the rule `SECURITY.md` states for `[network]`, where a repo config may choose `mode` while
`default`, `allow` and `deny` belong to the operator, enforced by `validate_as_repo` in
`crates/outrig/src/config/validate.rs`. And no approval is reused: a human's allow covers one
invocation, and a rule's deny is final. A request a rule denies never reaches a human, so no human
can override that rule.

The first rule needs the operator's layer of config, and today nothing keeps it. `Config::load`
merges the global file and the repo file into one flattened `Config`
(`crates/outrig/src/config/merge.rs`), a repo entry replacing a global one of the same name, and a
`Model` row records nothing of which file declared it. Read after the merge, a rule, a `default`
or a `[models.<name>]` row could be the repository's. `0003-23` needs the same layer, since the
evaluator's model has to be the operator's choice and not a name a repository redefined. So does
`[events] mode`: `0003-13` merged it as `[network].mode` merges, a repository's value replacing
the global one, and the maintainer has since decided that the global value stands
(`observability.md`), which is possible only with the global file kept apart from the merge.

Escalation is where the CLI and an embedder differ, and the boundary code does not.
`embedding.md` has the embedder install an escalation handler; the CLI's prints each request with
an id and reads `/approve <id>` or `/deny <id>`. A request waiting for an answer can also end
because its caller is interrupted or because the session closes, and either can race an allow
already on its way. `lifecycle.md` requires one host-side transition to decide which of them
wins, so that an allow arriving late runs nothing.

Admission is decided in two places, and `close_admission()` closes Rust's gate and stops the relay
at once; each binding's flag is set when it reads the control line (`lifecycle.md`). Rust's gate
decides executions (`0003-19`), child launches (`0003-25`), and, from this task, every request
held for a decision. Each binding process's closed flag (`0003-21`) decides the requests a rule
allowed: rules run in the binding (fork 1), so such a request never waits on Rust.

## Goal

Rules an operator writes, and a human when a rule asks for one, decide whether each hosted request
runs, and a request that is not allowed never reaches its target.

## Deliverables

- **`[policy]` and `[[policy.rules]]`.** `default` takes any of the four actions -- `allow`,
  `deny`, `escalate`, `evaluate` -- and is `allow` when absent, which keeps `0003-21`'s behavior
  for a session that configures nothing. A rule matches on binding, host-side object type, member
  and operation, any of which it may omit, and names an action. Rules are tried in order and the
  first match wins; a request no rule matches takes `default`. Validation rejects an unknown
  action and an operation name outside `0003-21`'s vocabulary. Matching per fork 3, and a call on
  a callable obtained by an attribute read per fork 4.
- **The operator's layer, kept beside the merged config.** Loading keeps the global config as it
  was read -- its `[policy]`, `[models]` and `[providers]` included -- next to the merged `Config`
  the rest of OutRig reads. The operator's `[policy]` is read from that layer, and `0003-23` reads
  the evaluator's model from it, so no repo entry can replace either. A `Config` given to
  `0003-19`'s builder with no layer kept -- one an embedder built -- is the operator's layer as a
  whole, since a program and not a repository wrote it; `run-new` passes the config it loaded,
  layer included. How the layer is carried is this task's.
- **`[events] mode` is the operator's when the operator sets it.** When the global config sets
  `[events] mode`, that value is the mode, and a repository's value applies only when the global
  config says nothing. A cloned project can then neither turn the user's recording of their own
  session on, retaining the conversation and argument previews they did not ask to keep, nor turn
  it off. The mode governs only the CLI's file, as before (`observability.md`); an embedder's
  `Config` is the operator's layer as a whole, so for it nothing changes.
- **A repo's `[policy]` only restricts**, per fork 2. A repo `allow`, as a rule or as `default`,
  is rejected at load with a message naming it, as `RepoNetworkPolicy` does for `[network]`.
- **`evaluate` is legal before there is an evaluator.** `0003-23` adds the model that judges it.
  Until one is enabled, `evaluate` takes the path an evaluator's failure takes: escalate when a
  handler is installed, otherwise deny. It never allows.
- **A deny raises in the caller before dispatch**, as an exception of OutRig's own class,
  reachable by name from `runtime`, as `runtime.MessageAvailable` is. It is distinct from anything
  a target could raise and from a transport failure, so the agent can tell a call that did not
  happen from one whose outcome is unknown. It subclasses `Exception`, since a denial is an
  outcome agent code is expected to handle. It carries the reason, which the request's `refused`
  outcome records too: a rule, with its position and layer; the `default`; the approver; or no
  approver installed. The target is not invoked. The spelling is this task's.
- **An escalation handler on the embedding API**: an async trait installed through `0003-19`'s
  builder. Each request carries a host-generated id, the binding, the agent, the operation, member
  and host-side type, the bounded argument previews `0003-21`'s event records, the reason it
  escalated, and the policy version, with a cancellation signal that fires when the caller is
  interrupted or the session closes. The handler answers allow or deny for that one request, and a
  handler that fails or is dropped without answering denies. With no handler installed, `escalate`
  denies, the reason saying no approver is installed, and a session that starts with an
  `escalate` action reachable and no handler says so at start.
- **No expiry.** The wait ends with an answer, an interrupt or the close, and OutRig sets no
  deadline of its own. A handler that wants one answers deny when it passes, with a reason saying
  so, which leaves the deadline with the embedder, who also chooses the approvers.
- **Held requests in Rust's admission gate.** A request held for a decision -- a human's, or from
  `0003-23` an evaluator's -- has one state there: waiting, then allowed, denied, or cancelled.
  The first transition wins, and a later one is recorded and ignored. A handler's answer, an
  interrupt and a close all go through it, so a close and an allow cannot both win, and an allow
  that arrives after a cancel runs nothing. A request a rule allowed does not pass through the
  gate; after the close, its binding's closed flag refuses it (`0003-21`). The binding honors an
  allow the gate issued before the close, even if its flag is set by the time the allow arrives,
  since the gate has already ordered the two. A held request blocks nothing in its binding: the
  connection that carried it has no other call in flight, it holds no serialize lock, which is
  taken at dispatch (`0003-17`), and while it waits the binding serves every other request.
- **The policy version**: a digest of the rules in force -- each layer's rules, in order, and its
  `default` -- computed when the session starts and sent to each binding with its rule table.
  Every decision event and every escalation request carries it, so a record says which rules
  decided it, and `0003-23` puts it in the evaluator's input.
- **The CLI's handler.** `run-new` prints each pending request on stderr with its id, binding,
  member and operation. `crates/outrig-cli/src/cli/run_new/converse.rs` takes `/approve <id>` and
  `/deny <id>`, and `compose_help` in `crates/outrig-cli/src/repl.rs` gets their help lines. An id
  that is unknown or already settled gets a note, not an error. Both are commands, so neither
  reaches the user channel, mid-round included.
- **Policy events**, in the integration-audit category `0003-13` defined, sharing the request's id
  with `0003-21`'s receipt, dispatch and outcome. Each decision made about the request is a
  decision event carrying the policy version: the rule's action, with the rule's position and
  layer or the `default` -- a rule's `escalate` starting the wait -- and an approver's answer. An
  answer that arrives after the request settled is recorded as ignored. A request whose target is
  never invoked has no dispatch event; its outcome event, `refused` or `cancelled`, is the record
  that it was never invoked (`0003-21` fork 4) -- `refused` with its reason, or `cancelled` with
  `interrupted` or the close as the reason.
- **No approval reuse.** Every escalated request asks again, and nothing is remembered between
  requests. `plan/next/approval-reuse-with-explicit-scope.md` records what scoped reuse would need.
- **The waiting-for-an-approval row in `lifecycle.md`'s close table.** At close a pending request
  is cancelled through the handler's signal, its caller gets the closing error, a late answer is
  recorded and ignored, and the shutdown report lists the request `cancelled`, never invoked.

## Acceptance

- **A denied target is never invoked**, asserted with a counter on the host side of a test
  binding. A rule's deny, a `default` of `deny`, a human's `/deny`, and `escalate` with no handler
  each leave it at zero, raise the distinguishable exception in the caller, and end the request
  `refused` with that reason.
- `/approve <id>` runs the call exactly once, and `/deny <id>` raises in the caller.
- **A deny rule never reaches the handler**, asserted with a handler that counts its requests.
- Interrupting a pending request and then typing `/approve <id>` runs nothing: the counter stays at
  zero, the request ends `cancelled` with `interrupted` as the reason, and the note says the
  request is no longer pending.
- A close while a request is pending cancels it: the handler sees the cancellation signal, the
  caller gets the closing error, and the shutdown report lists the request `cancelled`, never
  invoked.
- An allow raced against a close, with each order forced in turn, records exactly one outcome
  each time, and the counter agrees with it.
- **Each request is admitted in one place.** A request made after `close_admission()` returns is
  refused in Rust -- `refused`, admission closed, the counter at zero -- and never reaches its
  binding's flag. A request a rule allows that the relay forwarded before the close and its
  binding read after the control line -- `0003-21`'s shape: the binding paused, requests
  forwarded, the close, the binding resumed -- is refused by the flag with the same outcome. An
  allow the gate issued before the close, delivered after the binding's flag is set, still runs
  the call once.
- **A held request blocks only its caller.** While one kernel's request waits for `/approve`, a
  request from another kernel to the same binding returns.
- **A repo cannot loosen the operator's policy.** A repo `allow`, as a rule or as `default`, is
  rejected at load with a message naming it. A request the operator's layer escalates and a repo
  rule evaluates is escalated.
- **A repo cannot redefine the operator's model names.** A repo config that replaces the global
  `[models.fast]` alias, and the `[providers.<name>]` it reaches, leaves the operator's layer
  resolving `fast` to the global rows and the global provider's endpoint, which is what `0003-23`'s
  evaluator reads; the merged config resolves it to the repo's, as today. A `Config` given to the
  builder with no layer kept is read as the operator's: its `[policy]` decides as a global one
  does.
- **A repository cannot switch the user's recording.** Global `record` with repo `off` records;
  global unset with repo `record` records; global `off` with repo `record` does not.
- **Decisions name the rules in force.** Every decision event and every escalation request carries
  the policy version. Two sessions with the same rules show the same version, and changing one
  rule's action, or the order of two different rules, changes it.
- `/approve` typed mid-round never reaches the user channel: the agent's
  `runtime.channels["user"].pending()` is unchanged.
- `evaluate` with no evaluator escalates when a handler is installed and denies without one.
- A handler that fails, and one dropped without answering, each leave the counter at zero.
- With fork 4's recommendation, a rule on a member whose read returns a callable governs the later
  call of that callable, and the escalation request shows the call's arguments.
- Each case above leaves its events in the stream, in order -- receipt, decisions, a dispatch where
  there was one, and the outcome -- all sharing the request's id.
- `crates/outrig/public-api.txt` regenerated: the handler trait, its request and answer types, the
  builder method, the `[policy]` config types, the way a loaded config keeps its operator's layer,
  and the policy events are its only additions.
- `cargo test --workspace`, `cargo clippy --all-targets`, `cargo fmt --check` pass.

## Design forks

1. **Where rules are evaluated -- Recommended: in each binding process, from a rule table pushed
   into it at start; escalations go to Rust.** The host-side `Connection` subclass from `0003-16`
   decodes each request, so the binding process already holds everything a rule matches on. With
   the table there, an allow costs nothing beyond `0003-21`'s relay -- one expression such as
   `repo.head.commit.tree` makes several requests, and none of them waits on an extra round trip.
   `escalate` and `evaluate` go to Rust, where the handler, the evaluator and the admission gate
   are, and a request a rule allowed is admitted by its binding's closed flag (`0003-21`). The
   alternative, asking Rust about every request, keeps all policy code in one process at the cost
   of a round trip per request.
2. **How a repo's `[policy]` combines with the operator's -- Recommended: each layer decides on its
   own, and the stricter action wins.** Each layer tries its rules, first match winning; the
   operator's layer falls back to its `default`, and a repo layer with no matching rule and no
   `default` of its own takes no part. Strictness runs `allow` < `evaluate` < `escalate` < `deny`:
   an evaluator may allow without a human, and a human may allow where a deny rule never does. A
   repo `allow` can then never take effect, which is why it is rejected at load rather than
   accepted and ignored. One list with the repo's rules ahead of the operator's was considered and
   rejected: a repo `evaluate` matched first would replace an operator's `deny`.
3. **How a rule matches a value -- Recommended: exact names, `*` as a wildcard within a field, and
   an omitted field matching anything.** The host-side type is matched by the qualified name
   `0003-21`'s event records, so an operator can copy it from the stream, and a subclass is not
   matched by its base's name. Matching by type hierarchy would need the binding process to read
   the class hierarchy of the hosted library's types for every request.
4. **How a call on a callable read from an attribute is described -- Recommended: in terms of the
   read that produced the callable.** `boundary-policy.md` leaves this to this task.
   `repo.git.status("--short")` is a read of `status` on `git.cmd.Git`, which returns a function,
   and then a call on that function, which names no member; a raw client can split a direct method
   call the same way. The binding process records, for each callable it returns from an attribute
   read, the type and member it was read from, and a call on it is matched, evented and shown to an
   approver as a call of that member on that type, with its arguments. A callable with no recorded
   origin -- one a call returned, say -- is matched by its own type. Matching only the read would
   leave a rule deciding before the arguments exist, and an approver approving a call they never
   saw.

## Dependencies

- **Hard: `0003-21`.** Policy decides on the requests that task decodes and records, and its
  closed flag is what refuses a rule-allowed request after the close. Through it, `0003-19`, whose
  builder takes the handler and whose admission gate this task adds held requests to.

## See also

- `plan/phase/0003-python/boundary-policy.md` -- rules and actions, the default of allow,
  published as events, escalation and `/approve`, and what policy cannot see.
- `plan/phase/0003-python/lifecycle.md` -- admission, and the close-state table this task adds a
  row to.
- `plan/phase/0003-python/embedding.md` -- the escalation handler among what the embedder
  supplies, and who may approve.
- `SECURITY.md` and `validate_as_repo` in `crates/outrig/src/config/validate.rs` -- the
  `[network]` rule the repo restriction mirrors.
- `crates/outrig/src/config/merge.rs` -- the merge whose result the operator's layer is kept
  beside.
- `plan/next/approval-reuse-with-explicit-scope.md` -- reuse, deferred.
