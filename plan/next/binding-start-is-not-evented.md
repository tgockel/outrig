# A binding's start is in no event

## Context

`0003-20` starts each binding before the interpreter and stops it at shutdown. The stream hears of
a binding twice: `binding.refused` when a repository's binding was not approved, and the
`bindings` list of `session.report` at the end. Nothing records that a binding started, with
which packages, what its factory returned, or how long the install and the factory took, so a
recording cannot say which bindings the session ran with until it is over, and a session that
dies before its report says nothing of them at all.

## Shape

- `binding.started`, once per binding as its factory returns: the name, where it was declared
  (global config, repository, or `bind`), the requirement set's directory, the qualified type the
  factory returned, `serialize`, and the time the install and the factory took.
- In the execution-diagnostics category beside `session.state`, with no subject.
- `scripts/render-session.py` lists the session's bindings from these events.

## Acceptance

- A session with two bindings publishes two `binding.started` events, in the session's order,
  before `session.state` reaches `idle`, each naming its type.
- A recording cut off before its report still names the session's bindings.
