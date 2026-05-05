# `outrig build`

`outrig build` builds (or cache-hits) the image for one or more container-configs, without
starting an agent session. It's useful for:

- Verifying your `Dockerfile` builds cleanly before you start an agent.
- Pre-warming the image so the first `outrig run` is fast.
- CI: ensuring the agent's environment still builds after a dependency bump.

## Synopsis

```
outrig build [--container <name>]
             [--config <path>]
             [--no-cache]
             [--all]
```

| Flag                        | Default             | Notes                                                              |
|-----------------------------|---------------------|--------------------------------------------------------------------|
| `--container <name>` | `default-container` | Build a specific named container-config.                           |
| `--all`                     | off                 | Build every container-config defined in the config file.           |
| `--config <path>`           | walks up from cwd   | Use a non-default config path.                                     |
| `--no-cache`                | off                 | Force rebuild even on cache hit. Passes `--no-cache` to `buildah`. |

## What it does

1. Loads `.agents/outrig/config.toml`.
2. For each selected container-config:
   - Computes the cache key (blake3 over Dockerfile content + build-args + context content hash).
   - If a tag matching that key exists and `--no-cache` is not set, prints
     `image ready (cache hit)` and skips.
   - Otherwise runs:
     ```
     buildah build --tag outrig-cache:<hash> \
                   --file <dockerfile> \
                   [user build-args] \
                   <context>
     ```

## Examples

Build the default:

```sh
$ outrig build
[outrig] container-config: coding
[outrig] dockerfile:       .agents/outrig/containers/coding/Dockerfile
[outrig] context:          .agents/outrig/containers/coding
[outrig] cache key:        outrig-cache:8c2a4f7e91d6b5a3
[buildah] STEP 1/6: FROM docker.io/library/node:20-bookworm-slim
...
[outrig] image ready
```

Cache hit on the second run:

```sh
$ outrig build
[outrig] image ready (cache hit: outrig-cache:8c2a4f7e91d6b5a3)
```

Build a specific container-config:

```sh
$ outrig build --container planning
[outrig] container-config: planning
...
[outrig] image ready
```

Build every container-config in one go:

```sh
$ outrig build --all
[outrig] container-config: coding   -> outrig-cache:8c2a4f7e91d6b5a3 (cache hit)
[outrig] container-config: planning -> outrig-cache:b91e3a6d217f4c08 (built in 27s)
[outrig] all images ready
```

Force a rebuild without changing files:

```sh
$ outrig build --no-cache
```

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
