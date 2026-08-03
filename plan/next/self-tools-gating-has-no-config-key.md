# The `outrig__*` self-documentation tools have no config key

## Problem

The eight `outrig__*` docs and validation tools are offered when, and only when, a session fell
through to the built-in default image-config (`crates/outrig-cli/src/cli/run.rs`, gated on
`used_builtin_default`). There is no way to keep them and no way to decline them.

The trigger is arguably anti-correlated with need. The moment a user writes any config naming
an image, `outrig__validate_config`, `outrig__get_config_schema`, and
`outrig__validate_dockerfile` disappear -- which is precisely the session where they were about
to be useful, and precisely the workflow `outrig mcp self` exists to serve. Conversely someone
happy on the built-in cannot trim eight tool schemas out of their prompt budget.

The precedent sits a few lines above it in the same function: subagent tools gate on
`Agent::subagents_enabled`, a config key with a default.

## Why it shipped this way

Deliberate, not an oversight. The alternatives were weighed when the feature was designed and
this one was chosen on the grounds that it is the narrowest blast radius: a configured repo's
tool list is byte-for-byte what it was before, so nothing that worked can regress. A config key
was the runner-up and was declined as speculative -- no user had asked.

## Sketch

`self-tools = <bool>`, top-level or per-agent, **defaulting to `used_builtin_default`** rather
than to a constant. That keeps today's behavior as the default expression while making both
overrides expressible. Note the sequencing consequence: adding the key later means the current
bool becomes the default *expression* for it, so nothing about today's code is wasted -- which
is part of why deferring was reasonable.

Worth deciding at the same time whether the tools should be individually selectable (the docs
half is cheap and broadly useful; the five validators are authoring-specific), or whether the
whole set moves together.

## Acceptance

- `self-tools = true` in a repo with its own image-config gets the eight tools.
- `self-tools = false` on a built-in-default session gets none of them.
- Unset behaves exactly as today: on for a built-in-default session, off otherwise.
- `doc/reference/config.md` documents the key and its non-constant default.

## See also

- `crates/outrig-cli/src/self_tool.rs` -- the tool set and the `Kind` table.
- `crates/outrig-cli/src/cli/run.rs` -- the `subagents_enabled` gate directly above, the shape
  to match.
- `plan/next/subagent-model-allowlist.md` -- the other "an operator wants a say in what the
  agent can reach" entry; may want to land as one config surface.
