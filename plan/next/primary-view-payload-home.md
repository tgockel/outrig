# A `view = "primary"` payload keeps the image's `HOME`

Task 0102 made the payload run as the session's uid/gid, but nothing changes its environment: it
inherits whatever the sidecar image sets, and an image whose `USER` is root usually sets
`HOME=/root`. So a server that writes into `$HOME` -- a cache, a config file, a lockfile -- now
gets `EACCES` where it previously succeeded as root.

Exec-stdio servers do not have this problem: `Container::build_exec_argv`
(`crates/outrig/src/container/mod.rs`) passes `--env HOME=<in-container home>` from
`userdb::home_dir(user_name)`, which the user bootstrap created. An entrypoint host skips the
bootstrap by construction, so there is no in-container home to name and no exec to name it on --
the env has to be baked in at `podman create` time instead.

The primary's own `/home/<user>` *is* reachable through the view, since the sidecar joins the
primary's mount namespace and the primary was bootstrapped. So the fix is likely a `HOME` in the
sidecar's create-time env pointing at the primary's home directory, resolved from the primary
`Container`'s `user_name`. Worth checking whether `XDG_CACHE_HOME` and friends want the same
treatment, and whether an explicitly configured `env` entry should win (it should).

Not urgent: no server OutRig ships or documents in this placement writes to `$HOME`. It becomes
urgent the first time one does, and the symptom -- a server that starts, serves, and then fails
one tool call with a permission error on a path nobody configured -- is hard to attribute.

## See also

- `plan/done/0102-outrig-enter-privilege-drop.md` -- the drop that exposes this.
- `crates/outrig/src/container/mod.rs` -- `build_exec_argv`, and `userdb::home_dir`.
