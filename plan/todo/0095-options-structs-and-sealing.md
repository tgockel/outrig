# 0095 -- Options structs, trait sealing, and `ImageTag` privatization

## Context

0094 insulates public types with `#[non_exhaustive]`, which is a pure addition -- the attribute
can go on at any time. This task collects the three hardening changes that are **themselves
breaking**, and therefore have to land inside `0.2.0` or not at all:

1. `Container::create_initialized` takes **7 positional parameters**
   (`crates/outrig/src/container/mod.rs:332-340`). The `0.2.0-rc.1 -> Unreleased` CHANGELOG
   already records adding the trailing `args` param as a documented break -- this is the clearest
   "positional param list that grows" on the surface, and it has already grown once.
2. `BackingClient` (`crates/outrig/src/mcp_proxy.rs:39`) is a `pub trait` in a `pub mod`, with a
   blanket `impl<T> for Arc<T>` and a production impl for `McpClient`. Its own doc says it
   abstracts "the surface the proxy depends on" -- meant for the crate plus test fakes, not
   downstream implementors. Adding a method breaks any external impl.
3. `ImageTag` has a public tuple field (`crates/outrig/src/image.rs:54`,
   `pub struct ImageTag(pub String)`). That lets consumers bypass any future validation and locks
   the `String` representation into the contract. The contrast is `ApiKeyRef`
   (`config/api_key.rs:31`), which correctly declares `pub struct ApiKeyRef(String)` -- private
   field, same shape, already sealed.

Converting positional params to an options struct, sealing a trait, and privatizing a field are
each a break. Doing them now is the whole point; doing any of them in `0.2.1` is a second break
for the same benefit.

**Items 1 and 2 are conditional on 0093.** If that task demotes `container` and `mcp_proxy` out
of the public surface, `create_initialized`'s param list and `BackingClient`'s implementability
stop being SemVer commitments and both items evaporate -- an even better outcome than fixing
them. Check 0093's decision before starting; this task may reduce to item 3 alone.

## Goal

Land the three breaking API-shape corrections before `0.2.0` freezes, so each stops being a
recurring source of breaks.

## Deliverables

### `Container::create_initialized` -> options struct

Replace the 7 positional params (`image`, `launch`, `name`, `transcript`, `env`, `intercept_dns`,
`args`) with a single options struct. The struct must itself be `#[non_exhaustive]` with a builder
or `Default`-plus-update construction, or the problem has only moved: adding a field to a
`pub`-field options struct is exactly the same break as adding a positional param.

Skip entirely if 0093 made `container` crate-internal.

### Seal `BackingClient`

Give it a sealed supertrait (`trait BackingClient: sealed::Sealed { ... }`) so only the crate can
implement it, and it can then gain methods freely.

**Verified friction:** `crates/outrig/tests/mcp_proxy_dispatch.rs` defines `FakeClient` and
implements `BackingClient` for it. Integration tests are separate crates, so sealing breaks that
fake. Three ways out, in order of preference:

- 0093 de-publishes `mcp_proxy` entirely, and the trait needs no sealing at all.
- Export the sealed supertrait as `#[doc(hidden)] pub` so the in-repo test can still implement it
  while downstream crates are warned off.
- Move the fake into a `#[cfg(test)]` unit-test module inside the crate.

Decide which before writing the seal, since the third option means relocating a test rather than
annotating a trait.

### Privatize `ImageTag`'s field

Change `pub struct ImageTag(pub String)` to a private field and add the accessors that replace
`.0`: `new`, `as_str`, and `From<String>`. `Display` already exists and stays. Update every
in-repo `.0` use site.

## Acceptance

- `cargo test`, `cargo clippy --all-targets -- -D warnings`, and `cargo fmt --check` pass in the
  `default`, `local-llm`, and e2e-compile configurations.
- `crates/outrig/tests/library_surface.rs` compiles and passes.
- `crates/outrig/tests/mcp_proxy_dispatch.rs` still exercises the proxy's dispatch behavior --
  by whichever of the three routes above was chosen, and the choice is recorded in the task's
  Decisions section.
- No `.0` access to `ImageTag` remains anywhere in either crate.
- Any item skipped because 0093 removed its reachability is recorded as skipped, with the reason
  -- not silently dropped.
- The changelog records all three as breaking changes landed within `0.2.0`.

## Friction and caveats

- **The options struct is a break either way.** There is no non-breaking migration path for a
  7-param function that downstream code calls, which is exactly why it has to happen before the
  freeze rather than after.
- **Sealing changes who may implement, not who may call.** Downstream code that *uses*
  `ProxyServer` is unaffected; only an external `impl BackingClient` breaks. The only known such
  impl is the in-repo test fake.
- **Privatizing `ImageTag.0` removes `.0` access** for any downstream consumer, and there is no
  deprecation window inside a single release. `Display` already covers the common read path, so
  the practical blast radius is small.
- **Do not touch the already-correct types.** `ApiKeyRef` is the model this task copies, not a
  target; `ProxyServer`, `Container`, and `NetworkInterceptor` are already opaque. 0094 carries
  the full do-not-touch list.

## Dependencies

- **0093.** Determines whether items 1 and 2 exist at all.
- **0094.** The options struct introduced here must follow the same `#[non_exhaustive]`
  conventions the sweep establishes, rather than inventing a second style.

## See also

- `crates/outrig/src/container/mod.rs` -- `create_initialized` and its 7 params.
- `crates/outrig/src/mcp_proxy.rs` -- `BackingClient`, its blanket `Arc<T>` impl, and the
  already-opaque `ProxyServer`.
- `crates/outrig/src/image.rs` -- `ImageTag`.
- `crates/outrig/src/config/api_key.rs` -- `ApiKeyRef`, the correctly-sealed newtype to mirror.
- `crates/outrig/tests/mcp_proxy_dispatch.rs` -- the out-of-crate `FakeClient` impl that sealing
  has to account for.
