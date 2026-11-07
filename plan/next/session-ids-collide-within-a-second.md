# Two session ids minted in one second collide one time in 65,536

Found while landing `0003-40`, which put a facade-launched container and a `harness::Session` in
one process.

## What happens

`container::mint_session_id` (`crates/outrig/src/container/mod.rs`) makes `yyyymmddTHHMMSS-xxxx`:
a UTC timestamp to the second and two random bytes. `Outrig::launch` and `SessionBuilder::start`
both call it for a spec that names no id, and the primary is created as `outrig-<sid>`. Two ids
minted in the same second share their suffix one time in 65,536. When they do, the second
`podman create` finds the name taken and that launch fails. Nothing is removed or shared: the
attempt label keeps every removal to its own container. But a start fails for no reason the
caller can act on, and the chance grows with the number of starts in one second: by the
birthday bound, about 1 in 1,500 for ten.

It matters more on the 0.3 line, where one process mints several ids: an application running an
engine over the facade beside one over `harness` (A-08), or a host program opening several
`outrig.Session`s.

## Options

- More randomness in the suffix -- four bytes, say. The id's shape is visible in session
  directories, `outrig clean`'s listing and every event's `source`, so check what parses or sorts
  it (`outrig-cli`'s session listing) before widening it.
- Retry the create once with a fresh id on a name conflict, for a minted id only. A caller who
  named the id keeps today's failure.

## Evidence that would decide it

Whether anything in `outrig-cli` or `scripts/render-session.py` reads the suffix's width. If
nothing does, widening it is the smaller change.
