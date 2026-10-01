# An approval covers one call, so a loop of escalated calls prompts once per call

## Context

`0003-22` holds a call that an `escalate` rule matches until the escalation handler answers. The
CLI prints the request with an id and takes `/approve <id>` or `/deny <id>`, and each answer
settles one pending call. Phase 0003 reuses no approval
(`plan/phase/0003-python/boundary-policy.md`), so forty escalated calls in a loop mean forty
prompts.

The obvious fix, remembering an approval by member name, is the one the phase 0003 design brief (now
in `plan/phase/0003-python/boundary-policy.md`) ruled out: an approval keyed by a method name or by
a rendered argument admits calls the approver never saw. Its condition for any reuse is an explicit
scope -- resource, arguments, binding, policy version and lifetime -- and a way to revoke it.

## Shape

- An approver may attach a scope when approving: binding, host type, member and operation;
  argument values, matched exactly against the by-value snapshot; the policy version, so any edit
  to `[policy]` ends it; and a lifetime -- a count, a duration, or the session.
- A later call reuses the approval only if every field matches, and its event names the approval
  it reused.
- Revocation is a REPL command and an embedding-API call, effective at once. A call already
  admitted under the approval is not recalled.
- Never reused for a call with a callable among its arguments, since what the callable does can
  change after approval. A rule's deny stays final, as in phase 0003; reuse only replaces asking.
- A scope covers argument values, not the state of what they name. The file at an approved path
  can change between calls.

## Open questions

- Whether every escalation may be approved with a scope, or only those a rule marks reusable.
- The REPL spelling for a scope, and for listing and revoking the approvals still in effect.
- Whether an escalation the evaluator raised (`0003-23`) may be approved with a scope.

## Acceptance

- An approval with a count of three admits three matching calls; the fourth escalates.
- A call that differs from the approved one in a single argument value escalates.
- Editing `[policy]` ends every scoped approval, and revoking one ends it before its next match.
