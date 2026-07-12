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
