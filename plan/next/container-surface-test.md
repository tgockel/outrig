# A surface test for the runtime-core API

## Context

0093 settled that `container`, `image`, `mcp_proxy`, and `network` are supported public API, not
leaked internals -- a downstream crate drives them directly rather than going through the `Outrig`
facade. But the only test that exercises the library *as a downstream consumer would* is
`crates/outrig/tests/library_surface.rs`, and it covers just the facade: `Outrig`, `LaunchSpec`,
the spec types, and one label constant.

So the half of the surface that 0093 promoted to a contract has no test defining that contract.
The existing runtime tests (`container_lifecycle.rs`, `container_security.rs`,
`network_interceptor.rs`) do exercise these types, but they are written as internal tests of
behavior -- they would happily keep passing through a signature change that broke every external
caller. The proxy's dispatch tests are no longer even out-of-crate: 0095 sealed `BackingClient`
and moved them to `crates/outrig/src/mcp_proxy_dispatch_tests.rs`.

## Goal

A companion to `library_surface.rs` that pins the runtime-core entry points the same way, so a
breaking change to them fails a test rather than a downstream build.

## Sketch

- Model it on the real consumer's shape: acquire an image (`image::compute_tag` /
  `probe_cached` / `ensure_image`), start a `Container` from a `ContainerLaunchSpec`, exec a
  server over it, aggregate through `mcp_proxy::ProxyServer`, and tear down.
- Construct the spec types through the constructors 0094 shipped (`ContainerWorkspace::new`,
  `ContainerMount::new`, `ContainerCapabilities::new`) and by assigning `pub` fields on a
  `Default` base. Struct literals are no longer available to an external caller -- every one of
  these types is `#[non_exhaustive]` -- so the test must use the paths a consumer actually has.
- Build `ImageTag` through `ImageTag::new` and read it through `as_str` / `into_string`; 0095 made
  the tuple field private, so the `ImageTag(name)` form `cococlaw` used is gone.
- Drive `create_initialized` through `ContainerCreateOptions`, which is the one entry point whose
  shape 0095 changed.
- Gate behind `e2e` alongside `library_surface.rs`.

## Notes

Originally written to land before 0095. It did not, so the churn 0095 introduced is already in
the tree and this test gets written against the post-0095 shape -- which is the shape `0.2.0`
freezes, and so the better target anyway.

Related: `plan/done/0093-shrink-reachable-surface.md`,
`plan/done/0095-options-structs-and-sealing.md`.
