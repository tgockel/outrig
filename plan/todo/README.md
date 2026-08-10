# Plan: v0 task queue

The per-step index of `plan/todo/`. Tasks land lowest-numbered first; every task depends only
on smaller-numbered predecessors. Run `/next-task` to advance one task end-to-end on its own
branch; run `/groom-plan` to maintain ordering after edits or `plan/next/` pulls.

See [`.claude/CLAUDE.md`](../../.claude/CLAUDE.md) for the workflow conventions.

This queue is pre-freeze work for `0.2.0`. Everything in it either changes what an existing
thing means or fixes a contract a release would otherwise freeze wrong. The public surface is
now settled and, with 0096 landed, every breaking change it will take is in: 0094 sealed every
public type with `#[non_exhaustive]` and shipped the constructors that replace the struct
literals it forbids, which is what makes 0098-0101 additive rather than breaking -- each of
those adds a field or a variant to a type the sweep insulates. 0097 was the first to cash that
in, adding a `ConfigSource` to `ImageConfig` / `MountConfig` and a `declared_in` field to two
sealed `ConfigValidationError` variants without a single breaking change. 0095 finished the
other half --
the options struct, the sealed `BackingClient`, and an opaque `ImageTag`. 0096 reshaped
`SidecarServerSpec` into an enum and gave the library API every sidecar placement the config
path has, which is what an external embedder was blocked on. The e2e suite compiles again and
CI gates it (0092), so `library_surface.rs` -- the facade test that work was measured against
-- is runnable. 0093 settled the reach question: all six of the library's public modules are
supported API, so 0094-0095 cover more than first scoped, and `outrig-cli`'s internals are no
longer public at all. 0098 cashed the sweep in a second time, adding an `LlmProvider::Anthropic`
variant and a `Model::max_tokens` field additively; the one break it did take -- renaming three
provider-specific `ConfigValidationError` variants -- was chosen, not forced.

Everything through 0111 landed; 0105-0111 refilled the queue from `plan/next/`
against one gate: **does deferring this force a breaking change later, or freeze a contract a
point release cannot fix?** The 0094 sweep is what makes that question narrow -- `#[non_exhaustive]`
makes field and variant *additions* free, so what is left are field *type* changes, variants the
sweep did not seal, and published method signatures. Seven of the 39 entries in `plan/next/`
qualified. The rest are additive by construction, internal, or explicitly post-v0, and stay there.

0110 landed, taking the last of the seven field *type* changes: `Model::provider` is now
`Option<String>`, because a model entry names either a provider or an `alias` over other models.
`Model` follows `ImageConfig`'s two-shapes-one-table pattern exactly -- every field `Option`,
exactly-one-shape enforced by validation, and a discriminated `Model::source()` as the way readers
ask which shape they got. Two calls are worth carrying forward. Its shape and alias-graph rules
validate *ungated*, unlike every other model rule, because they establish an invariant rather than
resolve a cross-reference -- so `outrig build` now rejects a typo'd alias target while still
accepting a typo'd `provider`, which reads as inconsistent and is not. And a single-target alias
skips candidate selection entirely: filtering it replaced `MistralrsFeatureDisabled`, the message
carrying the *rebuild with `--features local-llm`* remedy, with a "no usable candidate" list of
one. Selection filters only where there is a genuine choice.

0112-0113 are the queue's first **post-freeze** tail, and are here despite failing the gate above
rather than because they pass it. Both are additive: they live entirely in `outrig-cli`, whose
internals 0093 made private, so neither regenerates `public-api.txt` nor writes a CHANGELOG line.
They are numbered rather than left in `plan/next/` because 0113 has a hard dependency on 0112, and
the buffer cannot express ordering -- a dependency between two buffer entries is a note, while a
dependency between two numbered tasks is an invariant this file maintains. They sit after 0111 so
nothing pre-final is displaced.

- **0113 [model-alias-failover](0113-model-alias-failover.md)** -- the runtime half 0110 defers:
  moving to the next alias candidate when one fails *inside* a `completion()` call, which is the
  only layer that can do it without re-running container tool calls. Resolves 0110's design fork §4
  (the ceiling must move with the identifier) and its shared-budget blocker (a chain-scoped
  deadline, at the cost of `RetryPolicy: Copy`). Depends on 0110 and 0112.

0112 landed, and the split is narrower than "a connect failure is terminal". `RetryPolicy` grew a
second bound, `connect_budget`, that applies only while no attempt has got bytes back, and
`send_with_retry` latches it off on the first response of any kind. A refused connect is still
transient -- the classification was never the bug -- but it now gives up in 30 seconds rather than
the full `retry-budget-secs`, so a typo'd `base-url` ends the turn instead of ten minutes of retry
lines. Two calls carry forward. The bound in force while unanswered is the *smaller* of the two
rather than `connect_budget` outright, which keeps `retry-budget-secs = 0` the single "no retries"
knob instead of a knob with an exception. And `reqwest::Error::is_connect()` has to be read in the
loop's own `Err` arm: the error is boxed into `HttpError::Instance` immediately after, so no
downstream reader -- `is_transient` included -- can still tell what it was. Both discard-port
fixtures dropped their `retry_budget_secs: Some(0)` crutch and stay fast on the new bound, which is
the end-to-end proof; the one real-clock test that measures shutdown grace rather than retry opts
out explicitly, and says why.

0105 landed, and was the cheapest and highest-value of them: 0097 had already built the machinery
-- `ConfigSource`, per-mount stamping, `resolved_host_path()` -- and given `declared_in` to the two
image variants it could reach additively, so threading provenance through the mount variants was
mostly wiring. A mount diagnostic now ends with `(declared in "<file>")`, which is what the
concatenated global+repo mount list makes necessary: a bare relative path is ambiguous between two
files. All five `MountRuleViolation` variants and all five `WorkspaceMount*` variants took it and
are now `#[non_exhaustive]`, so the next field is additive. Its one open fork was resolved *yes*:
the container-path rules carry it too, `ContainerRoot` ceasing to be a unit variant, because the
clause answers which file to go edit rather than what a path resolved against -- and that rule's
message carries no path at all, so the declaring file is the only handle it has.

0107 landed: `ExecOptions` replaces the bare `&BTreeMap` on all four published exec methods, and
`with_workdir` becomes `--workdir` on the `podman exec`. Absorbing the environment into the struct
rather than leaving it a third parameter was the call worth recording -- Rust has no default
arguments, so the "add a parameter" shape breaks every call site anyway without buying source
compatibility, and `ContainerCreateOptions` already holds its `env` the same way. Two findings came
out of building it. A missing directory is *not* an `Err`: `process::try_capture` fails only when
the spawn does, so podman's own non-zero exit arrives as `Ok(Output)` with the path named on
stderr -- which is what the existing "a non-zero exit is data, not an error" contract requires, and
what the acceptance criterion had to be read against. And the first draft of the e2e test asked for
`/workspace`, which the run path already sets with `-w`, so every positive assertion passed with the
flag deleted; it now uses `/workspace/sub`, verified by removing the flag and watching it go red.
The suite gained its first shell-less image, which is the case the argv form exists to serve.

0111 landed last, as sequenced: it was the only one of the seven that is API *shape* rather than
API *correctness*, so it was the entry to cut had the window tightened. `LlmProvider::openai` /
`::anthropic` now take `OpenAiOptions` / `AnthropicOptions`, and `with_retry_budget_secs` -- rc.1's
non-breaking workaround, and a silent no-op on the `Mistralrs` variant -- is gone, the no-op
dissolved by construction rather than replaced by a check. Two things carry forward. An options
struct that is `#[non_exhaustive]` needs `new` plus `with_*` setters to be usable at all from
downstream, because the attribute forbids the struct literal that `..Default::default()` would
need; the draft that shipped bare `pub` fields typechecked in-tree and would have been dead on
arrival outside it. And no lint catches that -- `field_reassign_with_default` declines to fire
precisely because its suggested fix is the form the attribute forbids -- so the convention 0095 and
0107 established is held by review and nothing else.

Two things are **not** queued and are worth stating so they are not re-litigated. Regenerating
`crates/outrig/public-api.txt` is each task's own deliverable rather than a task of its own -- four
of the seven rewrite it, and a queued step would only be a second place to forget.
`plan/next/public-api-snapshot-gate.md` (an opt-in check so the snapshot cannot silently rot) stays
in the buffer as post-release process hygiene; a check against `cargo-public-api` 0.52.0 found no
semantic drift in the current file, only `std::io` -> `core::io` renderings.

0100 landed: `subagent-width-max` (default `8`, range `1..=16`) bounds how many live subagents one
launching agent may hold, refusing past the limit rather than queueing. It was sequenced before
0101 deliberately -- model selection makes wide fan-out expensive rather than merely slow, so the
containment half belonged first, and 0101 could then assume a bounded tree. What the cap does not
settle is whether the five-second teardown budget fits the tree it permits; that measurement is
filed as `plan/next/subagent-tree-shutdown-grace.md`.

0101 landed, and closed the arc: `outrig__subagent` takes an optional `model` naming a
`[models.<name>]`, so an expensive parent can farm mechanical work out to a cheap subagent instead
of running the whole session cheap. Omitting it inherits the launching agent's model and is
byte-for-byte the old path, in behavior and in the logs. The schema's `enum` lists only the models
the running build can actually reach, and disappears entirely when that set holds one name.
Sampling and limits stay with the launching agent -- carried forward by overwrite direction, which
is what keeps the session's `--max-tool-calls` / `--max-tool-result-bytes` alive across a
re-resolution. Three of its forks were left open on purpose: the device override and per-model
sampling defaults have no pressing audience, and the operator allowlist -- the one that matters,
since an agent can escalate *itself* onto the expensive model -- is filed as
`plan/next/subagent-model-allowlist.md`.

0102-0104 were one arc, and the last of them is why the first two were pre-freeze work rather
than polish. This repo's `.agents/outrig/config.toml` used to embed every MCP server in the
primary image, so nothing in daily use exercised sidecar placement, `view = "primary"`, or the
`image-name` pull path -- the feature only ran under the gated e2e suite. Moving the three
servers out found three defects that a release would otherwise have frozen: a `view = "primary"`
payload ran as root in the primary's user namespace and left subuid-owned files behind (0102,
landed -- the launcher now drops to the session's ids once the graft is in place), the
launcher's missing `PATH` lookup made the published quickstart image unusable, which is what
kept `primary_view_e2e.rs` red on trunk (0103, landed -- a bare program name is resolved
against the sidecar image's `PATH` before the namespace join, and that e2e now passes against
the stock image), and the payload inherited the primary's procfs, in which `/proc/self` names
nothing, so every rustup shim failed (0104, landed -- the launcher mounts a fresh `proc`). The
arc is closed; all three servers now run as `view = "primary"` sidecars every session.

0106 landed: `request-timeout-secs` is bounded to `1..=3600`, the hour its sibling
`retry-budget-secs` already used. Two things it turned up are worth recording, because both were
assumptions the queue carried rather than facts. The key has **no top-level form** -- it lives
only on the two remote provider variants, so the task's "top level and per provider" wording
described a path that does not exist; the criteria were corrected to the one that does, and the
missing default is filed as `plan/next/top-level-request-timeout-secs.md`, additive and so not
window-bound. And `0` is an *immediate* timeout in reqwest rather than a disabled one, the
opposite of what the `plan/next/` entry it was groomed from claimed -- verified against the
pinned 0.13.4 source, not inferred. That makes `0` mean opposite things on the two keys, which
is correct: the budget counts attempts, the timeout bounds one. A third finding was incidental --
`style = "mistralrs"` is a unit variant, so `deny_unknown_fields` silently swallows *every*
unknown key on it, filed as `plan/next/mistralrs-provider-swallows-keys.md` and pinned by a test
written against today's behavior.
