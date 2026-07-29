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

## Now observed, not hypothetical

This entry used to close with "Not urgent: no server OutRig ships or documents in this placement
writes to `$HOME`." That is false as of 0104, which put this repo's own `shell` server
(`mcp-server-commands`, from `node:22-slim`) on `view = "primary"`. `node:22-slim` sets
`HOME=/root`, `/root` is `drwx------ root root`, and the payload now runs as uid 1000 -- so the
predicted `EACCES` is live in this repo's daily configuration.

Confirmed from inside the running sidecar:

```
$ id                       # uid=1000(travis), CapEff=0000000000000000, Groups: (empty)
$ echo $HOME               # /root
$ ls -ld /root             # drwx------ 1 root root
$ touch /root/probe        # Permission denied
```

The concrete casualty is `git`, and through it `cargo`:

```
$ cargo clippy -p outrig --all-targets
error: failed to determine package fingerprint for build script for outrig v0.2.0-rc.1
  Could not read repository exclude
  Permission denied (os error 13)
```

libgit2 resolves `core.excludesFile` under `$HOME`, cannot stat `/root/.config/git/ignore`, and
returns a hard error rather than skipping it; cargo surfaces that as a fingerprint failure. Plain
`cargo build` succeeds, because it does not walk the package directory the same way -- so the
failure looks arbitrary until `HOME` is suspected. Every bare `git` invocation also emits
`warning: unable to access '/root/.config/git/ignore': Permission denied` on stderr, which is the
visible tell. `HOME=/tmp` in front of the same commands makes both go away, deterministically.

That sharpens the severity: the symptom is not confined to servers that deliberately write to
`$HOME`. Any tool that merely *reads* per-user config through `$HOME` can fail, and `git` is in
that set, so the blast radius is every shell server on this placement in a repo that is a git
checkout.

## One wrinkle for the fix

`userdb::home_dir(name)` returns `/home/<name>`, but the primary's own `/etc/passwd` -- readable
through the view -- names `/workspace` as this user's home:

```
travis:*:1000:1000:Travis Gockel:/workspace:/bin/sh
```

Both `/home/travis` and `/workspace` exist and are owned by uid 1000, so either would resolve the
`EACCES`. They are not interchangeable, though: `/workspace` is the bind-mounted repo, so a `HOME`
pointing there means caches and config land in the user's checkout. Decide deliberately which one
the sidecar's create-time `HOME` should name, and note that `build_exec_argv` already commits
exec-stdio servers to `userdb::home_dir` -- so picking `/workspace` here would make the two
placements disagree about `$HOME`, which is its own bug class.

## See also

- `plan/done/0102-outrig-enter-privilege-drop.md` -- the drop that exposes this.
- `plan/done/0104-dogfood-sidecar-mcp-config.md` -- shipped the first affected server.
- `crates/outrig/src/container/mod.rs` -- `build_exec_argv`, and `userdb::home_dir`.
