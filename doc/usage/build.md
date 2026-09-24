# `outrig build`

`outrig build` builds (or cache-hits) the image for one or more image-configs, without
starting an agent session. It's useful for:

- Verifying your `Dockerfile` builds cleanly before you start an agent.
- Pre-warming the image so the first `outrig run` is fast.
- CI: ensuring the agent's environment still builds after a dependency bump.

## Synopsis

```
outrig build [--image <name>]
             [--config <path>]
             [--no-cache]
             [--all]
```

- `--image <name>` (default: `default-image`): build a specific named
  image-config. This also reaches outrig's
  [built-in default](../reference/config.md#the-built-in-default-image-config) by name. It has
  three parts and one invocation warms one of them, so warming the set is three commands:
  `outrig build --image outrig-default` (pulls the primary), then `--image outrig-default-fs`
  (pulls the filesystem server), then `--image outrig-default-shell` (builds the shell server,
  the only part that is built rather than pulled).
- `--all` (default: off): build every image-config defined in the config file. The built-in
  default is deliberately not among them -- `--all` means the image-configs *you* declared,
  and pulling and building outrig's fallback in every repo would be a surprise.
- `--config <path>` (default: walks up from cwd): use a non-default config path.
- `--no-cache` (default: off): force rebuild even on cache hit. Passes `--no-cache`
  to `buildah`.

## What it does

1. Loads `.agents/outrig/config.toml` and validates every image-config in the merged config.
   Agent/model wiring is not required for image builds.
2. For each selected image-config:
   - Computes the cache key (blake3 over Dockerfile content, resolved build-args,
     the OutRig labels derived from `[images.<name>.mcp]`, and the context content hash).
   - If a tag matching that key exists and `--no-cache` is not set, prints
     `image ready (cache hit)` and skips.
   - Otherwise runs a buildah build to a temporary tag:
     ```
     buildah build --tag <image-config-name>:outrig-tmp-... \
                   --file <dockerfile> \
                   [user build-args] \
                   <context>
     ```
     Then it reads any inherited/Dockerfile `org.outrig.mcp` label, overlays
     `[images.<name>.mcp]`, and commits the final `<image-config-name>:<hash>` image with the
     merged `org.outrig.mcp` label.
     The temporary tag is then removed, whether or not the build succeeded. If buildah refuses
     the removal, outrig warns and reissues it in the background rather than leaving the tag
     behind; the build's own result stands either way.
     The repository is the `[images.<name>]` block key, so the built image is self-describing
     in `podman images`; the `<hash>` is the content-addressed cache key.

## Examples

Build the default:

```sh
$ outrig build
[outrig] image-config: coding
[outrig] dockerfile:       .agents/outrig/images/coding/Dockerfile
[outrig] context:          .agents/outrig/images/coding
[outrig] image tag:        coding:8c2a4f7e91d6b5a3
[buildah] STEP 1/6: FROM docker.io/library/node:20-bookworm-slim
...
[outrig] image ready: coding:8c2a4f7e91d6b5a3
```

Cache hit on the second run:

```sh
$ outrig build
[outrig] image ready (cache hit: coding:8c2a4f7e91d6b5a3)
```

Build a specific image-config:

```sh
$ outrig build --image planning
[outrig] image-config: planning
...
[outrig] image ready: planning:b91e3a6d217f4c08
```

Build every image-config in one go:

```sh
$ outrig build --all
[outrig] image-config: coding   -> coding:8c2a4f7e91d6b5a3 (cache hit)
[outrig] image-config: planning -> planning:b91e3a6d217f4c08 (built in 27s)
[outrig] all images ready
```

Force a rebuild without changing files:

```sh
$ outrig build --no-cache
```

## Cancelling a build

Ctrl-C, a timeout, or an abandoned MCP call stops a build in flight. outrig asks buildah to
stop rather than killing it outright, and waits a bounded grace before escalating, because a
buildah that unwinds removes the per-stage *working containers* it created and a killed one
does not. Those are the one engine resource outrig cannot name for itself: buildah derives
their names from the base image and offers no way to label them, so the only process that can
prove which belong to this build is buildah.

What a cancelled build leaves:

- **The temporary `outrig-tmp-*` tag** is removed. It carries this build's pid and a nonce, so
  nothing else on the machine can be what the removal names.
- **The stage working containers** are removed by buildah, if the stop landed while a `RUN` was
  executing.
- **The final `<name>:<hash>` tag** was never created; the commit is the last step.

The gap is the second row's condition. buildah installs its signal handler only while a `RUN`'s
command is running, so a build stopped during a pull, a `COPY`, the commit, or the seam between
two `RUN`s still ends where it stands and can leave a working container behind. Collect those
with [`outrig clean --build-containers`](sessions.md#outrig-clean).

## Exit codes

- `0` -- every selected image is built or cache-hit.
- non-zero -- at least one image failed to build. The buildah stderr is reproduced with the
  `[buildah]` prefix, ending with the exact failing step.

## See also

- [Concepts -> Containers](../concepts/containers.md) -- Dockerfile conventions.
- [Concepts -> Workspace](../concepts/workspace.md) -- runtime UID/GID mapping (no build-time
  user setup needed).
- [outrig run](run.md) -- what gets run after the image is ready.
- [Reference -> CLI](../reference/cli.md) -- every flag.
