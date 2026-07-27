# A surface test for the runtime-core API

## Context

0093 settled that `container`, `image`, `mcp_proxy`, and `network` are supported public API, not
leaked internals -- a downstream crate drives them directly rather than going through the `Outrig`
facade. But the only test that exercises the library *as a downstream consumer would* is
`crates/outrig/tests/library_surface.rs`, and it covers just the facade: `Outrig`, `LaunchSpec`,
the spec types, and one label constant.

So the half of the surface that 0093 promoted to a contract has no test defining that contract.
The existing runtime tests (`container_lifecycle.rs`, `container_security.rs`,
`network_interceptor.rs`, `mcp_proxy_dispatch.rs`) do exercise these types, but they are written
as internal tests of behavior -- they would happily keep passing through a signature change that
broke every external caller.

## Goal

A companion to `library_surface.rs` that pins the runtime-core entry points the same way, so a
breaking change to them fails a test rather than a downstream build.

## Sketch

- Model it on the real consumer's shape: acquire an image (`image::compute_tag` /
  `probe_cached` / `ensure_image`), start a `Container` from a `ContainerLaunchSpec`, exec a
  server over it, aggregate through `mcp_proxy::ProxyServer`, and tear down.
- Construct the spec types by struct literal, as an external caller must, so a new required field
  breaks the test.
- Build `ImageTag` through its public tuple field -- that is how `cococlaw` builds it, and 0095
  may want to replace it with a constructor, which should be a deliberate break.
- Gate behind `e2e` alongside `library_surface.rs`.

## Notes

Worth doing before 0095 changes any of these signatures, so the test is written against the
current shape and the churn shows up as a diff.

Related: `plan/done/0093-shrink-reachable-surface.md`, `plan/todo/0095-options-structs-and-sealing.md`.
