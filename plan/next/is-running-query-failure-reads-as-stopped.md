# Container::is_running treats a failed engine query as "stopped"

Flagged in review of the #869 fix (pre-existing, unrelated to that change).

`Container::is_running` maps every nonzero `podman inspect` exit -- "no such container"
and "podman could not be reached" alike -- to `false`. If a borrowed or orphaned
container is actually running without an owner claim on its session directory while the
engine query happens to fail (a podman hiccup, a timeout, a transient socket error),
`discard`'s running check reads it as stopped, and confirmation (or `--yes`) deletes its
record, `logs/`, and launcher out from under it.

Fix: distinguish "no such container" (a real, informative answer: stopped) from a query
that did not complete (unknown), and propagate the latter as an error rather than letting
unknown liveness read as stopped.

Location: `crates/outrig-cli/src/cli/discard.rs` (`podman_is_running`), and
`Container::is_running` in `crates/outrig/src/container.rs`.
