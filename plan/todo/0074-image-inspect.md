# 0074 -- `outrig image inspect <ref>` (local, read-only)

## Context

With config now carried as OCI labels (0072/0073), users need a quick, read-only way to see
what a local image declares -- its metadata and its declared MCP servers -- without starting a
container or any MCP servers.

This is a true inspect: it reads image labels via `podman image inspect`, which is local-only
and never pulls. It does **not** start the declared MCP servers. Starting servers and calling
`tools/list` is a *test*, and that concept already lives in `outrig image build` (run by
default, skippable with `--no-test`); `inspect` does not duplicate it. (A future task may add a
standalone test verb and/or remote inspection -- see `plan/next/`.)

## Goal

Add `outrig image inspect <ref>` that prints a local image's declared config from its OCI
labels.

## Deliverables

- Add `outrig image inspect <ref>` under the `outrig image` command group.
- Read the image's labels via `read_image_labels` (`podman image inspect`); local-only, no
  pull -- matching `podman inspect` behavior for absent images.
- Print to stdout (the scriptable surface; progress/diagnostics go to stderr):
  - the queried ref,
  - optional metadata: description, version, tags (only when present),
  - the declared MCP server commands (from `org.outrig.mcp`), with no initialization.
- A clear error when the image is not present locally (and no pull is attempted).
- Factor the human-readable rendering as a pure function over the parsed label data so it can
  be unit-tested without podman.
- Document `image inspect` in `doc/usage/image.md` (synopsis, output example, failure modes;
  note it is read-only and that live testing belongs to `image build`).

## Acceptance

- `outrig image inspect <ref>` reports metadata and declared MCP servers from a local labeled
  image, without starting any MCP server.
- Inspecting a missing local image returns a clear error and does not pull.
- A pure-render unit test covers the static output (ref + metadata + declared commands) with no
  podman.
- An e2e test builds the scaffold (which stamps labels) and asserts `inspect` reports them.
- `cargo test`, e2e, `clippy`, `fmt`, and the doc-style audit pass.

## Dependencies

- **Hard: 0072**. Inspect reads the labels and uses the codec introduced there.
