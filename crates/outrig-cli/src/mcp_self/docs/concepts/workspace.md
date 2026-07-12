# Workspace

The workspace is what the agent's tools get to read and write. By default, your repository is
mounted live into the container at `/workspace`. You can also mount extra resource directories,
read-only by default, when the agent needs supporting material outside the repo. Read this page
before you run outrig on anything you can't easily roll back.

## Direct bind-mount, no staging

outrig mounts the primary host workspace directly:

```
podman run -v <repo>:/workspace:rw --userns=keep-id ...
```

That means **changes made by tools inside the container appear on your host filesystem
immediately** for any read-write bind mount. There is no staging directory, no overlay, no
per-session shadow copy. If the agent runs `rm -rf /workspace/*`, your repo is gone --
recoverable only via git or whatever backup you have. Extra mounts default to read-only, but an
extra `access = "read-write"` mount has the same immediate-write behavior.

This is intentional. The alternative -- staging changes in a sandbox and asking you to "apply"
them after the session -- has a few real costs:

- Upfront copy time, every session, on potentially large repos.
- A second source of truth between session-end and apply-time that can drift.
- Friction every time you want to actually use the agent's output.
- An apply step that may merge surprisingly with files you've edited concurrently.

For an autonomous-agent workflow, the better safety net is **git**, not a staging step:

- Run outrig on a feature branch.
- Commit (or stash) your work-in-progress before starting.
- After the session, use `git diff`, `git status`, `git checkout -- <path>`, `git reset --hard`,
  etc. to review and roll back as you would any change.
- If the agent really destroyed something, that's what `git reflog` is for.

If you find yourself wanting a "review before apply" step, that's a signal you don't yet trust the
agent's environment -- usually the right fix is to tighten the Dockerfile or the MCP server
choices, not to layer a staging mechanism over the top.

## UID/GID: runtime user mapping

By default, rootless podman maps the container's UID 0 (root) to your host UID. Anything the
container does as root therefore appears on the host as files you own -- but anything done as a
*non-root* in-container UID maps to some scrambled subuid number that's awkward to clean up.

outrig sidesteps this in two parts:

1. **`--userns=keep-id`** on the run command. With this, your host UID/GID map straight onto the
   same UID/GID inside the container's user namespace -- so an in-container process running as
   UID 1000 produces files owned by host UID 1000 on the bind-mount.
2. **A startup bootstrap** that creates a matching user *inside* the container at run time, so
   tools that call `getpwuid()` (some shells, npm postinstall scripts, etc.) don't fail on a
   missing `/etc/passwd` entry.

After `podman run -d` brings the container up, but before any MCP server starts, outrig (running as
in-container root) does:

```sh
# Group: reuse if a group with this GID already exists, else create one.
gid=$(id -g) ; gname=$(id -gn)
existing=$(getent group "$gid" | cut -d: -f1)
if [ -z "$existing" ]; then
    # name collisions: append _ until groupadd succeeds
    until groupadd --gid "$gid" "$gname"; do gname="${gname}_" ; done
fi

# User: same dance.
uid=$(id -u) ; uname=$(id -un)
existing=$(getent passwd "$uid" | cut -d: -f1)
if [ -z "$existing" ]; then
    until useradd -u "$uid" -g "$gid" "$uname"; do uname="${uname}_" ; done
fi

# Some tools assume $HOME exists.
mkdir -p "/home/$uname"
chown "$uname:$gname" "/home/$uname"
```

After bootstrap, every `podman exec` outrig issues -- to start MCP servers, to run anything else
-- uses `--user=$(id -u):$(id -g)`. Files written under `/workspace` therefore appear with your
UID/GID on the host. The image itself doesn't need any user setup -- whatever base image you
pick works as long as `useradd`/`groupadd` are available.

The collision dance handles the case where the image already has a group or user at your UID/GID
(common for `1000:1000` -- the typical first non-root user in many distros). When that happens,
outrig reuses the existing entry rather than creating a duplicate.

## What's mounted, what isn't

The `[workspace]` block controls what host directories the container sees:

```toml
[workspace]
host-path      = "."          # relative to the repo root containing .agents/outrig/
container-path = "/workspace"

[[workspace.mounts]]
host-path      = "../shared-docs"
container-path = "/resources/shared-docs"

[[workspace.mounts]]
host-path      = "/var/tmp/outrig-cache"
container-path = "/resources/cache"
access         = "read-write"
```

`host-path = "."` (the default) mounts your whole repo. You can narrow this -- e.g.
`host-path = "src"` mounts only the source dir. The primary workspace is always read-write and
becomes the container workdir.

Extra `workspace.mounts` entries are for supporting directories: sibling repos, generated docs,
SDK checkouts, model artifacts, or caches. Relative extra host paths resolve against the repo
root, just like the primary workspace. Their container paths must be absolute, cannot be `/`, and
must not duplicate the primary workspace or another extra mount. Exact duplicates fail during
config validation instead of relying on podman mount ordering.

Extra mounts default to `access = "read-only"`. Use `access = "read-write"` only when the agent
really should mutate that host directory, such as a scratch cache under `/var/tmp`.

Files outside the declared bind-mounts are *not* reachable from the container under any
circumstances:

- `~/.ssh` -- not mounted, agent can't read your keys.
- `~/.config` -- not mounted.
- `/etc/passwd` on the host -- not mounted; the container has its own.

The container has its own `/etc`, `/home`, `/tmp`, etc. coming from the image. The only windows
onto the host filesystem are the primary workspace and the extra mounts you declare.

### Sidecar containers see nothing by default

MCP sidecar containers (see [Containers](containers.md#sidecar-containers)) get their own,
stricter defaults: `workspace = "none"` -- a sidecar sees no host directory at all unless its
block asks. `workspace = "ro"` mounts the session workspace read-only at the primary's
container path (and makes it the working directory for the sidecar's servers);
`workspace = "rw"` mounts it read-write. Extra `mounts` entries on a sidecar use the same
shape and validation as `[[workspace.mounts]]`.

Sidecars run with `--userns=keep-id` like the primary. The in-container user bootstrap runs
only where identity matters: the sidecar hosts at least one exec-stdio server, sees the
workspace, or declares mounts. A mount-less sidecar keeps the image's own `USER` untouched.

## Network is *not* part of the workspace

By default, outrig grants the container full outbound network access. This means an agent with
shell access can run `curl`, `git push`, `npm publish`, etc. -- anything that talks to the
outside world.

Network audit and filter modes are opt-in. Set one in config, or override one fresh session
with `--network audit` or `--network filter`, to put a host-side interceptor in the path of
outbound container traffic:

```toml
[network]
mode = "audit"
```

Audit mode writes one Zeek `conn.log`-style JSON object per connection to
`<session_dir>/logs/network.jsonl`. Audit mode is allow-and-log only: every connection is still
allowed, but records include the best known host, destination IP and port, transport, service,
byte counts, and duration. HTTPS remains opaque except for TLS SNI. URL, method, status, and
body inspection are deferred.

Filter mode uses the same interceptor and audit log, then applies global host/port policy
before opening upstream TCP connections:

```toml
[network]
mode    = "filter"
default = "deny"
allow   = ["github.com:443", "*.npmjs.org"]
deny    = ["*:22"]
```

Policy is global-only because it belongs to the machine running the agent. Repo config may
choose `network.mode`, but cannot set `default`, `allow`, or `deny`. Deny entries win over
allow entries, and unmatched connections use `default`. A denied connection is closed
immediately and still writes a `network.jsonl` record with `outrig.action = "deny"` and zero
byte counts, so the audit log is the place to diagnose network policy failures.

When network mode is `default`, outrig leaves Podman's configured default networking in place,
does not install nftables rules, does not rewrite container DNS, and does not create
`network.jsonl`. Systems without nftables or namespace support therefore keep running normal
sessions. If audit or filter mode is explicitly enabled and the host cannot install the
interceptor, startup fails before MCP servers launch.

## See also

- [Containers](containers.md) -- Dockerfile conventions, including the UID/GID setup.
- [MCP Servers](mcp-servers.md) -- the layer that bounds what the agent can actually *do* with the
  workspace.
- [Reference -> Config](../reference/config.md) -- full schema for the `[workspace]` block.
