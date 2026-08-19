# 0126 -- Correct the documentation contracts and draft the 0.1 -> 0.2 migration guide

## Context

Nine documented claims contradict the code, and the release notes are spread across two RC
sections plus `Unreleased` in each crate. Docs are a contract in this project -- `doc/` is
design-first and `doc/reference/config.md` ships inside the binary via `include_str!` -- so a
false sentence is a defect with the same standing as a wrong branch.

This task fixes what is **false now** and drafts the migration material. Cutting the final
`[0.2.0]` headings, dates, links, version prose, and support table is **0129**'s job: those can
only be written once, at the version that ships, and this task runs before an RC. Splitting them
is what keeps `0126` from having to be redone after rc.3.

### Wrong today

Each was checked against the tree while this task was written.

1. **The CLI changelog says aliases do not fail over.** `crates/outrig-cli/CHANGELOG.md`'s
   `[Unreleased]` describes model aliases as they behaved when `e912388` landed. 0113 reversed
   it: `FailoverModel` moves a turn to the next candidate from inside one `completion()` call.
   The section still says selection "answers 'am I configured for this' rather than 'is this
   endpoint up'" and that an alias "does **not** fail over when a vendor rate-limits
   mid-session". Two commits are missing entirely: the 30-second connect budget for an endpoint
   that never answered, and a provider response outrig cannot use ending the turn rather than the
   session. The replacement wording needs three specifics the old text lacks: the retry budget is
   shared across the chain, a fresh request restarts at the head of the chain, and completed tool
   calls are never replayed.
2. **MCP docs say no image is an error.** `doc/usage/mcp.md:67-71` promises startup fails with
   `error: no --image or default-image configured`. Both `run` and `mcp` now fall back to the
   built-in default image (`crates/outrig-cli/src/builtin_image/default.toml`).
3. **The advertised minimal config does not parse.** `doc/usage/mcp.md:107-110` shows
   `[workspace] root = "."`. `Workspace` is `deny_unknown_fields` with `host-path` /
   `container-path`; `root` is rejected. The one example a new user copies is the one that fails.
4. **The crates.io CLI README advertises a command that does not exist.**
   `crates/outrig-cli/README.md:45` lists `outrig design`; the command is `outrig design prompt`,
   so the bare form exits with a usage error. This one is published to crates.io.
5. **The library rc.2 changelog reports a removal that did not happen.**
   `crates/outrig/CHANGELOG.md:215` says "`impl Default for Workspace` is gone with the fields".
   `Workspace` still derives `Default` (`crates/outrig/src/config/mod.rs:1093`), `outrig-cli`
   calls it (`src/init/repo.rs:122`), and its "declares nothing and inherits both" semantics are
   load-bearing and documented at `config/mod.rs:1112`. The rc.2 section is already published, so
   the correction goes in the 0.2.0 section rather than by rewriting history.
6. **`doc/usage/run.md` omits that `--env` is repeatable.**
7. **`doc/reference/cli.md` says a config-less run needs an agent and an explicit image.** It
   does not, for the same built-in-default reason as item 2.
8. **The startup banner's documented claims have no focused tests**, so items 2, 6, and 7 can
   regress silently. `plan/next/startup-banner-has-no-tests.md` is the buffered entry; either
   pull it in here or record why not.
9. **Item 5's class is not closed by fixing item 5.** Walk `git log <last-tag>..HEAD` for both
   crates and confirm every commit touching `src` either has an entry or is deliberately
   invisible. That method is what found items 1 and 5; run it rather than trusting this list.

## Goal

Every documented claim about behavior is true at rc.3, and the migration material a 0.1 consumer
needs exists in draft, ready for 0129 to date and publish.

## Deliverables

- **The nine corrections above.**
- **A drafted 0.1 -> 0.2 migration checklist** in each crate's `Unreleased` section, enumerating
  the breaks by name rather than relying on a reader to reconstruct them. The audited break is
  real and coherent -- a real 0.1-era consumer compiles on 0.1, fails on trunk for the documented
  reasons, and compiles again once migrated -- so there is a known-good shape to describe. The
  list must cover, at minimum: MSRV and platform expectations; the **rmcp 1.x -> 3.1** bump and
  whatever 0124 decided about the exposure it created; `#[non_exhaustive]` construction *and*
  matching, including that downstream matches now need wildcard arms; sealed `BackingClient`;
  opaque `ImageTag`; the `OpenAiOptions` / `AnthropicOptions` construction change and the retry
  configuration that moved onto them; `Model::provider` becoming optional and `Model::source()`
  as the accessor; `ExecOptions`; `Workspace`'s declared-versus-effective accessors; the mount
  error reshapes; the removed bootstrap helper; **referenced-sidecars-only** semantics for
  top-level sidecars; the removed implicit preamble; alias failover; and 0123's local-LLM
  decision. Whatever 0115 and 0119-0124 settle joins it.
- **State that there is no stable binary ABI.** The crate produces ordinary `rlib`/metadata
  artifacts and exposes no `cdylib`, no stable `extern "C"`, no `#[repr(C)]` FFI contract, and no
  fixed symbol layer; public layouts changed and downstream crates rebuild, as Cargo normally
  does. Say it explicitly and do not claim ABI compatibility anywhere.
- **Not in this task:** dated `[0.2.0]` headings, tag links, the quickstart version sample, and
  `SECURITY.md`'s supported-version table. Those are 0129's, because they are only writable once.

## Acceptance

- `python3 scripts/audit-doc-style.py doc/` passes, and the mdBook build is clean.
- Each claim has a check behind it where one is possible: the minimal config in `doc/usage/mcp.md`
  parses through `Config::load_from_str` in a test, and `outrig design` versus `outrig design
  prompt` is checked against the clap command tree rather than by eye. A doc example that is
  executed cannot rot.
- The commit walk is clean for both crates: no commit touching `src` since that crate's last tag
  lacks an entry or a written reason to have none.
- The migration checklist names every item in the list above; a reviewer can diff it against that
  list rather than judging completeness by feel.
- Nothing in this task's output claims a version or a date.

## Dependencies

- **Soft: after 0114-0124.** Those change documented behavior -- 0114 changes what a hostname
  rule permits, 0115 changes how `[network]` precedence is expressed, 0119-0124 change public
  API, and 0123 decides what `style = "mistralrs"` does. Writing the checklist before them means
  writing it twice.
- Must precede 0127, which cuts the RC these notes describe.

## See also

- `plan/done/0113-model-alias-failover.md` -- the behavior item 1 misdescribes.
- `plan/next/startup-banner-has-no-tests.md` -- item 8's buffered entry.
- `doc/usage/mcp.md`, `doc/usage/run.md`, `doc/reference/cli.md`, `crates/outrig-cli/README.md`,
  both `CHANGELOG.md` files.
- `plan/todo/0129-release-0.2.0.md` -- takes the final-only half of this work.
