# 0085 -- from_image_config sidecar translation

## Goal

`LaunchSpec::from_image_config` copies `[images.<name>.mcp]` wholesale, so placement-bearing
entries (`sidecar = "<sc>"`, inline `image = "..."`) survive into the spec and
`Outrig::launch` rejects them with a pointer at `LaunchSpec::with_sidecar` /
`Outrig::add_sidecar` (decision recorded in `plan/done/0081-sidecar-dynamic-add.md`).

Make the translation faithful instead: `from_image_config` (or a sibling constructor taking
the whole `Config`) turns `[sidecars.*]` blocks and placement-bearing MCP entries into
`SidecarSpec`s on the returned `LaunchSpec`.

## Deliverables

A faithful translation, with these scope notes:

- Named-sidecar image refs resolve like `--image` (sibling `[images.<name>]` block first,
  else raw ref), which needs the whole `Config` plus `ensure_image` -- today's
  `from_image_config` only sees one `ImageConfig`.
- Sidecar `org.outrig.mcp` label merges (scoped, config-overrides-by-name, collision
  errors) would need to happen at launch, mirroring `setup_sidecars_and_network`.
- Entrypoint-stdio placements (`image`, no `command`) have no `SidecarSpec` form; either
  grow one or keep rejecting those specifically.
- `start = "manual"` maps to *not* adding the sidecar at launch; decide whether the library
  should expose the declared-but-unstarted set so callers can add them later by name.

## Acceptance

- Placement-bearing `[mcp]` entries and `[sidecars.*]` blocks in an image config translate
  into `SidecarSpec`s on the returned `LaunchSpec` instead of being rejected at launch.
- `start = "manual"` sidecars are not started at launch.
- Entrypoint-stdio placements either translate or fail with a specific, documented error.

## Dependencies

None (follows up on completed task 0081).

## Decisions

User-confirmed during planning (2026-07-13):

1. **`from_image_config` replaced by an async `from_config(&Config, image_name, repo_root,
   log_dir)`.** Faithful named-sidecar image resolution needs the whole `Config` (to find sibling
   `[images.<name>]` blocks) plus async `ensure_image`, which the old `&ImageConfig`-only, sync,
   infallible constructor could not do. `from_image_config` had no callers in `src/` or `tests/`,
   so removal was free. The primary image is config-name-only (raw primary refs remain
   `LaunchSpec::from_image`'s job).
2. **Sidecar images are resolved eagerly in the constructor**, mirroring the CLI's
   `ensure_sidecar_image` (sibling `[images.<name>]` -> `compute_tag_for` + `ensure_tagged_image_for`;
   else a raw local ref via `ensure_local_image`, no pull). The resolved tag is stored verbatim in
   `SidecarSpec.image`, preserving 0081 decision 1 (the library `SidecarSpec` image is a raw ref).
   The primary image stays lazy (built at `launch`) -- an accepted asymmetry, since `LaunchSource`
   already carries an unbuilt Build/Image source while `SidecarSpec` does not.
3. **Entrypoint-stdio placements are rejected** with a specific `Configuration` error naming the
   server. The facade is exec-stdio only (`SidecarServerSpec.command` is non-optional); growing an
   entrypoint-stdio form would duplicate the CLI's create->init->attach->start machinery for a
   surface with no consumers. Run those via the CLI. (Acceptance allows "translate or fail".)
4. **`start = "manual"` sidecars are skipped, not exposed.** They are dropped from the launch set;
   their sidecar-placed servers therefore vanish (no library stderr notice). A caller who wants one
   later rebuilds a `SidecarSpec` and calls `Outrig::add_sidecar`.
5. **Sidecar-image `org.outrig.mcp` label merges are deferred** (documented library limitation).
   Sidecar servers come only from the repo config's `sidecar =` / anonymous `image =` entries; a
   sidecar image's own embedded MCP label is not read. The CLI path remains the strict superset.
6. **Translation split into a pure `plan_to_launch_parts` + async `resolve_sidecar_image_tag`.**
   `plan_to_launch_parts` (podman-free) reuses `sidecar::plan_from_config` for placement
   classification and does the mcp/sidecar partition, manual-skip, and entrypoint-reject, leaving
   each `SidecarSpec.image` an unresolved ref; `from_config` then rewrites those via the async
   resolver. The split keeps the decision logic unit-testable without podman.
