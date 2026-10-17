# 0003-24 -- Policy decides what crosses, and the user can be asked

## Context

`0003-22` sends every hosted request through Rust and records it, and decides nothing: with no
policy configured, every request runs and is evented. That is the default `boundary-policy.md`
settles on -- allow, published as events -- so an operator can see what crosses before choosing
what should not. This task adds the choosing.

What a rule can match on is what `0003-22` already decodes for its event: the binding, the
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
or a `[models.<name>]` row could be the repository's. `0003-25` needs the same layer, since the
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
decides executions (`0003-19`), child launches (`0003-30`), and, from this task, every request
held for a decision. Each binding process's closed flag (`0003-22`) decides the requests a rule
allowed: rules run in the binding (fork 1), so such a request never waits on Rust.

The rule table a binding process receives at start also reaches a live object's serving module
when a host program injects one (`0003-36`): the same rules, evaluated in the program's process,
with the same escalations going to Rust. Nothing here assumes one serving process.

## Goal

Rules an operator writes, and a human when a rule asks for one, decide whether each hosted request
runs, and a request that is not allowed never reaches its target.

## Deliverables

- **`[policy]` and `[[policy.rules]]`.** `default` takes any of the four actions -- `allow`,
  `deny`, `escalate`, `evaluate` -- and is `allow` when absent, which keeps `0003-22`'s behavior
  for a session that configures nothing. A rule matches on binding, host-side object type, member
  and operation, any of which it may omit, and names an action. Rules are tried in order and the
  first match wins; a request no rule matches takes `default`. Validation rejects an unknown
  action and an operation name outside `0003-22`'s vocabulary. Matching per fork 3, and a call on
  a callable obtained by an attribute read per fork 4.
- **The operator's layer, kept beside the merged config.** Loading keeps the global config as it
  was read -- its `[policy]`, `[models]` and `[providers]` included -- next to the merged `Config`
  the rest of OutRig reads. The operator's `[policy]` is read from that layer, and `0003-25` reads
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
- **`evaluate` is legal before there is an evaluator.** `0003-25` adds the model that judges it.
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
  and host-side type, the bounded argument previews `0003-22`'s event records, the reason it
  escalated, and the policy version, with a cancellation signal that fires when the caller is
  interrupted or the session closes. The handler answers allow or deny for that one request, and a
  handler that fails or is dropped without answering denies. With no handler installed, `escalate`
  denies, the reason saying no approver is installed, and a session that starts with an
  `escalate` action reachable and no handler says so at start.
- **No expiry.** The wait ends with an answer, an interrupt or the close, and OutRig sets no
  deadline of its own. A handler that wants one answers deny when it passes, with a reason saying
  so, which leaves the deadline with the embedder, who also chooses the approvers.
- **Held requests in Rust's admission gate.** A request held for a decision -- a human's, or from
  `0003-25` an evaluator's -- has one state there: waiting, then allowed, denied, or cancelled.
  The first transition wins, and a later one is recorded and ignored. A handler's answer, an
  interrupt and a close all go through it, so a close and an allow cannot both win, and an allow
  that arrives after a cancel runs nothing. A request a rule allowed does not pass through the
  gate; after the close, its binding's closed flag refuses it (`0003-22`). The binding honors an
  allow the gate issued before the close, even if its flag is set by the time the allow arrives,
  since the gate has already ordered the two. A held request blocks nothing in its binding: the
  connection that carried it has no other call in flight, it holds no serialize lock, which is
  taken at dispatch (`0003-17`), and while it waits the binding serves every other request.
- **The policy version**: a digest of the rules in force -- each layer's rules, in order, and its
  `default` -- computed when the session starts and sent to each binding with its rule table.
  Every decision event and every escalation request carries it, so a record says which rules
  decided it, and `0003-25` puts it in the evaluator's input.
- **The CLI's handler.** `run-new` prints each pending request on stderr with its id, binding,
  member and operation. `crates/outrig-cli/src/cli/run_new/converse.rs` takes `/approve <id>` and
  `/deny <id>`, and `compose_help` in `crates/outrig-cli/src/repl.rs` gets their help lines. An id
  that is unknown or already settled gets a note, not an error. Both are commands, so neither
  reaches the user channel, mid-round included.
- **Policy events**, in the integration-audit category `0003-13` defined, sharing the request's id
  with `0003-22`'s receipt, dispatch and outcome. Each decision made about the request is a
  decision event carrying the policy version: the rule's action, with the rule's position and
  layer or the `default` -- a rule's `escalate` starting the wait -- and an approver's answer. An
  answer that arrives after the request settled is recorded as ignored. A request whose target is
  never invoked has no dispatch event; its outcome event, `refused` or `cancelled`, is the record
  that it was never invoked (`0003-22` fork 4) -- `refused` with its reason, or `cancelled` with
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
  binding read after the control line -- `0003-22`'s shape: the binding paused, requests
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
  resolving `fast` to the global rows and the global provider's endpoint, which is what `0003-25`'s
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
   the table there, an allow costs nothing beyond `0003-22`'s relay -- one expression such as
   `repo.head.commit.tree` makes several requests, and none of them waits on an extra round trip.
   `escalate` and `evaluate` go to Rust, where the handler, the evaluator and the admission gate
   are, and a request a rule allowed is admitted by its binding's closed flag (`0003-22`). The
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
   `0003-22`'s event records, so an operator can copy it from the stream, and a subclass is not
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

- **Hard: `0003-22`.** Policy decides on the requests that task decodes and records, and its
  closed flag is what refuses a rule-allowed request after the close. Through it, `0003-19`, whose
  builder takes the handler and whose admission gate this task adds held requests to.
- **Hard: `0003-23`.** The acceptance's agent code, and the orientation this task's deny
  exception joins, use the synchronous spelling; a held call blocks its caller's thread.

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

## Decisions

Forks 1 to 4 were taken as recommended, with the hardening below. The maintainer decided two
questions when asked.

- **No decision events without a `[policy]` (the maintainer's call).** With nothing configured,
  nothing is decided: every request runs under the unset default, so a per-request event saying
  so would repeat itself. A session publishes one `session.policy` at start -- its version,
  whether anything is configured, whether a request can be held, and whether a handler is
  installed -- and a session with no rule and no `default` keeps `0003-22`'s three events per
  request. Once anything is configured, every request decided by a rule carries a
  `hosted.decided`, a default's allow included. `session.policy` is the "says so at start" for an
  `escalate` with no handler too, with a `tracing` warning beside it.
- **`plan/next/an-interrupted-nested-hosted-call-sends-no-cancel.md` folded in (the maintainer's
  call).** Each call notes on its `_Wait` the sequence numbers of the requests it sent, and its
  cancel line names those alone, so the `conn.depth == 1` rule is gone and an interrupted call
  made inside a callback, held for an answer, ends `cancelled`. The entry is deleted.
- **The operator's layer.** The merged `Config.policy` is the operator's `[policy]`, as
  `[network]`'s policy keys already are, and each repository table is kept in a private
  `#[serde(skip)]` list of restrictions. A first plan made the merged `policy` the repository's,
  with the operator's read only from `Config::operator()`; a design review found that a TOML round
  trip of a merged config, which `config_merge.rs` exercises, would then lose the operator's rules
  and promote the repository's to the operator's. As built, a round trip loses only the
  restrictions. `Config::operator()` is the global config as read, kept in an `Arc` by `merge` --
  even when the global file is missing, so a repository's rules are never the operator's -- or the
  config itself when none was kept. It takes part in `PartialEq`; no test compares whole merged
  configs. This task reads `[models]` and `[providers]` from it only in tests; `0003-25` reads
  them.
- **`[events] mode`** is settled inside `merge`, which sees both files: the global value stands,
  and a repository's fills in only when the global config is silent.
- **The policy version** is the first 16 hex characters of a blake3 digest -- already a runtime
  dependency -- of the layers as the bindings process reads them: the operator's default resolved
  to the `allow` an unset one means, and each non-empty repository table. A `PolicyVersion` with a
  private field, like the id types.
- **Rule matching.** `operation` is an exact name from the boundary vocabulary less `buffiter` and
  `oldslicing`, which are refused before anything is decided; `*` is a wildcard in `binding`,
  `type` and `member` only, matched without backtracking -- the text before the first `*` and
  after the last fixed, each piece between found leftmost in turn -- since a name is the
  container's to make long. A rule that names no operation matches a member's read as
  well as its call, so "a person approves every push" is written with `operation = "call"`, and
  the documentation and `boundary-policy.md`'s example say so; otherwise the read asks too, with
  no arguments.
- **What a request is matched as**, the strictest of its descriptions deciding -- fork 4 and the
  bypasses a design review of the plan found:
  - A call of a reference a `getattr` returned is described as a call of that member on that
    type, and its receipt and its escalation show it so. The origins are kept in the agent's
    `Objects` slot as a list that only grows, so one function reached under two members is
    matched as both.
  - A bound method -- or a builtin one bound to an object -- is also matched as its own
    function's name on its `__self__`'s type, read from slots, so `also_noted = noted` and a
    method reached by iteration or returned by a call are still `noted`. Other callables a call
    returned are matched by their own type, as the task says.
  - `callattr` is a read and a call of its member too, and a call of the method the read would
    give, by that method's own name, found with `inspect.getattr_static`, which runs no
    descriptor; so a `callattr` of `also_noted` is `noted` before anything runs. Under a
    configured policy, a `callattr` whose method cannot be found that way -- a property's, or
    one `__getattr__` makes -- is refused by the interception.
  - A call described by a dunder member -- by its route or its own name -- that has an operation
    of its own is that operation too, so `__repr__` read and then called is a `repr`; and so is
    a `callattr` of one.
  - A request whose arguments -- nested in containers, or a `setattr` value -- hold a host callable
    whose own call the rules would not allow is refused with `Denied`, recorded as the decision of
    the argument's rule: the library could call it later with no request, so it cannot be asked
    about.
  - `buffiter` is refused by the interception: the interpreter's proxies iterate with
    `__iter__` and `__next__`, and a batch would run each step of the iteration with no rule
    asked about it.
  - `oldslicing` is refused by the interception: no Python 3 proxy sends it, and its fallback
    calls a second member.
  - `__call__`, `__get__`, `__func__` and `__self__` are not on RPyC's safe list, so the attribute
    check already refuses those aliases.
- **The type is the host's.** RPyC's `get_id_pack` asks the object for `____id_pack__` and
  `__class__`, which an attribute-dict object lets the container set, so the binding builds every
  id_pack itself (`id_pack_of`), laid out as RPyC lays one out and named by `type_name`, read from
  `type.__dict__`'s descriptors: a class's own qualified name, else the object's type's. Receipts,
  previews, rule matching and the object table's keys then all carry the host's name. That also
  makes `target` the qualified name `events.md` promised, where a nested class differs from
  RPyC's `__name__`; a module is `builtins.module`.
- **Deny** is `runtime.Denied`, an `Exception` with `reason`, beside `runtime.MessageAvailable`.
  The binding raises its own `Denied` (`__module__ = "outrig"`), and the interpreter's
  `_unbox_exc`, where every host exception is rebuilt, knows it by the name on the wire before
  RPyC rebuilds anything, so a local module named `outrig` makes no class of it. The refusal
  reasons are `rule`, `default`, `approver` and `no_approver`.
- **The handler** is `EscalationHandler`, whose method returns `impl Future + Send`, so an
  implementor writes `async fn`; the session holds it erased behind an `Arc`, and a blanket impl
  takes a closure returning a future. The future gives an `EscalationResult`; an `Err`, a panic,
  or the task ending unanswered denies. `Escalation::withdrawn()` is the cancellation signal, over
  a private `tokio_util` token, since `public_api_boundary.rs` admits no `tokio_util` type.
  `Escalation` has no public constructor: its id types have none either. Its `execid` is the
  interpreter's claim, and its other fields come from the binding's decision line.
- **The gate is the relay.** A held request is kept on its call in the relay's `State`, which
  `Table::close` closes in the same step as admission, so one lock orders an answer against the
  close. Decision
  lines now ride the events channel: production never read the 64-slot decisions channel, which
  would have stalled the stdout router once full, and `Process::answer` wrote around the relay's
  queue. `Decision`, `take_decisions`, `Process::send` and `answer` are gone, and the two
  supervisor tests that drove raw decision lines were moved: the interleaving one became
  `a_held_request_blocks_only_its_caller`, and the payload one now sees the relay's answer to a
  decision it holds no request for.
- **What ends a held request.** The handler's answer; the interpreter's cancel line, which also
  marks a call not yet held, since a cancel can reach the relay before the decision line, and
  whatever the queue's room; a connection's close, from either side; the process's end, as
  `refused`, `exited`; the close; and the interpreter's exit, which closes the relay at once --
  `Table::close` does nothing while the exit's cause is awaited, and a decision line arriving then
  must be refused too. For each but an allow, the relay records the outcome itself, and ignores
  the `ended` the binding writes after, as it ignores one for any request it no longer holds: a
  SIGKILL after the close cannot turn a cancelled request `unknown`. Each handler is one spawned
  task -- spawned under the state's lock, which runs none of its code -- holding the relay weakly;
  finished ones are pruned as others start, and those still running are aborted when the relay
  reports. A handler's failure after the request stopped waiting is no answer and is not
  recorded; an `Ok` answer then is recorded as ignored.
- **An allow is honored after the flag.** The binding's reading thread marks a request `released`
  as it hands over an allow, in pipe order against the control line, and `_control` leaves a
  released request in its turn's line. An interrupt after the allow can still drop it before its
  turn: decided `allow`, then `cancelled`, `interrupted`, as any forwarded request waiting for its
  turn. A request whose caller gave up before it would be held is cancelled there, asking nobody.
- **The CLI.** `run-new` prints `[outrig] binding call 7 waits for your answer: repo: call push
  on git.remote.Remote ('main')` and the line saying how to answer, its fields' control and
  direction characters escaped. The handler hands each call to the conversation's loop over a
  channel, since stdin and the session belong to the loop; a call withdrawn first is noted and
  forgotten, an id nothing waits under gets a note, and an ignored answer -- one that raced the
  withdrawal, which the handler prefers to the withdrawal when both are ready -- is noted from the
  event stream. Once input ends nobody is left to answer, so a
  waiting call, and each that arrives after, is denied; otherwise piped input would leave the
  round waiting for ever.
- **Known limits**, documented: a rule keyed on `binding` covers that name only; what an allowed
  call does on the host, a library calling a host object's methods by itself included, is not a
  request; and a callback's request held for an answer holds a serialized binding's turn, filed
  as `plan/next/a-held-callback-call-holds-its-bindings-turn.md`.
- **`/simplify`** ran four reviews -- reuse, simplification, efficiency, altitude -- over the
  change. Taken: the binding's own id_packs, above, in place of a second naming path for receipts
  and previews; no `recorded` answer field or `Request.settled`, since the relay already ignores
  an `ended` it does not hold; a held request kept on its call rather than in a second map, one
  `cancel_where` for the close and a connection's close, and `Settle::Cancel` carrying the
  `Withdrawn` alone; one task per escalation; `Escalation` built by the gate, with no `new` and no
  `Described`; actions and layers read through `serde`; the version computed once, and not sent
  to the process, which never read it; one decisions table in the binding; no policy work at all
  in a session with nothing configured; `policy.loaded` renamed `session.policy`, so readers that
  take `session.` events as the session's own need no exception; the interpreter's exit closing
  the relay, and a handler's late failure not recorded, both behavior gaps the reviews found; and
  the tests' shared helpers. Not taken: deriving `OPERATIONS` from `Operation`, which needs a
  public `Operation::as_str`; trimming what `merge` keeps of the global config, once per load;
  and memoizing a call's descriptions between its receipt and its decision.
- **A review of the commit found five defects**, fixed in it. A `callattr` of a public alias was
  matched only by the name it asked for, so a rule on the method it named passed it: it is now
  also a call of the method `inspect.getattr_static` finds. A dunder read and then called was
  matched only as a `call`, past a `repr` rule: each call description by a dunder now carries its
  operation too, in the argument check as well. `buffiter` ran `__next__` with no decision about
  it: it is refused. A connection that closed after its request was received and before the
  binding asked for a decision left the request to be held and approved after its caller had
  its error: a call now keeps why its caller stopped waiting -- an interrupt or its connection's
  close -- and a decision line for it is cancelled without asking anyone. And a rule of several
  `*` compiled to a backtracking regex, which took seconds over a long public name, holding the
  binding's other requests: the matcher above replaces it.
- **A second review found two more**, fixed in the commit. A `callattr` of a property, or of a
  name `__getattr__` makes, that hands out a governed method was decided only by the name it
  asked for, while a read and then a call of what it gave was refused. The review proposed
  resolving the read and deciding the call before invoking it; that runs library code after the
  dispatch event, and an escalation would then wait part-way through the request, inside a
  serialized binding's turn. Since the interpreter's proxies send `callattr` only for a method
  the type defines -- RPyC builds a proxy's class from the type's callable members -- such a
  `callattr` is refused instead, under a configured policy, saying to read and then call; a
  session with no policy, which decides nothing, answers it as before. A type whose own
  `__getattribute__` answers a defined method's name with another callable is outside what the
  static lookup sees. And a cancel line was matched against each request the relay held by
  searching its whole list, under the state's lock, before it was cut to 16 for the hint: a
  16 MiB cancel held the relay for seconds. The numbers are now a set, built before the lock,
  every one still cancelled, and the hint keeps the first 16 in order.
