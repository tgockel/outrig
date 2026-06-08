# 0077 -- Remote `outrig image inspect` (no-pull, via labels)

## Context

Once a standalone image's config is carried as OCI labels (0072) and `outrig image
inspect <ref>` reads them locally (0075), the same labels can be read from a *remote*
ref **without pulling layers** -- the payoff that motivated moving from a baked file
to labels. A baked file could never support this: you cannot read a file out of a
remote image without pulling it.

Discovered while grooming the OCI-label migration. The local label read and the codec land
first; remote inspection adds a dependency and network surface that should be scoped and
reviewed on its own.

## Goal

Add remote, no-layer-pull inspection of standalone image labels to `outrig image inspect`.

## Deliverables

- Add a path to `image inspect` (a flag, e.g. `--remote`, or automatic fallback when a
  ref is not present locally) that reads labels from a registry without pulling.
- Mechanism: `skopeo inspect docker://<ref>` (its `Labels` map), or an OCI
  registry-API / `oci-distribution` client that fetches just the image config blob.
  Either way this is a **new dependency** and the first network-reaching read in the
  tool, so it warrants its own task (auth, registry config, error handling, and a
  feature/dependency decision).
- Reuse the 0072 label codec to parse the fetched labels into the same
  metadata + `[mcp]` view that local inspect prints.

## Acceptance

- `outrig image inspect` can report metadata and declared MCP servers from a remote image
  ref without pulling image layers.
- The remote path reuses the local inspect rendering and the 0072 label codec.
- Missing labels, registry/auth failures, and unsupported refs return clear errors.
- Local inspect behavior from 0075 is unchanged.

## Dependencies

- **Hard: 0075**. Remote inspection extends the local label inspect command and rendering.
