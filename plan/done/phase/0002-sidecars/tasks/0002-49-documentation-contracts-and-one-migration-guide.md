# 0002-49 -- Correct the documentation contracts and draft the 0.1 -> 0.2 migration guide

## Context

Nine documented claims contradict the code, and the release notes are spread across two RC
sections plus `Unreleased` in each crate. Docs are a contract in this project -- `doc/` is
design-first and `doc/reference/config.md` ships inside the binary via `include_str!` -- so a
false sentence is a defect with the same standing as a wrong branch.

This task fixes what is **false now** and drafts the migration material. Cutting the final
`[0.2.0]` headings, dates, links, version prose, and support table is **0002-54**'s job: those can
only be written once, at the version that ships, and this task runs before an RC. Splitting them
is what keeps `0002-49` from having to be redone after rc.3.

### Wrong today

Each was checked against the tree while this task was written.

1. **The CLI changelog says aliases do not fail over.** `crates/outrig-cli/CHANGELOG.md`'s
   `[Unreleased]` describes model aliases as they behaved when `e912388` landed. 0002-36 reversed
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
needs exists in draft, ready for 0002-54 to date and publish.

## Deliverables

- **The nine corrections above.**
- **A drafted 0.1 -> 0.2 migration checklist** in each crate's `Unreleased` section, enumerating
  the breaks by name rather than relying on a reader to reconstruct them. The audited break is
  real and coherent -- a real 0.1-era consumer compiles on 0.1, fails on trunk for the documented
  reasons, and compiles again once migrated -- so there is a known-good shape to describe. The
  list must cover, at minimum: MSRV and platform expectations; the **rmcp 1.x -> 3.1** bump and
  whatever 0002-47 decided about the exposure it created; `#[non_exhaustive]` construction *and*
  matching, including that downstream matches now need wildcard arms; sealed `BackingClient`;
  opaque `ImageTag`; the `OpenAiOptions` / `AnthropicOptions` construction change and the retry
  configuration that moved onto them; `Model::provider` becoming optional and `Model::source()`
  as the accessor; `ExecOptions`; `Workspace`'s declared-versus-effective accessors; the mount
  error reshapes; the removed bootstrap helper; **referenced-sidecars-only** semantics for
  top-level sidecars; the removed implicit preamble; alias failover; and 0002-46's local-LLM
  decision. Whatever 0002-38 and 0002-42 through 0002-47 settle joins it.
- **State that there is no stable binary ABI.** The crate produces ordinary `rlib`/metadata
  artifacts and exposes no `cdylib`, no stable `extern "C"`, no `#[repr(C)]` FFI contract, and no
  fixed symbol layer; public layouts changed and downstream crates rebuild, as Cargo normally
  does. Say it explicitly and do not claim ABI compatibility anywhere.
- **Not in this task:** dated `[0.2.0]` headings, tag links, the quickstart version sample, and
  `SECURITY.md`'s supported-version table. Those are 0002-54's, because they are only writable once.

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

- **Soft: after 0002-37 through 0002-47.** Those change documented behavior -- 0002-37 changes what
  a hostname rule permits, 0002-38 changes how `[network]` precedence is expressed, 0002-42 through
  0002-47 change public API, and 0002-46 decides what `style = "mistralrs"` does. Writing the
  checklist before them means writing it twice.
- Must precede 0002-52, which cuts the RC these notes describe.

## See also

- `plan/done/phase/0002-sidecars/tasks/0002-36-model-alias-failover.md` -- the behavior item 1
  misdescribes.
- `plan/next/startup-banner-has-no-tests.md` -- item 8's buffered entry.
- `doc/usage/mcp.md`, `doc/usage/run.md`, `doc/reference/cli.md`, `crates/outrig-cli/README.md`,
  both `CHANGELOG.md` files.
- `plan/todo/0002-54-release-0.2.0.md` -- takes the final-only half of this work.

## Decisions

1. **The commit walk's boundary for `outrig-cli` is the rc.1 prep commit, not the crate's last
   tag.** `outrig-cli-v0.1.0` is the newest tag bearing that crate's name, but the changelog
   already carries a `[0.2.0-rc.1]` section covering through `3cbde0f9`, so walking from the
   tag would re-audit content that is already written down. The walk ran
   `3cbde0f9..HEAD -- crates/outrig-cli/src` (60 commits) and
   `outrig-v0.2.0-rc.2..HEAD -- crates/outrig/src` (14 commits). The mismatch between the two
   crates' boundaries is itself a finding, recorded as decision 8.

2. **A commit is "covered" if its behavior is described in *either* changelog, not only its
   own crate's.** An initial pass over the CLI walk flagged 36 commits as unrecorded; checking
   each against both files brought it down to ten topics. Config keys, validation rules, and
   public types are described in `crates/outrig/CHANGELOG.md` because the schema and the API
   live in that crate, and `outrig_cli`'s entire published surface is `run() -> ExitCode`. So
   `3cdbd301` (the `subagent-max-depth` rename), `43c30ede` (device passthrough and the
   `no-new-privileges` opt-out), `292452b6` (the global `[workspace]` block), `f8dd95b1`
   (`request-timeout-secs` bounds), `9453c620` (mount provenance), and the five primary-view
   sidecar commits are each recorded once, in the library file, and are correctly absent from
   the CLI's. The rename additionally never shipped under its old spelling -- the library's
   rc.1 section documents `subagent-depth-max` directly -- so it carries no migration step.

3. **New entries are written per user-visible capability, not per commit.** The subagent
   system spans five commits (`6346061d`, `05c76acc`, `09e8abd7`, `58560e4b`, `d2c45b92`) and
   had no `Added` entry in either crate -- a whole feature that shipped unannounced. It gets
   one bullet, not five. The nine other topics that gained entries are `72ea130a` (both
   crates), `233dd1e7`, `6b119a9f`, `9f44ed07`, `adee4f61`, `fec7f161`, `6443bb02`,
   `a556ec42`, and `9cd2b83b`.

4. **Deliberately invisible, with reasons.** Test-only (`4ea438ea`, `5dc5bd30`), plan and
   prompt files (`01cd7070`, `0aed79be`), a compiler-warning cleanup (`2c03ebe6`), link
   configuration for a helper that was not yet in a release (`fac486c9`), internal ownership
   and loop fixes with no surface (`48dc1802`, `67baef48`, `5ae5f8b1`), and error-text polish
   (`5ae5f8b1`). `617e19ff` is covered retroactively by `d79f8372`'s removal entry, which
   describes the state both commits arrived at together.

5. **`72ea130a` is the one finding that changes this task's weight.** It is a security fix --
   a hostname allow-rule matched against the name the client announced, so a container under
   `mode = "filter"` could reach an arbitrary address by claiming an allowed name -- and it had
   no entry in either changelog. `SECURITY.md` names that class as in scope. It is written up
   in both crates rather than only the library, because the property a CLI user relies on is
   the one that was not holding.

6. **The migration checklist is a `### Migrating from 0.1` subsection** at the top of each
   crate's `[Unreleased]`, rather than being spread across the five Keep a Changelog headings.
   It is a different kind of thing from a change record -- an ordered list of work, read once
   -- and 0002-54 promotes it whole into `[0.2.0]`. The split between crates is by what breaks:
   the library's is Rust-source breaks and carries the no-stable-ABI statement, the CLI's is
   config and command-line breaks.

7. **Item 5's correction goes in `[Unreleased]`, not into the rc.2 section.** rc.2 is
   published, so rewriting it would make the tree disagree with what consumers already read.
   The correction states what rc.2 said, what is actually true, and why the difference matters
   -- the derived `Default` declares neither path where the hand-written one declared both,
   which is what makes a repo file with no `[workspace]` table stop shadowing the global one.

8. **Two structural findings are recorded and not fixed**, as
   `plan/next/cli-changelog-has-no-rc2-section.md`. `244485b4` cut a `[0.2.0-rc.2]` section in
   the library changelog and none in the CLI's, which is why the CLI's `[Unreleased]` has two
   runs of the same subsection headings; and the CLI's `[0.2.0-rc.1]` heading links to a tag
   that does not exist in this clone. Both are version-bearing and therefore 0002-52's or
   0002-54's, and the tag question cannot be settled without the remote. Guessing at a link is
   how a dead link becomes a false one.

9. **`plan/next/startup-banner-has-no-tests.md` was pulled in, and its "no production change
   needed" premise was wrong.** `print_banner` built a `String` and ended in `eprint!`,
   returning `()`; the buffer was never reachable. Both `run`'s and `mcp`'s banners now split
   into a `render_banner(..) -> String` with the `eprint!` in the caller. The entry's four
   acceptance criteria are met. Its one conditional deliverable -- the mid-turn move
   announcement -- is not, because that line is written to process stderr from inside an async
   model call and asserting on it in-process needs either a global redirect that races other
   tests or a writer injected into `FailoverModel` for no other reason. It is refiled narrowly
   as `plan/next/failover-move-announcement-untested.md`.

10. **The doc examples are read from `doc/`, not transcribed into the test.** A test carrying
    its own copy of the minimal config would have passed for the three releases in which the
    page advertised `[workspace] root = "."`. `crates/outrig-cli/tests/doc_contracts.rs`
    extracts the fence under `## Minimal Config` from `doc/usage/mcp.md` and parses it, and
    `app.rs`'s sweep extracts the `## Commands` fence from the crates.io README and resolves
    every line against the clap tree, expanding `a|b` alternations and accepting
    `MissingRequiredArgument` as the one error a shape may legitimately raise. Both were
    confirmed to fail against the pre-fix text before being kept.

11. **`[workspace]` was dropped from the minimal config rather than corrected to
    `host-path`/`container-path`.** Both parse, but the block's defaults are already `.` and
    `/workspace`, so including it taught a reader that a minimal config needs a stanza that
    does nothing. `doc/reference/config.md` documents the real keys.

12. **The embedded docs are not copies, and the test guarding them asserts paths, not bytes.**
    The eight pages under `crates/outrig-cli/src/mcp_self/docs/` are the real files; the
    matching entries under `doc/` are git symlinks (mode `120000`) pointing at them, so mdBook,
    `scripts/audit-doc-style.py`, and `include_str!` all read one set of bytes. There is no
    duplication and never was. This task first read them as byte-identical duplicates -- `diff`
    and `ls` on the containing directory both follow symlinks and say nothing -- and wrote a
    content-comparison test, which compared each file to itself and could not have failed.

    What can actually break is the arrangement rather than the contents: a checkout without
    symlink support, or an editor that saves over a symlink, leaves two real files that drift
    from that point on. The test compares canonicalized paths, and was confirmed to fail when
    a symlink is replaced by a copy of its target.

    The arrangement itself is load-bearing and was arrived at the hard way: `39acfa83` records
    that `include_str!` used to reach the repo-root `doc/` tree, which `cargo package` does not
    pack, so `outrig-cli` could not be published at all. Moving the real files inside the crate
    and symlinking `doc/` back out is the deeper fix, already taken. The test keeps it honest;
    it does not replace it. It is keyed on `DOCS` -- the pages actually served by `get_doc` --
    rather than on a directory walk, so it asserts exactly the set whose staleness would
    mislead an agent.

13. **Extracting an example by heading is not new coupling.** `.github/workflows/ci.yml` already
    runs lychee with `--include-fragments` over `doc/**/*.md` and `README.md`, so a renamed
    heading is a red build today -- and this task adds two such anchor links itself. The
    extractors fail loudly rather than vacuously: `fenced_block_after` panics naming the
    heading it could not find, and the README sweep asserts the fence is non-empty, so a test
    cannot quietly start checking nothing.

14. **A repo-wide "every toml fence under `doc/` parses" sweep was considered and refused.**
    The fences are not one schema and not all whole: `doc/concepts/mcp-servers.md` has an
    `image.toml` fence, which is a different schema from `Config` entirely, and
    `doc/reference/config.md`'s twenty-five are mostly deliberate fragments like a bare
    `[providers.openai]` stanza. A sweep would need a per-fence opt-in marker -- a new markdown
    convention over the whole doc tree -- for a weaker guarantee than naming the examples that
    are meant to stand alone. If this grows, it grows as more named-heading extractions.

15. **The `render_banner` split follows the crate's existing shape rather than introducing
    one.** `build_tools_summary` sits immediately after it in the same file, and
    `render_merged_mcp`, `clean_summary`, `render_table`, `render_inspect`, `render_prompt`,
    and `render_candidate_reasons` are all `-> String` with the printing at the caller. The
    alternative of writing through a `&mut dyn Write` would have been a new pattern for one
    function -- the crate has no such sink anywhere -- and the tests want a `String` to search.

16. **`builtin_image::banner_suffix` grew into `banner_image_config_row`.** Its comment said it
    existed "so the two banners cannot word it differently", but it shared only the ` (built-in
    default)` suffix while `run` and `mcp` each held the `[outrig] image-config:  ` label as
    its own literal -- the half more likely to drift. The whole row is now rendered in one
    place and tested there; each banner keeps one test that the row reaches its output, which
    is the part a shared helper cannot prove.

17. **Several corrected claims are covered by tests that already existed**, and were checked
    rather than re-written: the `[sidecars.<name>]` shadowing case by `tests/builtin_default.rs`,
    the fallback announcement on stderr by `tests/embedded_image.rs` and `tests/builtin_default.rs`,
    and `--env`'s repeatability and precedence by `tests/cli_env.rs` and
    `cli/run.rs`'s `env_flag_collects_multiple_values`. New tests were written only where the
    claim had none.

18. **A review round caught four claims this task got wrong, each verified against the code
    before being corrected.** They are worth recording because three of the four are the same
    failure mode the task exists to fix -- prose that describes a mechanism accurately enough
    to sound right.

    - **The fallback veto.** The first draft said only a `[sidecars.<name>]` block vetoes the
      built-in default, and listed `outrig-default` among the reserved sidecar names. Both
      wrong. `RESERVED_IMAGES` is all three names and `RESERVED_SIDECARS` is only the `-fs`
      and `-shell` pair -- `[sidecars.outrig-default]` is deliberately not reserved, since the
      built-in declares no sidecar by that name. And a veto is not uniformly fatal: `inject`
      returns `Some(DEFAULT_IMAGE)` when the user's own `[images.outrig-default]` is what
      shadowed it, so only the other four end startup. Two tests now pin this, because it is
      exactly the claim that had gone unchecked.
    - **`LaunchSpec`.** The checklist framed `from_config` as a behavior-only change and the
      one item that is not a compile error. 0.1 had no `from_config`: it had
      `from_image_config(&image_config, &workspace, repo_root, log_dir) -> Self`, synchronous
      and infallible, and it is gone. The entry now leads with the constructor and keeps the
      network warning where the rewrite will be read, with a note for anyone arriving from a
      release candidate, where the compiler flags nothing.
    - **The chain budget.** "The first candidate's provider" is what 0002-36's own decision
      text says, but `chain_retry_budget_secs` uses `find_map` and returns `None` for a
      `Mistralrs` row, so it is the first *remote* candidate. Repeating the decision text
      would have told a user with a local-first alias to write `retry-budget-secs` onto a
      provider that now rejects unknown keys -- advice that makes the config fail to load.
    - **`set_container_path`.** Only `set_host_path` clears the recorded `ConfigSource`, and
      correctly so: the container path is not resolved against the declaring file, so clearing
      provenance there would discard the host path's for nothing.

19. **The shadowing error still claims the sidecar-only rule the docs just stopped claiming.**
    `session_setup` raises "shadowed by a `[sidecars.<name>]` block" for all four fatal vetoes,
    three of which are image blocks, and it contradicts the note printed directly above it.
    That is a production diagnostic rather than a documentation contract, and it is unchanged
    on the base branch, so it is filed as
    `plan/next/shadowing-error-blames-the-wrong-block.md` rather than fixed here. The corrected
    pages describe the *condition* and deliberately do not quote the message.

20. **Both migration checklists opened with a blanket claim about *how* a break announces
    itself, and both were false.** The library's said every listed break is a compile error;
    the CLI's said none is silent. The counterexample that surfaced it is
    `sanitize_tool_name`, whose `(&str, &str) -> String` shape is unchanged from 0.1 while its
    output moved -- a consumer compiles clean and keeps a stale tool list, which the same
    checklist tells them to refresh. Auditing the rest found it was not one exception: three
    library items are fully compiler-silent (referenced-sidecars-only, tool names, the
    implicit preamble) and four more are compile breaks whose fix leaves a behavioral question
    open (`LaunchSpec::from_config`, `ExecOptions`'s unset workdir, `Model::source`'s panic,
    `set_container_path`'s provenance). The CLI list has two silent items -- an unreferenced
    `[sidecars.<sc>]` block simply does not start, with no warning anywhere in the tree, and
    an agent omitting `preamble` quietly stops sending the sentence 0.1 supplied.

    Both intros now split the list by how the break reaches the reader and name the silent
    ones, which is the half a checklist is actually for. The lesson generalizes past this
    task: a summary sentence over a list is itself a claim, and it rots the same way the list
    does -- it just has no single line to check it against.
