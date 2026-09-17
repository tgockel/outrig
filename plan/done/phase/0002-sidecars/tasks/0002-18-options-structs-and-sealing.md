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

## Decisions

1. **Nothing was skipped: 0093 left all three items live.** The task's opening caveat made items
   1 and 2 conditional on 0093 demoting `container` and `mcp_proxy`. It did the opposite --
   0093's Decision 1 records a live downstream consumer and ratifies all six `pub mod`s as
   supported API, and its Decision 4 names `create_initialized` as still needing this treatment.
   So all three landed, and the acceptance list's "record what was skipped" has nothing to
   record.

2. **All seven parameters moved into `ContainerCreateOptions`; the function takes one argument.**
   The alternative -- keeping `image` and `launch` positional for parity with `start_named` --
   would have left two of three parameters able to grow, which is the problem the task exists to
   end. Construction follows 0094's convention exactly: `new(image, launch, name)` names the
   required three, and `with_transcript` / `with_env` / `with_intercept_dns` / `with_args`
   supply the rest. The struct is `#[non_exhaustive]`, so the next knob is an addition.

3. **`start_named` and `attach` keep their positional parameters.** Neither has grown, and a
   shared options type would be actively wrong: `podman run` accepts none of `env`,
   `intercept_dns`, or `args`, so folding both constructors into one struct would mean fields
   that are silently ignored on one path. If `start_named` ever grows a parameter, it wants its
   own struct, not this one.

4. **`BackingClient` is sealed with a private `sealed::Sealed` supertrait, and the dispatch test
   moved into the crate.** Of the task's three routes, the middle one -- `#[doc(hidden)] pub` on
   the sealed module -- was rejected: it advertises the seal without enforcing it, so a
   downstream `impl` still compiles and the trait still cannot gain a method safely. Relocating
   `tests/mcp_proxy_dispatch.rs` to `src/mcp_proxy_dispatch_tests.rs` gives a real seal, and it
   was mechanical: the test touched no private items, so it needed only its `outrig::` paths
   rewritten to `crate::` / `super::` plus one `impl sealed::Sealed for FakeClient`. The wiring
   (`#[cfg(test)] #[path = "..."] mod ...`) is the shape `image.rs` already uses for
   `image_cache_tests.rs`, and 16 of the crate's 28 source files already carry in-crate tests.
   All 10 tests run under a plain `cargo test`.

5. **`ProxyServer::list_tools_inner` and `dispatch_call` stay public.** Both documented
   themselves as exposed for the external test, which the move dissolves -- but 0093 recorded
   that the five removed label helpers are "the only reachability `0.2.0` removes", and these
   two are the `RequestContext`-free half of the dispatch path, exactly what a caller driving
   the proxy outside an rmcp server needs. Their doc comments were rewritten to justify them by
   that use rather than by a test file.

6. **`ImageTag` gained `into_string` beyond the task's `new` / `as_str` / `From<String>` list.**
   Three sites move the `String` out rather than borrowing it (`outrig_.rs` twice, returning
   `Result<String>`, and one CLI test helper); without an owning accessor each would have become
   an `as_str().to_string()` allocation to work around a private field. `ApiKeyRef` needs no
   equivalent because nothing consumes it by value.

7. **The API snapshot lost 87 lines that were never a surface change.**
   `crates/outrig/public-api.txt` had the whole `#[non_exhaustive]` block committed twice --
   once as a leading block, once in its sorted position -- from 0094. Regenerating with the
   pinned `cargo-public-api` 0.52.0 dropped the duplicate. The real diff is this task's three
   items and nothing else.

8. **The private command builder took the options struct too, rather than keeping the shape the
   task set out to remove.** `build_podman_create_cmd` had the same seven positional parameters
   `create_initialized` did, so converting only the public function would have moved the churn one
   level down: the next create-path knob would still mean editing a seven-argument call and its
   every test. It now takes `(&ContainerCreateOptions, selinux)`, `create_initialized` is a
   straight forward with no destructure, and the seven `build_podman_create_cmd` unit tests
   construct options instead of argument lists -- so they cover the options plumbing as a side
   effect.

9. **`with_transcript` takes `Option<Transcript>`, not `Transcript`.** The bare-value form matched
   the `with_workspace`-style convention 0094 established, but every producer of a transcript has
   an `Option` -- `Container::start_named`'s own parameter is `Option<Transcript>` -- so the bare
   form forced the single call site to unwrap an `Option` purely to let the setter rewrap it. The
   convention exists to make call sites read well, and here it did the opposite.

10. **`From<String>` stays despite being reachable through `ImageTag::new`.** Two review passes
    flagged it as redundant with `new(impl Into<String>)`, and it is -- but the task names it as a
    deliverable, and a `From` impl is what makes `ImageTag` usable in generic `Into` position.
    Recorded as a deliberate keep rather than an oversight.

11. **`ContainerCreateOptions` keeps both `pub` fields and `with_*` setters.** Flagged as two ways
    to do one thing, and `ContainerLaunchSpec` in the same module offers only the field-assignment
    form. Kept anyway: that is exactly the shape 0094 shipped for `LaunchSpec` and `SidecarSpec`,
    the CHANGELOG documents the `with_*` path as the migration, and `#[non_exhaustive]` means the
    fields alone cannot construct the value from outside.

12. **One follow-up filed, one corroborated.** `plan/next/public-api-snapshot-gate.md` is new:
    nothing enforces the surface snapshots, which is why the duplicate block in Decision 7 went
    unnoticed for a whole task. The e2e failure hit during verification
    (`clean_sweeps_stopped_recordless_labeled_containers`) turned out to be the already-filed
    `plan/next/clean-batch-removal-fidelity.md`, so that entry got the new observation rather
    than a duplicate file -- notably that the failure is self-perpetuating, and that it
    reproduces on unmodified trunk with this task's changes stashed.
