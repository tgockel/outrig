# Workspace

> **TODO: Incomplete** -- every behavior described here is the intended behavior; the
> implementation isn't ready yet.

The workspace is what the agent gets to read and write. By default it's your repository, mounted
live into the container at `/workspace`. Read this page before you run outrig on anything you
can't easily roll back.

## Direct bind-mount, no staging

outrig mounts the host workspace directly:

```
podman run -v <repo>:/workspace:rw --userns=keep-id ...
```

That means **changes the agent makes inside the container appear on your host filesystem
immediately**. There is no staging directory, no overlay, no per-session shadow copy. If the agent
runs `rm -rf /workspace/*`, your repo is gone -- recoverable only via git or whatever backup you
have.

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

The `[workspace]` block controls what the container sees:

```toml
[workspace]
host-path      = "."          # relative to the repo root containing .agents/outrig/
container-path = "/workspace"
```

`host-path = "."` (the default) mounts your whole repo. You can narrow this -- e.g.
`host-path = "src"` mounts only the source dir. Anything outside `host-path` is invisible to the
container.

Files outside the bind-mount are *not* reachable from the container under any circumstances:

- `~/.ssh` -- not mounted, agent can't read your keys.
- `~/.config` -- not mounted.
- `/etc/passwd` on the host -- not mounted; the container has its own.

The container has its own `/etc`, `/home`, `/tmp`, etc. coming from the image. The only window
onto the host filesystem is the workspace mount.

## Network is *not* part of the workspace

v0 outrig grants the container full outbound network access. This means an agent with shell
access can run `curl`, `git push`, `npm publish`, etc. -- anything that talks to the outside
world.

> **TODO: Incomplete** -- egress interception (CONNECT proxy + per-host allowlist + per-session
> log) is deferred.

If the consequences of free network access matter for your repo, gate the agent at the MCP-server
level for now: don't include a `shell` MCP, only include MCP servers whose tools are scoped (a
filesystem MCP, a code-search MCP, a tightly-defined custom MCP). The agent then can't shell out
to arbitrary network calls because the tool surface doesn't expose any.

## See also

- [Containers](containers.md) -- Dockerfile conventions, including the UID/GID setup.
- [MCP Servers](mcp-servers.md) -- the layer that bounds what the agent can actually *do* with the
  workspace.
- [Reference -> Config](../reference/config.md) -- full schema for the `[workspace]` block.
