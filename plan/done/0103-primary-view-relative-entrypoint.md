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

## Decisions

- **Launcher-side, as the deliverables predicted.** Host-side resolution would have to read
  the image's `Config.Env` *and* probe the image's filesystem for each candidate, which needs
  a container -- the thing the launcher exists to avoid. Launcher-side the search is four
  syscalls at worst, against the environment podman already handed the process.
- **Only absolute `PATH` entries produce candidates**, unlike `execvp`, which treats an empty
  or relative entry as the current directory. The `ElfKind::Dynamic` branch names the payload
  to the loader as `<graft><path>`, so a relative hit could not be addressed after the setns.
  No image puts its entrypoint in `.`, so nothing real is lost, and the alternative -- a
  candidate that opens pre-setns and then cannot be exec'd -- fails later and worse.
- **`PATH` unset or empty falls back to the OCI default**
  (`/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin`), which is what podman gives
  a container whose image declares none and what `execvp` reaches for via `confstr(_CS_PATH)`.
  The failure message prints the value walked either way, so a search of the fallback list is
  never mistaken for a search of the image's own.
- **`argv[0]` reaches the payload as written.** A name found on `PATH` is passed on as that
  name, not as the resolved path -- `execvp`'s behavior, and what a server that reports its
  own invocation should print.
- **The pure part lives in `path_search.rs`**, `include!`d by `launcher.rs` and compiled as
  `#[cfg(test)] mod path_search` by the library, exactly as `elf.rs` already is. Candidate
  generation is the part with edge cases (empty entries, a program that already names a
  path); the `open` loop around it has none. It builds on `std::env::split_paths` and
  `Path::join` rather than splitting bytes by hand -- the launcher forgoes crates, not `std`,
  and `network.rs`'s `require_tool` already resolves tools that way.
- **One predicate decides both the search and the message.** `searched_path` returns the
  `PATH` a name will be walked along, or `None` when the program names a path; the candidate
  list and the "not found in PATH=..." hint each go through it, so the value reported cannot
  drift from the value searched.
- **`build.rs` watches `src/container/enter/` as a directory** rather than naming each
  `include!`d file. Cargo rescans it recursively (verified: touching `path_search.rs` reruns
  the script), and the per-file form had the one failure mode that reports nothing -- an
  unlisted input leaves a stale launcher embedded while every test still passes.
- The negative test lives in `crates/outrig/tests/library_surface.rs` rather than
  `primary_view_e2e.rs`: `add_sidecar` returns the `McpStartupFailed` error directly, stderr
  tail included, so the assertion reads the launcher's message without scraping a log. It
  derives its sidecar from the local `mcp-fs` fixture image, not from
  `docker.io/mcp/filesystem`: the launcher gives up in the `open` loop before the namespace
  join, so the base image's payload is never reached and pulling it would be waste.
  `build_absolute_entrypoint_mcp_fs_image` became `build_image_with_entrypoint`, taking both
  the base and the `ENTRYPOINT` now that two tests want different shapes.

## Notes

Worth doing before `0.2.0` is cut: the quickstart in the published book does not currently
work, and `view = "primary"` is the feature it is selling.

Related: `plan/done/0089-outrig-enter-helper.md` (the launcher),
`plan/done/0090-primary-view-sidecars.md` (the graft-prefix rule and the e2e),
`plan/done/0096-library-sidecar-parity.md` (the double-graft half of the fix, and the
absolute-ENTRYPOINT e2e).
