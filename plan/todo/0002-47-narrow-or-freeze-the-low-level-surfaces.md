# 0124 -- Narrow or explicitly freeze the rmcp-coupled and low-level surfaces

## Context

0093 settled that `container`, `image`, `mcp_proxy`, and `network` are supported public API
rather than leaked internals, because a downstream crate drives them directly instead of going
through the `Outrig` facade. 0094 and 0095 then hardened them with `#[non_exhaustive]`,
constructors, options structs, and sealing.

Three surfaces came through that arc without an explicit verdict, and 0.2.0 is where a verdict
becomes binding. None is a proven current failure; all three are shapes that make a future
change more expensive than it needs to be.

**rmcp reaches the public surface in three places, not one.** `crates/outrig/public-api.txt`
carries sixteen rmcp-typed lines:

- `ProxyServer`'s inherent methods and its `ServerHandler` impl -- `dispatch_call`,
  `list_tools_inner`, `call_tool`, `get_info`, the three `list_*` methods, and
  `supported_protocol_versions` (`public-api.txt:900-911`);
- **`OutrigError` variants and a `From` impl** -- `McpServerInitialize(Box<ServerInitializeError>)`,
  `McpService(ServiceError)`, `McpToolsListFailed::source`, and
  `From<ServerInitializeError> for OutrigError` (`public-api.txt:833-854`). These are reachable
  from every fallible call in the crate, not only from the proxy;
- **`SUPPORTED_PROTOCOL_VERSIONS: &[rmcp::model::ProtocolVersion]`** (`public-api.txt:914`), which
  is a public constant whose *type* is an rmcp type.

So an rmcp major upgrade is an OutRig public-API event on all three, and the neutral content
types 0120 may introduce decouple none of them -- 0120 is about `McpTool`/`McpToolResult`, which
are already outrig-owned. This is not speculative: the rmcp 1.x -> 3.x break is the
demonstration, and `plan/next/rmcp-list-result-spec-gaps.md` documents how the 2.2 -> 3.1 bump
moved outrig onto a protocol revision it did not satisfy, with no outrig source change.

**`container::enter` exposes exactly two functions.** The public surface is
`enter::is_available()` and `enter::materialize(&Path)` (`public-api.txt:664-666`); the launcher
and parser modules are already private, and `crates/outrig/build.rs` compiling
`src/container/enter/launcher.rs` is a build-time detail rather than public plumbing. This task's
earlier framing overstated it. The real question is narrower and worth asking anyway: does an
embedder need either function, and do they need the same answer?

**`IoPathExt` is an unsealed public extension trait.** It lives in `crates/outrig/src/error.rs`
and is used from five modules (`process.rs`, `mcp.rs`, `network.rs`, `image.rs`,
`container/enter/mod.rs`). Unsealed means an external type may implement it, which means adding a
required method later breaks those implementors -- the exact hazard 0095 sealed `BackingClient`
against.

## Goal

Each of the three has a written verdict -- frozen as supported 0.2.x surface, or narrowed now --
and the verdict is somewhere a consumer will read it.

## Deliverables

- **A `## Decisions` section carrying all three verdicts**, in the format `plan/done/` uses, so
  the reasoning is recoverable later. This is the primary deliverable; the code change is
  whatever the verdicts imply.
- **`crates/outrig/CHANGELOG.md` states the rmcp coupling either way.** If frozen: say that an
  rmcp major is an outrig major, so consumers can plan. If narrowed: say what replaced the rmcp
  types in the signatures. Silence here is the one outcome that is wrong, because a consumer
  cannot discover the coupling without reading the trait impls.
- **A complete rmcp inventory**, taken from `public-api.txt` rather than from memory, so the
  verdict covers the error variants and the version constant and not only the proxy. If the
  verdict is narrow, outrig-owned error and protocol-version types are part of the work; if it is
  freeze, **every** exposed path is named, including the `From` impl.
- **Whatever narrowing is chosen, applied.** Sealing `IoPathExt` is a one-line private
  supertrait and costs nothing. `is_available` and `materialize` get independent verdicts. The
  rmcp verdict is the expensive one and may reasonably be "freeze".
- `crates/outrig/public-api.txt` regenerated.

## Acceptance

- Every rmcp-typed line in `public-api.txt` is accounted for by a verdict -- diff the inventory
  against the file, so "we forgot `OutrigError`" is not possible.
- `is_available` and `materialize` each have their own verdict, and `IoPathExt` has one; none is
  left implicit.
- If `IoPathExt` is sealed: a compile-fail test that implements it for a type **defined in the
  test crate**, so the failure cannot come from Rust's orphan rules instead of the seal, and that
  asserts the diagnostic names the inaccessible sealing supertrait. Pair it with a positive test
  that the supported `Result<T, io::Error>` receiver still compiles -- a seal that also breaks
  ordinary use is not a seal, it is a regression.
- If anything is narrowed: `crates/outrig/tests/library_surface.rs` still compiles, and the
  companion runtime-core surface test (`plan/next/container-surface-test.md`) covers what remains
  supported.
- No public item is left in an "accidentally public" state -- if it survives, it survives because
  a verdict says so.

## Design forks

1. **The rmcp coupling -- Decided in 0120, applied here.** 0120's fork 2 records the boundary
   principle, because it is the first task that has to commit to one and the queue cannot run two
   tasks jointly. This task inherits that answer and applies it to what 0120 does not touch: the
   three `OutrigError` variants, the `From` impl, and `SUPPORTED_PROTOCOL_VERSIONS`. If applying
   it here shows the principle was wrong, that is a reopened decision recorded in both places --
   not a second, quieter answer.

2. **`is_available` versus `materialize` -- Open, and they are separable.** `is_available` is a
   cheap predicate with no ownership implications and is plausibly useful to an embedder.
   `materialize` writes a helper binary to a path and is the one that constrains future
   implementation. Decide them separately rather than treating `enter` as one surface; 0093's
   method was to ask what a real downstream crate drives.

3. **Whether sealing `IoPathExt` is worth a break -- Recommended: yes, and it is barely a break.**
   Nothing outside this crate plausibly implements an internal IO-error-context helper. Sealing
   costs one private supertrait and removes a whole category of future breakage.

## Dependencies

- **Hard: 0120**, which records the rmcp boundary decision this task applies.

Must precede 0125's snapshot regeneration and 0126's migration guide.

## See also

- `crates/outrig/src/mcp_proxy.rs` -- `ProxyServer` and its rmcp trait impls.
- `crates/outrig/src/error.rs` -- `IoPathExt`; `crates/outrig/src/container/enter/` and
  `crates/outrig/build.rs` -- the launcher plumbing.
- `plan/done/0093-shrink-reachable-surface.md`, `plan/done/0095-options-structs-and-sealing.md`
  -- the sealing precedent and the method for deciding what is supported.
- `plan/next/rmcp-list-result-spec-gaps.md` -- concrete evidence of what an rmcp bump costs.
