# Sessions

A session is the on-disk record of one `outrig run`, `outrig run-new`, or `outrig mcp`
invocation: when it started and ended, which image-config it used, what image tag, plus
per-MCP-server stderr captured to disk, and -- for `run-new`, when asked -- what its agent did.
Sessions exist so you can go back and inspect what happened -- they are *not* a staging area for
workspace changes (outrig writes to your repo directly; see [Workspace](../concepts/workspace.md)).

## Where sessions live

Sessions live under a **session root** directory. Resolution order:

1. `--session-root <path>` flag (any session-touching subcommand).
2. `session-root` key in the global config.
3. `<XDG_DATA_HOME>/outrig/sessions/` (default; typically
   `~/.local/share/outrig/sessions/`).

Set `session-root` once in `~/.outrig/config.toml` to keep sessions somewhere persistent like
`/var/lib/outrig/sessions/`:

```toml
session-root = "/var/lib/outrig/sessions"
```

A relative value resolves against the global config's directory, and a leading `~` is your home
directory, so `session-root = "~/outrig-sessions"` works too. The key is global-only: one machine
keeps one root, so `outrig ls` finds the same sessions from any directory, and a repo config that
sets `session-root` is rejected at load. See
[Reference -> Config](../reference/config.md#top-level).

## Layout under the root

Each session is a subdirectory of the root, named by session id (a UTC timestamp plus four
random hex digits -- sortable, unambiguous across concurrent runs):

```
~/.local/share/outrig/sessions/
└── 20260501T134412-3f2a/
    ├── session.json              # id, timestamps, containers, image, exit status
    ├── outrig-enter              # launcher for view = "primary" sidecars, if any start
    ├── report.html               # the event log as a page, once you have rendered it
    └── logs/
        ├── container.log         # buildah/podman transcripts when --verbose is set
        ├── network.jsonl         # network audit/filter records when enabled
        ├── events.jsonl          # what a run-new agent did, when [events] records it
        ├── fs.stderr             # MCP "fs" server's captured stderr
        ├── shell.stderr          # MCP "shell" server's captured stderr
        ├── subagent-audit.log    # transcript of the subagent "audit"
        └── subagent-audit/       # transcripts of the subagents "audit" launched
            └── subagent-scan.log
```

`session.json` names every container the session owns: `container_name` for the primary,
plus `sidecar_container_names` when the config declares
[sidecars](../concepts/containers.md#sidecar-containers). Sidecar-less sessions omit the
field, so records from older versions read and write unchanged.

A session record is history: it describes a run that already happened, so it has to keep
reading after the schema moves on. `image_config_name` may be absent -- records written
before the `container` -> `image` rename stored the same value under
`container_config_name`, which is still accepted on read and rewritten under the current
name the next time outrig updates the record. A record with no image-config name at all
shows `-` in the `IMAGE` column.

When you run `outrig run --session-dir <path>`, outrig writes the session content into
`<path>` directly and creates a symlink at `<root>/<sid> -> <path>` so `outrig ls` still finds
it:

```
~/.local/share/outrig/sessions/
├── 20260501T134412-3f2a/        # auto-allocated, real directory
└── 20260501T141907-9b1c -> /tmp/my-debug-run/    # explicit --session-dir, symlink
```

This means you can launch with a known path and read `session.json`, `logs/`, etc. directly
without first looking up the auto-generated session id. Useful for scripted runs and
quick debugging:

```sh
$ outrig run --session-dir /tmp/my-debug-run < prompts.txt
$ cat /tmp/my-debug-run/session.json
$ tail -f /tmp/my-debug-run/logs/shell.stderr
```

`<path>` has to be empty or not exist yet; outrig creates it, with any missing parents. A
directory that already holds anything -- even a `.gitkeep` -- is refused, because from then on
the directory is the session's. Running again with the same path is refused for the same
reason, since the first run's `session.json` is still there: discard that session first, or
give each run its own path.
`outrig run-new --session-dir` takes the same path on the same terms.

## `outrig ls`

List sessions newest-first:

```sh
$ outrig ls
ID                     STARTED              DURATION  IMAGE      EXIT
20260501T141907-9b1c   2026-05-01 14:19:07  0m44s     coding     0   -> /tmp/my-debug-run
20260501T134412-3f2a   2026-05-01 13:44:12  2m18s     coding     0
20260430T091203-44d2   2026-04-30 09:12:03  6m02s     planning   1
```

`EXIT` is the outrig process exit code, not the agent's "did it succeed" signal -- there's no
formal way for the agent to declare success or failure. A non-zero exit usually means an LLM API
error or a container failure. `130`, `143`, and `129` mean a SIGINT, SIGTERM, or SIGHUP ended the
session -- Ctrl-C during startup, a `kill`, a closed terminal -- and it was torn down like any
other. The `-> /path` suffix marks sessions whose root entry is a symlink (created via
`outrig run --session-dir`).

`outrig mcp` sessions have no agent. Their in-memory session row has `agent_name = None`; new
on-disk `session.json` files omit `agent_name`, and older records with `"agent_name": null`
mean the same thing. `outrig ls` does not currently show an agent column, so these sessions appear
with the same `ID / STARTED / DURATION / IMAGE / EXIT` columns as `outrig run` sessions.

An entry that can't be read -- a truncated write, or a schema outrig no longer understands
-- costs only its own row. `outrig ls` reports it on stderr and lists everything else:

```
[outrig] skipping 20260430T091203-44d2: session.json at ...: missing field `image_tag`
```

If nothing in the root parsed, the notice says `no readable sessions (N skipped)` rather
than `no sessions`, so an unreadable store isn't mistaken for an empty one. Either way the
exit status is 0 -- skipping applies to reading and parsing a `session.json`, which is
reported rather than failed on. A fault reaching the entry at all (an unreadable session
root, or a directory entry outrig can't stat) is still an error. `outrig clean` treats an
unreadable record as still owning its container, so the container is not swept up as a
stray. `outrig discard <sid>` removes such a record after the usual confirmation, which says why
it is unreadable, and `outrig clean --unreadable` removes the ones past the cutoff.

Override the root for a single invocation with `--session-root`:

```sh
$ outrig ls --session-root /var/lib/outrig/sessions
```

## `outrig logs`

Print one server's captured stderr by session id:

```sh
$ outrig logs 20260501T134412-3f2a fs
[mcp-server-filesystem] starting; root=/workspace
[mcp-server-filesystem] tools/list: 3 tools advertised
[mcp-server-filesystem] tools/call: list_directory({"path": "/workspace"})
[mcp-server-filesystem] tools/call: read_file({"path": "/workspace/HELLO.txt"})
[mcp-server-filesystem] EOF on stdin; shutting down
```

Or read directly from a session directory (skips the id lookup):

```sh
$ outrig logs --session-dir /tmp/my-debug-run fs
```

Omit the server name to get a directory listing of available logs:

```sh
$ outrig logs 20260501T134412-3f2a
[outrig] logs for session 20260501T134412-3f2a:
  fs       (1.2 KiB)
  shell    (3.4 KiB)
```

A file in a subdirectory, like the transcript of a subagent another subagent launched, is listed
by its path under `logs/`.

Follow a still-running session's log:

```sh
$ outrig logs 20260501T141907-9b1c shell --follow
[shell-mcp-command] starting
[shell-mcp-command] exec: cargo check
warning: unused import: `std::collections::HashMap`
   --> src/lib.rs:3:5
    |
3   | use std::collections::HashMap;
...
```

`--follow` (`-f`) tails the file with `tail -F`-style behavior: continues when the file rotates,
returns when EOF is reached and no further writes are expected.

When network audit or filter mode is enabled, `logs/network.jsonl` contains one Zeek
`conn.log`-style JSON object per outbound TCP connection and one per name lookup. It is not
selected with the server-name argument because it is not an MCP stderr log; read it directly from
the session directory:

```sh
$ jq . /tmp/my-debug-run/logs/network.jsonl
```

The log uses Zeek field names where OutRig has equivalent data: `ts` is Unix epoch seconds,
`uid` is a per-connection id, `id.orig_h` / `id.orig_p` identify the container-side socket,
`id.resp_h` / `id.resp_p` identify the remote endpoint, `proto` is the transport, `service` is
the sniffed application (`http`, `ssl`, `ssh`, or `-`), `duration` is seconds, and
`orig_bytes` / `resp_bytes` count bytes sent and received by the container. OutRig-specific
metadata is namespaced under `outrig.*`, including `outrig.action`, `outrig.rule`,
`outrig.host`, and `outrig.host_source`. The last of those is `resolved` when the name in
`outrig.host` is one the interceptor validated for that address and `asserted` when the
container merely claimed it -- only a resolved name can have granted a hostname allow rule. A
connection that a hostname `allow` entry granted may therefore show an `asserted` host: the name
that earned the grant is a resolved one, which can be the name shown or another the container
resolved to that address. `outrig.rule` says which entry decided, so read it against the policy;
see [Reference -> Config](../reference/config.md#what-a-hostname-rule-matches). A
connection refused because its container already had 256 open through the interceptor is recorded
with `outrig.action` `deny` and `outrig.rule` `connection-limit`. One that reached the
interceptor's own listener without being redirected there -- a process in the container
connecting straight to the listener's port, which the redirect leaves alone because it is on
loopback -- is closed on arrival and recorded with `outrig.action` `deny` and `outrig.rule`
`not-redirected`. Its only destination is the listener itself, which the interceptor would
otherwise dial from the host's network namespace.

A name lookup is recorded as the UDP exchange with the interceptor's DNS listener that it is:
`proto` is `udp`, `service` is `dns`, `id.resp_p` is 53, and `outrig.host` is the name the
container asked for, with `outrig.host_source` `asserted`. `id.resp_h` is the address the query
reached the listener at, which is loopback whichever resolver the tool aimed at, since the
redirect has rewritten it by then. `orig_bytes` is the query's size. The record is written when
the lookup is decided, before any resolver is asked, so `duration` and `resp_bytes` are zero and
`conn_state` is `S0` however the lookup came out; `outrig.action` and `outrig.rule` say whether
it was answered or refused, and why. A query that is not a plain
question for one name is refused under `outrig.rule` `dns-unparsed` where any lookup could be,
and has no `outrig.host` when even its question could not be read. See
[Reference -> Config](../reference/config.md#name-lookups) for what decides a lookup.

Interception ends with the session, and ending it is the exact inverse of starting it. Every
connection still open through the interceptor is cut, and each one is recorded before teardown
returns, so the last line of `network.jsonl` is *in the file* by the time the session is down
rather than after it. In the file, not synced to the disk: outrig does not `fsync` per record,
so every record is there for anything that reads the file, and a host that loses power
mid-session can still lose the tail the kernel had not flushed.

The container's `/etc/resolv.conf`, which interception rewrote to point at the per-container DNS
listener, is restored byte for byte, and the container's redirect rules are removed. The same
applies to one sidecar detached mid-session: it goes back to resolving and routing the way it
did before it was attached. A container whose resolver was supplied at
creation is the one exception -- nothing was rewritten, so nothing is restored.

What teardown removes is what interception created, and nothing else. The redirect table is
removed by the handle the kernel assigned it, and only while its name is still present, so a
table something replaced under that name is left alone and so is a namespace this removal
reaches by a pid that has since been reused.

If any part of that fails -- the resolver cannot be written back, the rules cannot be removed,
a task does not end, a listener had stopped taking the container's traffic before teardown began
-- the remaining parts still run, and the failure is *returned* rather than logged and swallowed.
Returned to the caller of the interceptor, that is: a library embedding outrig gets it and
decides. `outrig` the command is one such caller, and what it decides is to log it and carry on
stopping containers, because stopping them is the more urgent half, so from the command line these
failures appear in `container.log` rather than in the exit status.

Attaching has the same property from the other side: a failed or interrupted attach leaves the
container exactly as it found it, so a container never ends up resolving to a listener that is
not there.

## The event log

With `[events] mode = "record"` in either config file, `outrig run-new` writes
`logs/events.jsonl`: what its agent did, one event per line, in the order it happened. Each model
call and the turns it was sent, each piece of Python the model submitted and how it ended, each
message on the agent's channel, the tokens each round used, and what OutRig did along the way --
a Ctrl-C, an interrupt, an execution it stopped waiting for. With the mode off, which is the
default, there is no file.

```toml
[events]
mode = "record"
```

Every event is a [CloudEvents](https://cloudevents.io/) record whose `data` is OutRig's; the
[event log reference](../reference/events.md) lists them all, with the rule each one is recorded
under. Read it directly from the session directory:

```sh
$ jq -c 'select(.type == "org.outrig.exec.completed") | .data | {execid, status, duration}' \
    /tmp/my-debug-run/logs/events.jsonl
```

Like `network.jsonl`, it is not selected with `outrig logs`'s server-name argument: it is not an
MCP stderr log. Unlike it, the file is readable by its owner only, because it holds what was
typed to the agent and what the agent sent back. Discarding the session removes it with
everything else.

The file is finished as the session stops, so every event it got is *in the file* by the time
`run-new` exits, on the same terms as `network.jsonl`: not synced to the disk. Nothing the agent
does waits for it: if the disk cannot keep up, the events it has not taken wait, up to a bound,
and past that the oldest are dropped, which shows in the file as a jump in the records' ids. Any
event the file never got is counted in a warning at exit.

### Reading it in a browser

`scripts/render-session.py` turns a session into one HTML page. It takes a session id, or enough
of one to name a single session, as `outrig logs` does, and finds it under the
[session root](#where-sessions-live) the way `outrig` does; `--session-root <path>` looks under
another. It takes a session directory as well. It reads `session.json`, `logs/events.jsonl`, and
`logs/network.jsonl` when there is one, and shows the session; whether its record is whole; the
tokens each round used; a timeline of the rounds, with each round's model calls, turns,
executions, and messages linked by their ids; every execution's source, output, and traceback;
the messages; and the connections. Nothing on the page runs and nothing on it loads from
anywhere.

```sh
$ uv run --script scripts/render-session.py 3f2a
/home/me/.local/share/outrig/sessions/20260501T134412-3f2a/report.html
$ uv run --script scripts/render-session.py /tmp/my-debug-run
/tmp/my-debug-run/report.html
```

[uv](https://docs.astral.sh/uv/) is all it needs: the script declares its Python version and its
one dependency, Jinja, inline, and `uv` provides both. The page is written into the session
directory as `report.html`, readable by its owner only, because it holds what `events.jsonl`
holds; `outrig discard` removes it with the rest. `--out <path>` writes it elsewhere, on the same
terms. A log still being written, or one whose session died mid-line, renders anyway: a line that
does not parse is skipped and listed on the page.

`--host` serves the page instead of writing it, until you stop it with Ctrl-C, and prints the
URL it bound on stdout before anything else. Bare, it binds the loopback on a port the kernel
picks, so nothing else on the network can reach it and no port of yours is taken:

```sh
$ uv run --script scripts/render-session.py 3f2a --host
http://127.0.0.1:38417/
render-session: serving /home/me/.local/share/outrig/sessions/20260501T134412-3f2a; Ctrl-C to stop
```

The argument is `[HOST][:PORT]`, each half optional: `--host :9090` takes that port on the
loopback, `--host 127.0.0.1:9090` says the same, `--host 0.0.0.0:0` binds every interface on a
port the kernel picks, and an IPv6 literal is bracketed, `--host '[::1]:9090'`. A host that is a
name is resolved. Binding past the loopback prints a line saying so, because the page holds what
`events.jsonl` holds -- submitted source, captured output, and message bodies -- and the server
asks for no password.

Each request builds the page again, so a reload follows a session still being written. Only `/`
is served; nothing else in the session directory is. Serving writes no file unless `--out` is
given as well, in which case the page is written first and then served.

The script is in OutRig's repository, not in the installed binary. If you installed with `cargo
install`, take the copy from the `outrig-cli-vX.Y.Z` tag matching your `outrig --version`: the
event log can change between versions, and each copy reads its own version's.

## `outrig discard`

Delete a session's directory -- `session.json`, `logs/`, `outrig-enter`, a rendered
`report.html`, and anything else in it:

```sh
$ outrig discard 20260430T091203-44d2 --yes
[outrig] removed ~/.local/share/outrig/sessions/20260430T091203-44d2
```

For sessions whose root entry is a symlink, discard removes both the real directory at the
symlink target and the symlink itself:

```sh
$ outrig discard 20260501T141907-9b1c --yes
[outrig] removed /tmp/my-debug-run
[outrig] removed ~/.local/share/outrig/sessions/20260501T141907-9b1c (symlink)
```

You can also point at a session directory directly, or at its symlink under the root, which
discards the directory the symlink names:

```sh
$ outrig discard --session-dir /tmp/my-debug-run --yes
```

`--yes` skips the interactive confirmation. Discard removes the directory whole: a session's
directory starts new or empty, so everything in it is the session's. A symlink inside it is
unlinked, never followed. Versions before 0.2.2 let a session start in a directory that already
held other files, and discarding one of those sessions removes them too.

A session that's still running can't be discarded; outrig refuses with an error pointing at the
running container.

## `outrig clean`

Delete old stopped session records in bulk. Bare `outrig clean` uses a 30-day retention
window, previews the sessions it will remove, then asks once before deleting:

```sh
$ outrig clean
[outrig] will remove 2 sessions older than 30d:
  20260401T134412-3f2a  ended 2026-04-01 13:46:30  /tmp/old-a
  20260402T091203-44d2  ended 2026-04-02 09:18:54  /tmp/old-b
Clean 2 sessions? [y/N]:
```

Use `--older-than` to pick a different cutoff. Durations are positive integers with `s`, `m`,
`h`, or `d` units:

```sh
$ outrig clean --older-than 7d
$ outrig clean --older-than 12h
```

Use `--yes` (`-y`) to skip the prompt:

```sh
$ outrig clean --older-than 30d --yes
```

`outrig clean` uses the same deletion semantics as `outrig discard`: auto-allocated sessions
remove the session directory, while sessions created with `--session-dir` remove both the
symlink target and the symlink under the session root, each directory whole. Running sessions
are skipped so cleanup doesn't race a live `outrig run`, `outrig run-new`, or `outrig mcp`
writer.

A record outrig cannot read is not shown to you, so `--yes` does not remove it: `clean` names
it and leaves it. Add `--unreadable` to have it previewed and removed with the rest, aged by the
start time in its directory name; an entry whose name is not a session id is never removed, and
neither is a record too damaged to say which container it owns -- `outrig discard <sid>` removes
that one.

Alongside the record walk, `outrig clean` sweeps for *stray containers*: podman containers
carrying the `org.outrig.session` label whose session record no longer exists (a lost record,
a SIGKILLed outrig, a failed `--rm`). Stopped strays older than the cutoff are removed with
`podman rm -f` and appear in the same preview/confirmation; running labeled containers are
never removed, only reported:

```sh
$ outrig clean --older-than 7d
[outrig] will remove 1 session older than 7d:
  20260401T134412-3f2a  ended 2026-04-01 13:46:30  /tmp/old-a
[outrig] will remove 1 stray container older than 7d (no session record):
  outrig-20260330T101500-88aa-tools  sidecar tools, session 20260330T101500-88aa
Clean 1 session and 1 stray container? [y/N]:
```

Each `[outrig] removed container <name>` line is a claim about that container, checked by
re-reading the container list rather than inferred from the batched `podman rm -f`'s exit
status -- one status for every name in the batch cannot say which of them went away. A name the
batch skipped is retried on its own; one that is still there afterwards is reported as
`could not remove container <name>`, and a sweep that ends with any of those exits `1`.

A third sweep, `--build-containers`, is available for the buildah *working containers* an
interrupted build can leave behind. It is off by default and is the one sweep outrig cannot
scope to its own work: buildah offers no way to mark these containers, so the sweep covers
every buildah working container past the cutoff, including one you made yourself with
`buildah from`. Removal is by container id, everything is previewed before the prompt, and a
container whose creation time cannot be read is reported and left alone:

```sh
$ outrig clean --older-than 7d --build-containers
[outrig] will remove 1 build container older than 7d (buildah working containers):
  213c716fa504  outrig-cache-working-container  image localhost/outrig-cache:d8aa33
Clean 1 build container? [y/N]:
```

The cutoff is what keeps this away from a build that is running right now, so do not pair
`--build-containers` with a short `--older-than` while one is in flight.

## What sessions don't track

- **Workspace changes.** outrig uses a direct bind-mount; mutations land on your host filesystem
  in real time. Use `git diff`, `git status`, etc. for review. There is no per-session changeset
  on disk.
- **Conversation history, for `outrig run`.** `run` doesn't persist the LLM conversation. If you
  need a transcript, redirect stdin/stdout (`outrig run < prompts.txt > replies.txt`).
  `--verbose` captures buildah/podman traces in `container.log`; it is not a conversation
  transcript. `outrig run-new` records its conversation in [the event log](#the-event-log) when
  asked.
- **API keys.** The bearer token used for the LLM call is never written to disk and never appears
  in tracing output.
- **Referenced env values.** A `${VAR}` build-arg or MCP `env` value appears in `container.log`,
  errors, and debug output only as `KEY=${VAR}`, and reaches podman and buildah through their
  environment rather than their command line -- with the exceptions listed under
  [the value syntax](../reference/config.md#mcp-env-value-syntax). What a build step or an MCP
  server prints is recorded as printed.

## Concurrent sessions

You can run multiple `outrig run` invocations against the same repository simultaneously. Each
gets its own container, its own session id, its own logs directory. They share the bind-mounted
workspace, so file writes in one session are visible to the other immediately -- be aware if
you're running two agents on overlapping work.

## See also

- [outrig run](run.md) -- what produces a session in the first place.
- [outrig mcp](mcp.md) -- stdio MCP server sessions with no agent.
- [Concepts -> Workspace](../concepts/workspace.md) -- why there's no per-session changeset.
- [Reference -> CLI](../reference/cli.md) -- every flag for `ls`, `logs`, `discard`, and
  `clean`.
- [Reference -> Config](../reference/config.md) -- the `session-root` and `[events]` config keys.
- [Reference -> Event log](../reference/events.md) -- every event `events.jsonl` holds.
