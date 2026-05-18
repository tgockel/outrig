# Sessions

A session is the on-disk record of one `outrig run` or `outrig mcp` invocation: when it started
and ended, which container-config it used, what image tag, plus per-MCP-server stderr captured to
disk. Sessions exist so you can go back and inspect what happened -- they are *not* a staging area
for workspace changes (outrig writes to your repo directly; see
[Workspace](../concepts/workspace.md)).

## Where sessions live

Sessions live under a **session root** directory. Resolution order:

1. `--session-root <path>` flag (any session-touching subcommand).
2. `session-root` key in repo or global config.
3. `<XDG_DATA_HOME>/outrig/sessions/` (default; typically
   `~/.local/share/outrig/sessions/`).

Set `session-root` once in `~/.outrig/config.toml` to keep sessions somewhere persistent like
`/var/lib/outrig/sessions/`:

```toml
session-root = "/var/lib/outrig/sessions"
```

## Layout under the root

Each session is a subdirectory of the root, named by session id (a UTC timestamp plus four
random hex digits -- sortable, unambiguous across concurrent runs):

```
~/.local/share/outrig/sessions/
└── 20260501T134412-3f2a/
    ├── session.json              # id, timestamps, container, image, exit status
    └── logs/
        ├── container.log         # buildah/podman transcripts when --verbose is set
        ├── network.jsonl         # network audit/filter records when enabled
        ├── fs.stderr             # MCP "fs" server's captured stderr
        └── shell.stderr          # MCP "shell" server's captured stderr
```

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

## `outrig ls`

List sessions newest-first:

```sh
$ outrig ls
ID                     STARTED              DURATION  CONTAINER  EXIT
20260501T141907-9b1c   2026-05-01 14:19:07  0m44s     coding     0   -> /tmp/my-debug-run
20260501T134412-3f2a   2026-05-01 13:44:12  2m18s     coding     0
20260430T091203-44d2   2026-04-30 09:12:03  6m02s     planning   1
```

`EXIT` is the outrig process exit code, not the agent's "did it succeed" signal -- there's no
formal way for the agent to declare success or failure. A non-zero exit usually means an LLM API
error, a container failure, or a SIGINT after the user gave up. The `-> /path` suffix marks
sessions whose root entry is a symlink (created via `outrig run --session-dir`).

`outrig mcp` sessions have no agent. Their in-memory session row has `agent_name = None`; new
on-disk `session.json` files omit `agent_name`, and older records with `"agent_name": null`
mean the same thing. `outrig ls` does not currently show an agent column, so these sessions appear
with the same `ID / STARTED / DURATION / CONTAINER / EXIT` columns as `outrig run` sessions.

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
`conn.log`-style JSON object per outbound connection. It is not selected with the server-name
argument because it is not an MCP stderr log; read it directly from the session directory:

```sh
$ jq . /tmp/my-debug-run/logs/network.jsonl
```

The log uses Zeek field names where OutRig has equivalent data: `ts` is Unix epoch seconds,
`uid` is a per-connection id, `id.orig_h` / `id.orig_p` identify the container-side socket,
`id.resp_h` / `id.resp_p` identify the remote endpoint, `proto` is the transport, `service` is
the sniffed application (`http`, `ssl`, `ssh`, or `-`), `duration` is seconds, and
`orig_bytes` / `resp_bytes` count bytes sent and received by the container. OutRig-specific
metadata is namespaced under `outrig.*`, including `outrig.action`, `outrig.rule`, and
`outrig.host`.

When MITM is enabled, each HTTPS request produces an additional record with
`service = "ssl-mitm"` carrying `method`, `url`, and `status` (and, with `capture-bodies` on,
`outrig.request_body_b64`, `outrig.response_body_b64`, and `outrig.body_truncated`). The `uid`
on the MITM record matches the TCP-level record for the same connection, so grouping with
`jq 'group_by(.uid)'` recovers the request flow. See
[Concepts -> Network MITM](../concepts/network-mitm.md).

## `outrig discard`

Delete a session's entire on-disk record (including logs):

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

You can also point at a session directory directly:

```sh
$ outrig discard --session-dir /tmp/my-debug-run --yes
```

`--yes` skips the interactive confirmation. This is destructive (it `rm -rf`'s the session
directory) but only of the session record itself -- your repository is untouched. A session
that's still running can't be discarded; outrig refuses with an error pointing at the running
container.

> **TODO: Incomplete** -- bulk discard (`outrig discard --older-than 7d`) is deferred.

## What sessions don't track

- **Workspace changes.** outrig uses a direct bind-mount; mutations land on your host filesystem
  in real time. Use `git diff`, `git status`, etc. for review. There is no per-session changeset
  on disk.
- **Conversation history.** v0 doesn't persist the LLM conversation. If you need a transcript,
  redirect stdin/stdout (`outrig run < prompts.txt > replies.txt`). `--verbose` captures
  buildah/podman traces in `container.log`; it is not a conversation transcript.
- **API keys.** The bearer token used for the LLM call is never written to disk and never appears
  in tracing output.

> **TODO: Incomplete** -- opt-in transcript capture (per-turn JSON of user/assistant/tool
> messages) is deferred.

## Concurrent sessions

You can run multiple `outrig run` invocations against the same repository simultaneously. Each
gets its own container, its own session id, its own logs directory. They share the bind-mounted
workspace, so file writes in one session are visible to the other immediately -- be aware if
you're running two agents on overlapping work.

## See also

- [outrig run](run.md) -- what produces a session in the first place.
- [outrig mcp](mcp.md) -- stdio MCP server sessions with no agent.
- [Concepts -> Workspace](../concepts/workspace.md) -- why there's no per-session changeset.
- [Reference -> CLI](../reference/cli.md) -- every flag for `ls`, `logs`, and `discard`.
- [Reference -> Config](../reference/config.md) -- the `session-root` config key.
