# 0017 -- In-process LLM documentation: drop `TODO: Incomplete` markers

## Goal

The user-facing pages for the in-process LLM landed ahead of implementation as
design-first docs and carry `> **TODO: Incomplete**` markers throughout. With 0013-0016
done, the surface those pages describe is real. Drop the markers; tighten any wording
that still hedges with "intended" or "planned"; verify the audit + mdbook build are
clean.

## Deliverables

- `doc/concepts/in-process-llm.md`:
  - Drop the top-of-page `> **TODO: Incomplete**` quote block.
  - Drop the in-page `> **TODO: Incomplete**` block under "Why you might want this"
    (the one about downstream features) **only** if those downstream features have
    landed -- they haven't in this batch, so this marker stays.
  - Drop the `> **TODO: Incomplete**` block under "What this enables (sketch)" -- the
    section title and prose already telegraph that the `PolicyEngine` API is
    forward-looking, so the marker is redundant. (Decision recorded mid-task; see
    `## Decisions` below.)
- `doc/concepts/llm-providers.md`:
  - Drop the `> **TODO: Incomplete**` block in the `## In-process providers (mistralrs)`
    section.
  - The page's top-of-page TODO marker (about other Rig provider styles) is unrelated
    and stays.
- `doc/reference/config.md`:
  - Drop the `> **TODO: Incomplete**` block under `### style = "mistralrs"`.
- `doc/concepts/workspace.md`:
  - The egress-section TODO marker stays (egress filter hasn't landed).
- Verify cross-links still resolve after marker drops; no other text changes unless the
  audit flags something.
- Run `python3 scripts/audit-doc-style.py doc` -- it should not regress vs. the pre-task
  baseline.
- Run `mdbook build` -- it should produce no new warnings vs. the pre-task baseline.

## Acceptance

- `python3 scripts/audit-doc-style.py doc` exits with no new violations introduced by
  this task.
- `mdbook build` produces no new warnings.
- `grep -n "TODO: Incomplete" doc/concepts/in-process-llm.md` matches at most one
  remaining block (the downstream-features one), or zero if downstream features have
  also landed.

## Dependencies

- 0016-llm-registry

## Notes

- Don't rewrite the page contents -- the docs were written to be accurate when the
  surface lands. If you find yourself wanting to change *how* a feature is described,
  that's a sign the implementation diverged from the docs and the right fix is in
  whichever earlier task introduced the divergence, not here.
- The audit script currently has pre-existing width violations on trunk (tables in
  `doc/reference/config.md` and `doc/usage/run.md` etc.). "No new violations" is the
  contract, not "passes cleanly."
- This is the right moment to also re-read the four touched pages end-to-end as a
  user might. If a paragraph has aged badly between the design pass and the
  implementation, fix the wording in this task rather than punt it to a follow-up.

## Decisions

- **Three TODO markers in `doc/concepts/in-process-llm.md`, not two.** The original
  Deliverables list addressed the top-of-page block and the "downstream features"
  block under "Why you might want this", but a third block lived under "What this
  enables (sketch)" marking the `PolicyEngine` API preview. Resolved at task start
  by dropping the line-185 block and keeping the line-34 one. The acceptance
  criterion "at most one remaining block (the downstream-features one)" is now
  satisfied exactly: line 34 is the sole remaining block. The `(sketch)` section's
  own title and prose ("will share", "before any of it ships") already signal it's
  forward-looking, so dropping the marker doesn't mislead readers.
- **`doc/concepts/llm-providers.md` no-op.** The marker the deliverables call for
  was already dropped during 0014 (commit `44d1c6d`); no change needed in this
  task.
