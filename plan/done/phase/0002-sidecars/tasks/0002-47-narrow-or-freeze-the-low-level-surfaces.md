# 0002-47 -- Narrow or explicitly freeze the rmcp-coupled and low-level surfaces

## Context

0002-16 settled that `container`, `image`, `mcp_proxy`, and `network` are supported public API
rather than leaked internals, because a downstream crate drives them directly instead of going
through the `Outrig` facade. 0002-17 and 0002-18 then hardened them with `#[non_exhaustive]`,
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

So an rmcp major upgrade is an OutRig public-API event on all three, and the neutral content types
0002-43 may introduce decouple none of them -- 0002-43 is about `McpTool`/`McpToolResult`, which are
already outrig-owned. This is not speculative: the rmcp 1.x -> 3.x break is the demonstration, and
`plan/next/rmcp-list-result-spec-gaps.md` documents how the 2.2 -> 3.1 bump moved outrig onto a
protocol revision it did not satisfy, with no outrig source change.

**`container::enter` exposes exactly two functions.** The public surface is
`enter::is_available()` and `enter::materialize(&Path)` (`public-api.txt:664-666`); the launcher
and parser modules are already private, and `crates/outrig/build.rs` compiling
`src/container/enter/launcher.rs` is a build-time detail rather than public plumbing. This task's
earlier framing overstated it. The real question is narrower and worth asking anyway: does an
embedder need either function, and do they need the same answer?

**`IoPathExt` is an unsealed public extension trait.** It lives in `crates/outrig/src/error.rs`
and is used from five modules (`process.rs`, `mcp.rs`, `network.rs`, `image.rs`,
`container/enter/mod.rs`). Unsealed means an external type may implement it, which means adding a
required method later breaks those implementors -- the exact hazard 0002-18 sealed `BackingClient`
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

1. **The rmcp coupling -- Decided in 0002-43, applied here.** 0002-43's fork 2 records the boundary
   principle, because it is the first task that has to commit to one and the queue cannot run two
   tasks jointly. This task inherits that answer and applies it to what 0002-43 does not touch: the
   three `OutrigError` variants, the `From` impl, and `SUPPORTED_PROTOCOL_VERSIONS`. If applying
   it here shows the principle was wrong, that is a reopened decision recorded in both places --
   not a second, quieter answer.

2. **`is_available` versus `materialize` -- Open, and they are separable.** `is_available` is a
   cheap predicate with no ownership implications and is plausibly useful to an embedder.
   `materialize` writes a helper binary to a path and is the one that constrains future
   implementation. Decide them separately rather than treating `enter` as one surface; 0002-16's
   method was to ask what a real downstream crate drives.

3. **Whether sealing `IoPathExt` is worth a break -- Recommended: yes, and it is barely a break.**
   Nothing outside this crate plausibly implements an internal IO-error-context helper. Sealing
   costs one private supertrait and removes a whole category of future breakage.

## Dependencies

- **Hard: 0002-43**, which records the rmcp boundary decision this task applies.

Must precede 0002-48's snapshot regeneration and 0002-49's migration guide.

## See also

- `crates/outrig/src/mcp_proxy.rs` -- `ProxyServer` and its rmcp trait impls.
- `crates/outrig/src/error.rs` -- `IoPathExt`; `crates/outrig/src/container/enter/` and
  `crates/outrig/build.rs` -- the launcher plumbing.
- `plan/done/phase/0002-sidecars/tasks/0002-16-shrink-reachable-surface.md`,
  `plan/done/phase/0002-sidecars/tasks/0002-18-options-structs-and-sealing.md` -- the sealing
  precedent and the method for deciding what is supported.
- `plan/next/rmcp-list-result-spec-gaps.md` -- concrete evidence of what an rmcp bump costs.

## Decisions

1. **The inventory, line by line, with a verdict on each.** Taken from
   `crates/outrig/public-api.txt` as it stood at `6e6289e` rather than from memory, which is
   what makes forgetting the `OutrigError` half impossible. Sixteen rmcp-typed lines; the
   context's own citations (`833-854`, `900-911`, `914`) were approximate, and these are the
   grep:

   | Line(s) | Item                                                                | Verdict  |
   | ------- | ------------------------------------------------------------------- | -------- |
   | 843     | `OutrigError::McpServerInitialize(Box<ServerInitializeError>)`      | Removed  |
   | 877-878 | `impl From<ServerInitializeError> for OutrigError`, and its `from`  | Removed  |
   | 844     | `OutrigError::McpService(ServiceError)`                             | Narrowed |
   | 848     | `OutrigError::McpToolsListFailed::source: Box<ServiceError>`        | Narrowed |
   | 940     | `ProxyServer::dispatch_call`                                        | Frozen   |
   | 942     | `ProxyServer::list_tools_inner`                                     | Frozen   |
   | 944     | `impl ServerHandler for ProxyServer<C>`                             | Frozen   |
   | 945     | `ProxyServer::call_tool`                                            | Frozen   |
   | 946     | `ProxyServer::get_info`                                             | Frozen   |
   | 947-950 | `list_prompts, list_resource_templates, list_resources, list_tools` | Frozen   |
   | 951     | `ProxyServer::supported_protocol_versions`                          | Frozen   |
   | 954     | `SUPPORTED_PROTOCOL_VERSIONS: &[rmcp::model::ProtocolVersion]`      | Frozen   |

   Eleven frozen, every one under `outrig::mcp_proxy`; five narrowed or removed, every one
   under `outrig::error`. The regenerated snapshot's diff is exactly those five lines going,
   their replacements arriving, and the seal -- no unrelated churn, so the diff is readable as
   the verdict itself.

   `McpStartupFailure::source` is named here although it is not in the table. It is typed
   `Box<dyn Error + Send + Sync>` and holds an rmcp `ClientInitializeError` in practice:
   rmcp-coupled in fact, rmcp-free in the signature, so an rmcp major is not a compile break
   against it. That is why it is out of the *table*, and it is not by itself why it is left
   alone -- decision 3 below argues against erasure, and this is erasure. It is left alone
   because `McpStartupFailure` is a struct already carrying `command`, `exit`, `stderr_path`,
   and `stderr_tail`: for a server that would not start, *those* are the diagnostic, and
   classifying the handshake error underneath adds little the exit status and captured stderr do
   not already say. A defensible asymmetry rather than a comfortable one, so it is written down
   twice -- the CHANGELOG warns a consumer that downcasting the field couples them to an rmcp
   major, and `plan/next/mcp-startup-failure-erases-its-cause.md` carries the fix.

2. **Fork 1 -- inherited from 0002-43, and applying it did not reopen it.** The principle gave
   a different answer for the proxy than for the errors, which was the point of checking it
   against both before committing. Nothing in the application argued the other way: the proxy's
   rmcp types are all arguments to or results from rmcp's own dispatch, and the error variants
   are all values a caller of `McpClient` receives without ever naming rmcp.

3. **The narrowed payload is a classified outrig type, not an erased `Box<dyn Error>`.** The
   cheaper option was there and has a precedent one screen up in the same file --
   `McpStartupFailure::source` already erases an rmcp error -- and it was rejected. Erasure
   leaves a caller with text to parse, and text is what the reduction 0002-43 undid was made
   of. So `McpSessionError` carries an `McpFailureKind` beside the SDK's own rendering: a
   coarse, five-way classification outrig commits to, and the wording verbatim for a human.

   Two consequences recorded on purpose:

   - **The mapping needs a wildcard and the wildcard is a silent path.**
     `rmcp::service::ServiceError` is `#[non_exhaustive]`, so a ninth variant becomes `Other`
     with no compile error. `every_rmcp_service_error_is_classified` names all eight that rmcp
     3.1.0 declares -- naming them *is* the record of what was classified deliberately -- and
     `plan/next/rmcp-list-result-spec-gaps.md` now carries the classifier beside
     `SUPPORTED_PROTOCOL_VERSIONS` as a second thing to review on every rmcp bump.
   - **`McpSessionError` has no `source`.** `message` already is the underlying rendering, so a
     cause named beside it would print the same text twice in every walked chain -- the
     objection already written down for `NetworkTeardown`.

   The conversion is a free function, `session_error_from_rmcp`, matching `result_from_rmcp` and
   `tool_from_rmcp`, and for the reason 0002-43 gave: a public `From<rmcp type>` would put the
   SDK straight back on the surface the conversion exists to keep it off.

4. **`McpServerInitialize` is removed rather than narrowed, because of a constraint rather than
   a preference.** The library never constructs it. An initialize failure happens while
   *serving* MCP, and `outrig` does not serve -- its only two construction sites are
   `outrig-cli`'s `cli/mcp.rs` and `mcp_self/server.rs`, reached through a `From` impl that
   existed so their `?` would compile.

   Narrowing it would have needed a classifier for `ServerInitializeError` reachable from
   `outrig-cli`, which is a separate crate. That is either a public rmcp-typed function -- the
   exact line being deleted -- or six match arms duplicated in `outrig-cli` and free to drift.
   Neither is worth paying for a variant the library cannot return. `CliError` takes the
   variant and keeps the SDK's type, which costs nothing there: `outrig-cli` publishes only
   `run()`.

   A downstream crate driving `mcp_proxy::ProxyServer` calls rmcp's `serve_server` itself and
   already holds rmcp's error, so it never needed outrig's variant either. This is also the
   task's "accidentally public" criterion biting: a variant the library cannot produce is the
   clearest case of one.

5. **Fork 2 -- `is_available` and `materialize` are decided separately and both frozen, for
   different reasons.** 0002-16's method was to ask what a real downstream crate drives, and
   both answer yes:

   - **`is_available()` -- frozen, no caveat.** `outrig-cli/src/builtin_image/mod.rs:97` drives
     it to decide whether the built-in image can offer a filesystem view at all. A cheap,
     side-effect-free predicate with no ownership implications, and the exact question an
     embedder choosing between `SidecarView::Primary` and a bind mount has to answer.
   - **`materialize(&Path)` -- frozen, with a break recorded in advance.**
     `outrig-cli/src/cli/session_setup.rs:922` drives it, and `pub(crate)` is simply not
     available: `outrig-cli` is a separate crate and stages the helper before a session exists.
     No narrower door replaces it, so narrowing here would have meant opening a different
     public one. The caveat that the task suspected is real, though, and is written down rather
     than discovered later: `&Path` in and `PathBuf` out assumes the podman engine shares a
     filesystem with the caller, which `plan/next/primary-view-remote-podman.md` shows is false
     under remote podman. A remote-podman implementation breaks this signature. That is now a
     known cost with a named owner rather than a surprise.

   The task asked whether they need the same answer. They get the same answer and it is load
   bearing for neither: `is_available` survives because it is harmless and useful, `materialize`
   because there is nowhere cheaper to put it.

6. **Fork 3 -- `IoPathExt` is sealed, and the seal is generic over the trait's argument.**
   A private `mod sealed`, a supertrait bound, and an `impl` beside the single real impl, with
   `#[doc(hidden)] pub` not used -- 0002-18 rejected that for advertising a seal without
   enforcing one.

   Where it is *not* structurally identical to `mcp_proxy::BackingClient` is the part that
   matters. `BackingClient` has no type parameter, so a non-generic `Sealed` seals it.
   `IoPathExt<T>` does, and the first cut bounded `Self` alone -- which seals nothing, because
   the one receiver that satisfies `Sealed` is the supported one:

   ```rust
   impl outrig::error::IoPathExt<Local> for Result<(), std::io::Error> { ... }
   ```

   compiled outside the crate. `Result<(), io::Error>: Sealed` holds, a local type as the
   trait's *argument* satisfies the orphan rule, and it cannot overlap
   `impl<T> IoPathExt<T> for Result<T, io::Error>`, which requires the argument to be the `Ok`
   type. So a required method added later would still have broken a legal external impl --
   exactly the breakage the seal was for. `Sealed<T>`, implemented only for the pairing the real
   impl uses, closes it. `tests/ui/io_path_ext_seal_covers_the_type_argument.rs` is the
   regression, and it was written and watched to compile *before* the fix rather than after, so
   it is known to catch the hole rather than assumed to.

7. **`trybuild` earns a new dev-dependency, and the two earlier refusals do not carry over.**
   0002-42 declined a compile-fail test because the alternative -- a snapshot line -- said the
   same thing for less. That is not true here. `public-api.txt` records the supertrait bound,
   but a snapshot line cannot distinguish a seal from the orphan rules: an out-of-crate `impl`
   of a foreign trait for a foreign type is rejected either way, so a snapshot alone would let
   a seal that does not actually seal pass review. The case file defines its own type, which
   removes the orphan rules from the picture, and only the golden `.stderr` can show what is
   left. What it shows is worth having in the tree:

   ```
   error[E0277]: the trait bound `Local: outrig::error::sealed::Sealed` is not satisfied
   ...
   = note: `IoPathExt` is a "sealed trait", because to implement it you also need to
           implement `outrig::error::sealed::Sealed`, which is not accessible
   ```

   0002-18's own seal shipped untested and still is. The harness now exists, and a
   `BackingClient` case would be ~15 lines and a second golden, but writing it is 0002-18's
   territory rather than this task's verdict. The asymmetry is named here so it is a known gap
   rather than an inconsistency nobody noticed.

   Two costs. The golden is pinned to rustc's wording and there is no `rust-toolchain.toml`, so
   a release that rewords `E0277` fails this test and is fixed with `TRYBUILD=overwrite`; the
   regeneration command is in the test's own module doc.

   The second was understated when this was planned, and measuring it changed the shape.
   trybuild's scratch project sets its own `CARGO_TARGET_DIR` *and* passes an explicit
   `--target`, so it shares no artifact with `target/` -- not even host build scripts. The
   scratch tree is ~2 GB across 296 rlibs, it merges dev-dependencies so the graph is the dev
   one, and `build.rs` runs the musl launcher again inside it. Three of the four CI rows run a
   bare `cargo test`, so left ungated that is three full second builds per run, or three cache
   payloads roughly doubled. The target is therefore behind a `seal-tests` feature -- the shape
   `library_surface` already uses -- and `.github/workflows/ci.yml` runs it on the `default` row
   alone. The arm64 row would prove nothing extra, a seal being a property of the type system
   rather than the target, and the e2e row's `--no-run` never executes it anyway.

   Three cases, not one: an impl for a local *type* (`Local`), an impl for a supported receiver
   with a local *argument* (decision 6's hole), and a `pass/` case running `path_ctx` on
   `Result<T, io::Error>` from outside the crate. The last is not decoration -- a seal that also
   breaks ordinary use is a regression, not a seal -- and the middle one is the reason a golden
   `.stderr` is worth its maintenance, since both rejections are `E0277` and only the text says
   which bound did it.

8. **`public_api_boundary.rs` asserts the boundary; 0002-48 asserts the snapshot -- and the
   dependence runs one way.** 0002-48 says "this file is current"; this says "this file obeys
   the rule", mechanically. What this catches that a byte-exact diff cannot is a *deliberate*
   regeneration carrying an SDK type back into `outrig::error`, which a diff would accept as the
   new truth. What it cannot catch until 0002-48 lands is a surface change nobody regenerated:
   there is no currency gate in the repo today, so this test is exactly as current as the
   committed file. Its module doc says that rather than claiming a symmetry it does not have,
   and 0002-48 should fold the assertion into whatever it generates -- the same check against
   fresh data, and one reader of the surface instead of two.

   It reads the committed file, so it needs neither nightly nor `cargo-public-api`, and it
   guards against a truncated snapshot passing vacuously. Its header filter skips `# ` comment
   lines but keeps `#[non_exhaustive]` item lines, which would otherwise have been a hole the
   snapshot happens not to have anything in today.

   **The test is excluded from the package, because its input already was.**
   `crates/outrig/Cargo.toml` has excluded `public-api.txt` since it was introduced -- it is a
   review artifact of no use to a consumer -- and this test is ungated, unlike
   `library_surface`. Shipping an ungated test without the file it reads turns `cargo test` in
   an unpacked or vendored release into a missing-file panic; publishing and ordinary dependency
   builds never run it, so nothing else was affected. The two travel together in `exclude`
   rather than the test degrading to a skip when the file is absent, because in the repo a
   missing snapshot is precisely what it should fail on.

9. **`McpSessionError::new` is `pub(crate)`.** The first cut mirrored `SidecarUnwindFailure::new`
   and took its `pub` along with its shape. But that one is public because
   `outrig-cli/src/cli/session_setup.rs` calls it, and this one has no caller outside the crate
   and no prospect of one: nothing outside drives an MCP session outrig owns, so a consumer
   reads these errors rather than building them. A public constructor on a `#[non_exhaustive]`
   type reopens exactly the construction the attribute closed, and on a task whose criterion is
   that nothing survives without a verdict, "surface with no caller" is that criterion failing.
   Promote it if a consumer ever needs it.

10. **`McpService` still does not name the server it failed against, and that is left open.**
   `McpToolsListFailed` carries `name`; its call-tool sibling does not, which is why
   `mcp_proxy.rs:413-427` prepends the server by hand when it renders the failure into a tool
   result. Adding the field is a public break, so it is free now and costs a major after
   0002-54 -- the disclosure matters more than the fix, and it is
   `plan/next/mcp-service-error-does-not-name-its-server.md`. It was out of the shape this task
   settled on, and the proxy's existing compensation means nothing is currently unreadable.
