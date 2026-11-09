# Effective capabilities check misses a reacquirable NET_ADMIN via BoundingCaps

## Context

`crates/outrig/src/container/mod.rs:1133-1154` (`Container::attached_holds_net_admin`,
landed for #549) asks `podman inspect --format {{.EffectiveCaps}}` for a *borrowed*
container and refuses `NetworkInterceptor::attach` when `CAP_NET_ADMIN` is in it.

A code review on the #549 PR flagged a gap this does not close: `EffectiveCaps` is what
the container's init process holds *right now*, not what it could regain. A borrowed
rootless container started `--privileged --user=1000` with no `cap-add` can report
`EffectiveCaps=[]` while `BoundingCaps` still lists `CAP_NET_ADMIN` -- `--privileged` sets
the bounding set to everything regardless of `--user`. With `no-new-privileges` disabled
(outrig does not control a borrowed container's launch, so it cannot require this) and a
setuid-root helper on a non-`nosuid` mount somewhere in the image, a process in the
container can re-acquire `CAP_NET_ADMIN` from the bounding set and delete the interception
table, bypassing audit/filter mode with nothing recorded -- the same consequence #363 and
#549 both refuse against, through a path neither check reads.

This is **not a #549 regression**: the launch-derived `holds_net_admin` check an *owned*
container gets has the identical gap today (a profile or `cap-add`/`cap-drop` combination
that leaves `CAP_NET_ADMIN` only in the bounding set, not the effective one, was never
read before `attached_holds_net_admin` existed either). #549 made the borrowed path as
good as the owned one; it did not make either path read the bounding set.

## Suggested fix direction

`podman inspect --format '{{.EffectiveCaps}} {{.BoundingCaps}}'` (or two inspects, or a
JSON inspect read once) and refuse on `CAP_NET_ADMIN` in *either* list, for both the
owned and the borrowed path. Measure what `--privileged`, a plain `cap-add`, and a
`cap-drop` of `ALL` with `no-new-privileges` each put in `BoundingCaps` before changing
the read -- `reject_malformed_capabilities`'s existing spellings test
(`container::tests::grants_net_admin_follows_what_podman_would_hand_the_container`) is the
shape to extend or pair with an inspect-based equivalent.

## Suggested regression tests

- A borrowed (or owned) container with `--privileged --user=1000` and no `cap-add`: refused,
  where it is not today.
- The existing owned/borrowed `CAP_NET_ADMIN`-via-`cap-add` and `SYS_ADMIN`-is-fine cases still
  pass with the wider read.
