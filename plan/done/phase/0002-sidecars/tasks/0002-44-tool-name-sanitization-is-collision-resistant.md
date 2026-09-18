# 0002-44 -- Every lossy tool-name sanitization is collision-resistant

## Context

`crates/outrig/src/tool_name.rs` builds the LLM-facing `<server>__<tool>` name. Its module doc
claims a property the code does not have:

> Over-long names are truncated and tagged with a stable 6-hex blake3 suffix derived from the
> *pre-sanitization* `<server>__<tool>` **so two tools that would otherwise collide after
> character replacement get distinct suffixes.**

The suffix is only ever applied on the length path (`tool_name.rs:49-58`):

```rust
if sanitized.len() <= MAX_NAME_LEN {
    return sanitized;          // <-- no suffix, however lossy the replacement was
}
```

Character replacement is the lossy step, and it runs on every name. Two distinct valid short
names -- `read/file` and `read file` -- both sanitize to `read_file`. After server prefixing they
are the same advertised name, and proxy construction aborts on the duplicate. The existing
collision test confirms the whole proxy fails, which is the availability half: one server
exposing two such tools takes down the session's entire tool list, not just the pair.

There is a second, independent defect. The hash preimage is `format!("{server}__{tool}")`
(`tool_name.rs:37`), which does not encode where the server name ends. `("a", "_b")` and
`("a_", "b")` produce the identical preimage `a___b`, so even the suffix cannot distinguish them.
The separator is not self-delimiting when either side may contain underscores -- and both may.

This is wire behavior. Advertised names are what a model calls and what a client may persist
across sessions, so changing the mapping later invalidates anything that cached a name. Cheaper
to settle before final than after.

## What can and cannot be promised

**Collision *freedom* is impossible and must not be promised.** The input is an unbounded pair of
Unicode strings; the output is at most 64 characters over a 64-symbol alphabet. No injection
exists, and no suffix width changes that. An acceptance criterion of the form "distinct arbitrary
inputs produce distinct outputs" cannot be satisfied, and the earlier draft of this task asked
for exactly that.

What is achievable is **deterministic collision resistance**: distinct inputs collide only via a
hash collision, the mapping is stable across processes and runs, and a collision that does happen
is detected rather than silently merging two tools. The audit asked for a "collision-free" public
mapping; read that as this, because the literal version is not a thing that exists.

## Goal

Any name the sanitizer changes is disambiguated by a domain-separated hash of the input pair, and
a residual collision is caught rather than served.

## Deliverables

- **Suffix on change, not on truncation.** If the sanitized form differs from the input, append
  the stable `_<hex>` suffix -- the mechanism already exists at `tool_name.rs:53-57`, it is only
  gated wrongly. Names that survive sanitization untouched keep their exact spelling, which is
  the common case and the one worth not disturbing.
- **The hash preimage is domain-separated and length-delimited.** Hash the *pair*, encoding each
  side's length, rather than a concatenation whose separator can appear in either side. This is
  the `("a", "_b")` / `("a_", "b")` defect and it is invisible until someone hits it.
- **Length accounting composes.** Adding a suffix to a name near `MAX_NAME_LEN` (64) must not push
  it over; the truncation path and the change path have to compose, not race.
- **A stated collision policy.** 24 bits of suffix will collide eventually. Either widen or
  adapt the suffix on detected collision, or fail construction for the specific colliding pair
  with an error naming both tools. What is not acceptable is silently advertising one name for
  two tools.
- **The duplicate guard stays.** Its job changes from "catch our own lossiness" to "catch a
  genuine conflict" -- two upstream servers legitimately advertising the same name, or a residual
  hash collision -- and its error message should say so. Scoping the failure to the colliding
  pair rather than aborting the whole proxy is the availability fix; see fork 2.
- **`RESERVED_SERVER` interaction checked.** `outrig` is reserved so built-ins cannot collide
  (`tool_name.rs:18`); confirm the new suffixing cannot produce a name that shadows a built-in.
- **The module doc matches the code**, and states collision *resistance* rather than the property
  it currently claims.
- `doc/usage/mcp.md` wherever it explains the advertised-name shape.

## Acceptance

- `sanitize("s", "read/file") != sanitize("s", "read file")`, and both match
  `^[a-zA-Z0-9_-]{1,64}$`.
- `sanitize("a", "_b") != sanitize("a_", "b")` -- the domain-separation regression.
- **Golden vectors**, not just properties: a fixed table covering an unchanged name, a
  replacement-only name, a truncation-only name, a name needing both, a boundary-length name, and
  a non-ASCII name, each with its expected output pinned. These are what catch an accidental
  change to a mapping that clients may have persisted.
- A property test asserting the achievable claim: sanitized outputs always match the constraint,
  the function is deterministic, and equal inputs give equal outputs. **Not** injectivity.
- Stability across processes: the suffix is a hash of the input, so this should hold trivially --
  pin it anyway, because a session that reconnects must see the same names.
- **A residual digest collision is forced and its behavior asserted.** 24 bits will not collide
  by accident in a test suite, so make the digest width or the digest function injectable and
  drive it with a deliberately tiny one. Then assert whatever fork 2 chose actually happens, and
  that the diagnostic names *both* upstream identities -- a collision report that names one tool
  tells an operator nothing. Without this, the collision policy is untested code.
- The existing proxy collision test flips: two tools that previously aborted construction now
  both appear, under distinct names.
- A name that needs no sanitization is returned byte-identical.

## Design forks

1. **Whether truncation and replacement share one rule -- Resolved: one rule.** Truncation *is* a
   change; folding both into "suffix if the output differs from the input" is one branch instead
   of two and removes the class rather than the instance.

2. **What a residual collision does -- Resolved: rename (widen) the loser.** Aborting proxy
   construction cost the whole session's tool list for one bad pair, and dropping the loser makes
   the advertised set silently differ from the upstream set. Widening keeps both tools; see
   `## Decisions` for the cost it takes on.

## Dependencies

None hard. Land before 0002-48 regenerates the snapshot.

## See also

- `crates/outrig/src/tool_name.rs` -- `sanitize` (36), the preimage (37), the early return (49),
  the suffix mechanism (53-57), `MAX_NAME_LEN` (22), `RESERVED_SERVER` (18).
- `crates/outrig/src/mcp_proxy.rs` -- where duplicate advertised names abort construction.

## Decisions

1. **The one rule is faithfulness, not "the sanitized form differs".** Fork 1 asked for a single
   branch, and the literal version -- suffix when character replacement or truncation changed the
   composed string -- cannot satisfy this task's own acceptance list. `("a", "_b")` and
   `("a_", "b")` both compose to `a___b`, which nothing replaces and nothing truncates, so a rule
   phrased over those two steps returns the same name for both and contradicts the criterion two
   lines above it. The rule implemented instead is: return the composition byte-identical iff it
   *faithfully encodes the pair* -- no character replaced, it fits in 64, and its first `__` is
   the separator (`composed.find("__") == Some(server.len())`). `("a", "_b")` passes the third
   condition because `"a"` ends exactly where the `__` starts; `("a_", "b")` fails it and is
   suffixed. Both acceptance criteria then hold literally, and it is still one branch.

2. **The unsuffixed form is injective, but that does not make the guard unreachable.** Cutting a
   faithful name at its first `__` recovers `(server, tool)` exactly, so two distinct faithful
   names cannot collide -- including the case the old test used, two servers legitimately
   producing one name. Two *lossy* names collide only through a blake3 collision, which is why
   the suffix width had to become injectable: at 24 bits nothing in a test suite collides by
   accident, so the policy would otherwise be untested code. A lossy name landing on a faithful
   one needs no collision at all, though: a server exposing both `read/file` and
   `read_file_5d2270` reaches `fs__read_file_5d2270` twice. The first draft of this task missed
   that and widened through `sanitize`, which returns a faithful name unchanged at every width --
   so the ladder regenerated the same name three times and dropped the tool. `tool_name::suffixed`
   exists to force a suffix regardless, and `a_faithful_name_can_claim_a_suffixed_one_and_is_still_widened`
   pins it at the production width, with no injection needed to reach it.

3. **Fork 2: widen the loser's suffix.** `assign_public_names` scans widths upward from
   `tool_name::HASH_HEX_LEN` for the first name nobody holds, logging both `(server, tool)`
   identities at ERROR. The cost, taken knowingly: an advertised name is `sanitize(server, tool)`
   *unless that name was already claimed this session*, and only the proxy knows the widened
   form. `outrig run` (`rig_tool.rs:48`) and `builtin_tool::name_of` compute names with no view
   of the session's other tools, so in a collision the two paths would disagree -- a pair that has
   already collided at 24 bits, which needs roughly 4000 suffixed tools to reach a 50% birthday
   chance. Bought with it: nothing disappears from the tool list, which was the objection to
   dropping. The module doc and `doc/concepts/mcp-servers.md` state the qualifier rather than
   leaving the unconditional claim standing.

4. **The ladder is the whole range `HASH_HEX_LEN..=MAX_HASH_HEX_LEN`, scanned for the first free
   name.** A fixed list of a few widths was the first shape and bought nothing: a contiguous
   range is one expression, needs no "what if the list is empty" guard, and cannot run out except
   for an input no width separates. `MAX_HASH_HEX_LEN` is 63, where `_` plus the hex is exactly
   64 and the body is squeezed out entirely. Exhausting the range leaves the losing tool
   unadvertised; that arm is reached only by an upstream `tools/list` naming one lossy tool twice,
   because a pure function of the pair cannot tell a pair from itself. The same listing with a
   *faithful* name is advertised twice instead, under one plain name and one suffixed one -- the
   ERROR line names the same pair on both sides, which reads unmistakably as the malformed
   listing it is.

5. **A contested name is awarded by identity, not by arrival.** The first version of the
   widening handed the contested name to whichever tool `tools/list` returned first and moved
   the other. `tools/list` promises no order, so a backing server that restarted and relisted
   differently would bind the contested name to the *other* tool -- and a client replaying a
   name it had cached would reach a different backend and get a plausible answer rather than an
   error, which is the worst available outcome for this whole design and one the pre-task code
   could not reach, because any clash was fatal. `assign_public_names` therefore hands names out
   in `(server, tool)` order, so the map is a function of the *set* of tools; listing order is
   separate and stays the caller's, which is what `iter_public_names` and `per_server_counts`
   promise. `a_contested_name_is_awarded_by_identity_not_by_arrival` builds the same two tools
   in both orders and compares the name-to-backend mapping; with the sort removed it fails,
   showing `fs__read_file_5d2270` answering as a different tool in each direction.

   The residue is that a *widened* name still depends on the rest of the session's tool set, so
   it is not a function of the pair alone. That is unavoidable -- the loser's width is whichever
   one is free -- and it is now what `doc/` says, rather than the unconditional "same name every
   reconnect" the first draft promised.

6. **The widths are a wire contract but not a documented one.** No page in `doc/` names blake3 or
   the 6-hex width -- they say "a stable hash suffix" -- so the ladder can move later without
   contradicting anything published. The golden-vector table in `tool_name_tests.rs` is what makes
   a move deliberate.

7. **`RESERVED_SERVER` needed no code change.** Every built-in name is faithful, so it is returned
   byte-identical and no model-visible name grew a suffix. Nothing else can produce one: a
   faithful name decodes uniquely to its pair, so reaching `outrig__subagent` requires the server
   to be `outrig`, which validation rejects in four places; and a lossy name ends in `_<hex>`,
   which no built-in name does. Asserted rather than argued, in
   `reserved_prefix_is_not_reachable_from_another_server`.

8. **The `outrig run` path is left unguarded, deliberately.** `McpToolAdapter::from_client_tools`
   and `cli/run.rs`'s assembly have no duplicate check at all, so two colliding names are handed
   to rig and one becomes unreachable. It is the same defect class on the other half of the
   product, but outside this task's deliverables and reaching into three call sites in
   `outrig-cli`. Filed as `plan/next/run-path-has-no-tool-name-guard.md`.

9. **The shipped shape is `/simplify`'s alternative, not the first draft.** The independent
   implementation was smaller in the two places that mattered: a scalar starting width plus a
   contiguous range, instead of a slice constant threaded through both signatures with an
   `expect` on its first element; and a flat `if let` plus `map(..).find(..)` instead of a `loop`
   holding an iterator and breaking with a value. It also had the defect in decision 2 already
   fixed, by widening through an unconditional `suffixed` rather than through `sanitize`. The
   first draft's tests were kept: they derive the expected digest from the documented preimage
   rather than from the implementation's hasher, they *search* for a colliding pair at the
   injected width rather than pinning digest bytes a future change would break, and they share
   `process_tests::CaptureWriter` through a two-line visibility widening instead of copying it.
