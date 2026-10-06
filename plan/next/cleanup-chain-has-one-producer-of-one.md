# `detach_cleanup_chain` is handed at most one command

## Context

`supervise::detach_cleanup_chain` exists so that cleanups which must run *in order* arrive at the
reaper as one obligation. Its sequencing -- `Wait::then`, `hand_back`'s wait-the-head-out path,
`spawn_orphaned_chain`, and the `ordered_shell` fallback that runs a whole chain in one `sh` --
was built for one producer: `Rollback::drop` in `crates/outrig/src/network.rs`, which handed over
the nft table delete followed by the resolver restore.

Since #328 the resolver is put back in-process, through a descriptor, before `Drop` returns, so
`Rollback::drop` hands the reaper only its `Cmd` undos -- in practice the one nft delete. Nothing
else calls `detach_cleanup_chain` with more than one command (`detach_cleanup` wraps a single
command in a one-element chain).

## Question

Keep the multi-command machinery as general-purpose capability, or collapse it to the single
command case and delete what only a chain needs (`ordered_shell`, the head-then-tail paths in
`hand_back`, and their tests)? Collapsing shrinks a subtle, mostly-unexercised fallback; keeping
it costs nothing until a second ordered cleanup appears.

## Acceptance

- Either the chain-only paths and their tests are removed with `detach_cleanup` as the single
  entry point, or a comment at `detach_cleanup_chain` records why the general form is kept.
