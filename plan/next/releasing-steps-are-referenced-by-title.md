# `RELEASING.md`'s step cross-references are unverified prose

## Problem

0002-48 inserted a step into `RELEASING.md`'s checklist and had to hand-renumber the six below
it -- plus re-indent step 10's fenced block from three spaces to four, because `10.` is a wider
marker than `9.`. To stop the next insertion doing the same to the *references*, it converted
every by-ordinal mention outside the list into the step's bold title:

```
- The combined dry-run (**Dry-run both crates in one invocation**) does exercise that pin
- The **Refresh the version-bearing docs** step is skipped; see the note there.
```

That is strictly better than an ordinal, which rots silently and *guaranteed* on any insertion.
But it is still unverified prose: eight sites across `RELEASING.md`, `plan/todo/0002-52` and
`plan/todo/0002-54` now name a title that nothing checks. Rename a step and they rot silently --
the same class of failure as a version recorded as prose, which is what 0002-48 existed to end.

The renumbering cost is also only moved, not removed: the list still uses literal `1.` .. `10.`,
and `plan/todo/0002-52-cut-0.2.0-rc.3.md` plans two more insertions before the publish, so the
same hand-renumber lands twice more.

## Sketch

Two cheap halves, either useful alone:

- **Make the references checkable.** Promote each checklist item to a `###` heading (or give it
  an explicit anchor) and make the cross-references real links. `.github/workflows/ci.yml`
  already runs lychee over `RELEASING.md` with `--include-fragments`, so every intra-file
  reference becomes machine-checked at no extra cost. Note this trades the checklist's shape
  for sections; decide whether that reads worse before doing it.
- **Stop hand-maintaining ordinals.** Write every item as `1.` and let the renderer number
  them, so an insertion touches one line. Costs the ability to say "step 4" while reading the
  raw file, which is how the document is often read -- weigh that.

The five references in `plan/**` stay unchecked either way: lychee's glob set covers `doc/**`,
`README.md`, `CONTRIBUTING.md`, `SECURITY.md`, `CODE_OF_CONDUCT.md` and `RELEASING.md`, not
`plan/`. Widening it is separate, and `plan/next/plan-tree-link-rot.md` is the adjacent entry.

## See also

- `plan/done/phase/0002-sidecars/tasks/0002-48-enforce-the-public-api-snapshot.md` -- Decision
  12 made the ordinal-to-title move and recorded this as the half it did not do.
- `plan/todo/0002-52-cut-0.2.0-rc.3.md` -- inserts two more steps; doing this first makes that
  insertion cost one line.
