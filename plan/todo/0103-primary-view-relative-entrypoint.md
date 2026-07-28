# 0103 -- `view = "primary"` cannot run an image whose ENTRYPOINT is a bare program name

## Context

`outrig-enter` opens the payload program by literal path, before the setns, while the
sidecar's own rootfs is still at `/` (`container/enter/launcher.rs:182-186`). There is no
`PATH` search: it is `open(argv[0])`, not `execvp` semantics. Correspondingly,
`build_primary_view_argv`'s `graft_prefix` deliberately leaves relative elements bare
(`container/sidecar.rs:188-197`), on the grounds that they are "resolved through `PATH`" --
but nothing ever resolves them.

So an image declaring `ENTRYPOINT ["node", "/app/dist/index.js"]` dies at startup with:

```
outrig-enter: open node: No such file or directory (os error 2)
```

This is exactly `docker.io/mcp/filesystem:latest`, which is the image the quickstart
one-liner in `doc/concepts/mcp-servers.md` uses, and the image
`crates/outrig-cli/tests/primary_view_e2e.rs` runs. **That e2e fails on trunk today** --
verified against an unmodified `43dea081` worktree, so it predates 0096.

0096 fixed the *other* half of this: `build_primary_view_argv` was graft-prefixing the
payload program as well, while the launcher already re-prefixes it itself when handing the
path to the loader (`launcher.rs:302`), so an absolute ENTRYPOINT became
`<graft><graft>/...` and failed the same way. With that fixed, an image whose ENTRYPOINT is
an *absolute* path works end to end (`primary_view_sidecar_from_library_sees_the_primary_filesystem`
in `crates/outrig/tests/library_surface.rs` proves it, against a
`docker.io/mcp/filesystem:latest` derivative that restates the ENTRYPOINT absolutely).

The relative case is what remains, and it is what stands between this repo's own config and
using a published MCP image unmodified -- see `plan/todo/0104-dogfood-sidecar-mcp-config.md`.

## Goal

`fs = { image = "docker.io/mcp/filesystem:latest", view = "primary", args = ["/"] }` -- the
documented quickstart -- starts and serves, and `primary_view_e2e.rs` passes unmodified.

## Deliverables

- Resolve a relative `argv[0]` against `PATH` in the launcher, before the setns, trying each
  entry with `open` and keeping the first that succeeds. `PATH` comes from the launcher's own
  environment, which is the sidecar image's -- the right one, since the program lives in the
  sidecar's rootfs. Fall back to the current error, naming `PATH`, when nothing matches.
- The resolved absolute path then flows into the existing `ElfKind` classification and the
  `graft`-prefixed loader invocation unchanged, so only the lookup is new.
- Alternative worth weighing: resolve it host-side instead, in `build_primary_view_argv`, by
  reading the image's `Config.Env` `PATH` and probing the image. That keeps the launcher
  dependency-free but needs a container to probe, which is what the launcher exists to avoid.
  The launcher-side lookup is almost certainly right.
- Add a negative test: an image with a deliberately unresolvable relative ENTRYPOINT should
  fail with a message naming `PATH`, not a bare `open`.

## Acceptance

- `primary_view_e2e.rs` passes against the stock `docker.io/mcp/filesystem:latest` rather than
  a derivative that restates the ENTRYPOINT absolutely, and the derivative-based coverage in
  `crates/outrig/tests/library_surface.rs` keeps passing alongside it.
- An unresolvable relative ENTRYPOINT fails with a message that names `PATH` and the value it
  searched.
- An absolute ENTRYPOINT takes the same path it does today -- no lookup, no behavior change.

## Dependencies

- **0102**. Both edit `main()` in `crates/outrig/src/container/enter/launcher.rs`, and 0102
  moves the exec paths this task's resolved program flows into. Landing the privilege drop
  first keeps the two reviewable apart.

## Notes

Worth doing before `0.2.0` is cut: the quickstart in the published book does not currently
work, and `view = "primary"` is the feature it is selling.

Related: `plan/done/0089-outrig-enter-helper.md` (the launcher),
`plan/done/0090-primary-view-sidecars.md` (the graft-prefix rule and the e2e),
`plan/done/0096-library-sidecar-parity.md` (the double-graft half of the fix, and the
absolute-ENTRYPOINT e2e).
