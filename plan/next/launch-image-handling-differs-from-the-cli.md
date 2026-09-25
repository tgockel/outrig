# `Outrig::launch` never pulls, and builds under a tag the CLI never uses

## Context

Found while `0003-05` put `outrig run-new` on the facade.

- **No pull.** `LaunchSpec::from_config` lowers an `image-name` primary to
  `LaunchSource::Image`, and `launch` runs it with `--pull=never`. Nothing pulls it, so a
  library consumer launching an image that is not already local gets podman's "image not known".
  The CLI's path pulls through `image::ensure_tagged_image_for`. The library's own e2e tests pull
  alpine first for the same reason.
- **A second cache entry, under a different key.** For a Dockerfile image-config, `launch`
  builds through the nameless `image::ensure_image`, which tags under `outrig-cache`, where
  `outrig run` and `outrig build` tag under the image-config's name. Its key is also taken over a
  synthetic `ImageConfig` with no `mcp` table, so it differs from the key of the config it was
  lowered from whenever that declares servers. The same Dockerfile is built again, and a caller
  that ensured the image itself first does not get the image it ensured.

`run-new` avoids both: it ensures under the image-config's name, as `run` does, and lowers a copy
of the config whose image-config names that tag, so `launch` has nothing to pull or build. A
library consumer has no such route short of doing the same. That copy keeps only the
image-config's `security`, which is a caller's list of the fields `from_config` reads, over a
`#[non_exhaustive]` type -- `outrig_.rs` warns against exactly that kind of mapping.

## Shape

`from_config` lowering the primary as it already lowers each sidecar -- ensure it under its
image-config name (`resolve_sidecar_image_tag`), then hand `launch` a `LaunchSource::Image` --
fixes both for every caller, and retires `run-new`'s own ensure and its pinned copy. `LaunchSource`
is crate-private, so the surface does not move; the behavior of `from_config` does.

## Acceptance

- `Outrig::launch` of a `from_config` spec naming an absent `image-name` pulls it, in a live
  test that removes the image first.
- `run` and `run-new` over one Dockerfile image-config share one build.
