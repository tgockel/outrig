# Remote `outrig image inspect` (no-pull, via labels)

Once a standalone image's config is carried as OCI labels (0072) and `outrig image
inspect <ref>` reads them locally (0074), the same labels can be read from a *remote*
ref **without pulling layers** -- the payoff that motivated moving from a baked file
to labels. A baked file could never support this: you cannot read a file out of a
remote image without pulling it.

## Sketch

- Add a path to `image inspect` (a flag, e.g. `--remote`, or automatic fallback when a
  ref is not present locally) that reads labels from a registry without pulling.
- Mechanism: `skopeo inspect docker://<ref>` (its `Labels` map), or an OCI
  registry-API / `oci-distribution` client that fetches just the image config blob.
  Either way this is a **new dependency** and the first network-reaching read in the
  tool, so it warrants its own task (auth, registry config, error handling, and a
  feature/dependency decision).
- Reuse the 0072 label codec to parse the fetched labels into the same
  metadata + `[mcp]` view that local inspect prints.

## Why deferred

Discovered while grooming the OCI-label migration (0072-0075). The local label read
and the codec land first; remote inspection adds a dependency and network surface that
should be scoped and reviewed on its own. Fold into the numbered queue via
`/groom-plan` once 0072/0074 are done.
