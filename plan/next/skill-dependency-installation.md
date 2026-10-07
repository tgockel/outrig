# A skill's declared dependencies are checked, never installed

## Context

A skill's `skill.py` may declare `dependencies` in its PEP 723 `# /// script` block. Phase 0003
checks them against what the interpreter can import and fails the invocation, naming each unmet
requirement; it never installs (`0003-27`, `plan/phase/0003-python/skills.md`). The agent can
then run `pip install` itself, which installs pure-Python packages into its user site under the
container's network policy.

The phase 0003 design brief (now in `plan/phase/0003-python/skills.md`) left installation open:
never, operator-only, or per-skill approval. PEP 723 requires a script runner to fail when it
cannot provide the dependencies, and installing them is a separate grant, which skill metadata
cannot make by itself.

## Shape

- **Who allows it.** Off by default. Either an operator setting, or approval per skill the way
  `0003-20` approves a repo-declared binding: asked once, remembered by the skill's digest, asked
  again when the skill changes.
- **Where it installs.** In the container with the payload's pip, which keeps `[network]` in
  force and shares the user site (#466); or on
  the host into the package cache `0003-18` builds for bindings, keyed by requirement set and
  mounted read-only, where the container's network policy does not apply.
- **What it installs.** Pure-Python wheels only, through the same check `0003-18` applies to
  binding installs and #465 wants for the agent's pip.
- **When.** At invocation, never at discovery. A failed install fails the invocation with the
  reason.

## Acceptance

- With installation allowed, invoking a skill that declares a missing pure-Python package
  installs it once and the skill imports it; a second invocation installs nothing.
- A dependency with no pure-Python wheel fails the invocation with the reason and installs
  nothing.
- With installation off, the default, behavior is phase 0003's.
