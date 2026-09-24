# A refused working-container removal can strand the temporary tag

Found while fixing #165. When `commit_image_with_labels` fails and buildah also refuses the
awaited `buildah rm` of its `outrig-label-*` working container, the builder's `CleanupGuard`
drops armed and reissues the `rm` in the background. `into_temp_tag` then runs the awaited
`buildah rmi` of the temporary tag straight away. That `rmi` races the reissued `rm`, and while
the container still exists buildah refuses it as `image is in use by a container`. So the
tag's guard stays armed too.

The two obligations have separate retry budgets (`supervise::CLEANUP_RETRIES`, 250 ms
doubling backoff), and nothing orders one after the other. If the `rm` needs its own retries,
the `rmi`'s last attempt, about 750 ms in, can still land before the container is gone. The
tag is then abandoned, still tagged, where pruning never reaches it.

## Sketch

`supervise::detach_cleanup_chain` already exists for cleanups that must run in order. The
builder's removal and the tag's could leave as one chain, `rm` then `rmi`, when both are
still owed. The obstacle is that the builder's guard lives inside `commit_image_with_labels`,
two calls below `into_temp_tag`'s closure. It would have to be handed back out, or the tag's
guard handed down, which is more plumbing than #165 warranted.

Narrow in practice: it needs a refused commit *and* a refused `rm` *and* an engine slow enough
to outlast the backoff.
