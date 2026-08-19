# 0121 -- Every lossy tool-name sanitization is collision-resistant

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

2. **What a residual collision does -- Open.** Aborting proxy construction is today's behavior and
   costs the whole session's tool list for one bad pair. Dropping or renaming just the colliding
   tool keeps the rest available and means the advertised set silently differs from the upstream
   set. Whichever is chosen, it should be logged loudly; the current failure is at least visible.

## Dependencies

None hard. Land before 0125 regenerates the snapshot.

## See also

- `crates/outrig/src/tool_name.rs` -- `sanitize` (36), the preimage (37), the early return (49),
  the suffix mechanism (53-57), `MAX_NAME_LEN` (22), `RESERVED_SERVER` (18).
- `crates/outrig/src/mcp_proxy.rs` -- where duplicate advertised names abort construction.
