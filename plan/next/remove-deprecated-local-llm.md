# Remove the deprecated `local-llm` feature

> **Still buffered, and still post-0.2.0** -- the deprecation has to ship in a released version
> first. But the open question below (parse error versus no-op) is answered by
> `plan/todo/0123-deprecated-local-llm-behavior-for-0.2.0.md`, which settles the 0.2.0 behavior.
> Read 0123's `## Decisions` before executing this entry; the removal inherits that answer.

## Context

The `local-llm` Cargo feature, the `style = "mistralrs"` provider, and the
in-process backend behind them were deprecated (warnings only, nothing removed)
in the commit that added this entry. That change deliberately removed no code and
broke no config: it added a build warning, a per-model runtime warning, a
deprecation clause on the existing feature-off resolve error, and doc banners
including a written retraction of the "why not localhost" argument.

This entry is the other half: the actual removal. It is filed rather than queued
because the deprecation needs to *ship*, and be in users' hands for at least one
release, before the removal is defensible. Deprecating and removing in the same
release is not a deprecation.

## Why it is not one commit

The footprint spans both crates and, more importantly, the library's **public
API**. Measured at deprecation time:

**`outrig-cli` (mechanical).** Two files exist only for this and delete whole:
`src/llm/mistralrs.rs` (~1080 lines, the only `candle-core` and `indexmap`
consumer) and `src/llm/registry.rs`. Two test files are whole-file gated and go
with them: `tests/mistralrs_smoke.rs`, `tests/llm_registry.rs`. Then ~40
`#[cfg(feature = "local-llm")]` sites unthread from `llm.rs`, `subagent/mod.rs`,
`cli/run.rs`, `hf.rs`, `config_init.rs`, `init/mod.rs`. Three things worth
noticing before starting:

- **The streaming path goes away with it.** `run_turn_streaming_to` /
  `run_turn_streaming_inner` are `#[cfg(feature = "local-llm")]` and mistralrs is
  their only caller, so removal deletes outrig's *only* streaming code path.
  `plan/next/streaming-path-has-no-http-retry.md` is about that path and should be
  re-read (probably closed) at the same time.
- **`hf.rs` collapses, not deletes.** The trait, `HfFile`, `filter_gguf` and
  `format_size` are unconditional; only `ApiHfTreeFetcher` is gated. Decide
  whether anything still wants a HuggingFace tree listing once no model row can
  name a repo.
- **`--device` and `MistralrsDeviceSpec` leave `session_setup.rs` and `run.rs`**,
  including the `parse_mistralrs_device` clap parser and the banner's
  `model device:` line.

**`outrig` (public API -- the reason this needs a version plan).** All of the
following are in `crates/outrig/public-api.txt`: `LlmProvider::Mistralrs`,
`MistralrsDeviceSpec` (+ `EXPECTED`, `FromStr`, `Display`),
`MistralrsDeviceParseError`, the six `Model` weight fields (`model_id`,
`model_path`, `model_file`, `revision`, `context_length`, `device`),
`Config::model_cache_root`, and **nine** `ConfigValidationError` variants
(`Mistralrs*` x7, `RemoteModelHasMistralrsField`, `ModelCacheRootNotAbsolute`).
Also `Model::mistralrs_weight_fields()` and its use in `provider_shape_fields()`,
plus the `validate_mistralrs_model` dispatch in `config/validate.rs`.

`LlmProvider`, `ConfigValidationError`, `Model` and `Config` are all
`#[non_exhaustive]`, so per Rust's rules variant/field removal is not *formally*
breaking -- but any downstream code that names `LlmProvider::Mistralrs` or reads
`Model::model_id` stops compiling, so treat it as breaking in practice and pick
the version deliberately.

## The decision this entry does not make

**Does `style = "mistralrs"` become a parse error, or keep parsing as a no-op?**
Both are defensible and they are very different for users:

- **Parse error.** Honest, and consistent with `deny_unknown_fields` elsewhere.
  Every committed config declaring a mistralrs provider stops loading -- including
  ones whose *agents never used it*, since providers validate independently of
  whether anything resolves to them.
- **Keep parsing, fail at resolve.** Exactly today's feature-off behavior, whose
  rationale (`doc/reference/config.md`, "Always parses, even without
  `--features local-llm`") is portability of a shared checked-in config. A repo
  config keeps loading for teammates; only naming such a model fails.

The second is a much softer landing and the machinery already exists and is
already tested. The cost is carrying the six weight keys and the nine validation
variants for another cycle -- which is most of the public-API surface above, so
choosing it means the API cleanup does *not* happen in the same release.

## Related entries to resolve together

- `plan/next/mistralrs-provider-swallows-keys.md` -- proposes reshaping
  `Mistralrs` to a braced variant so `deny_unknown_fields` bites. **If the variant
  is being removed, do not do this work**; close it instead. It currently reads as
  live work on a deprecated surface.
- `plan/next/model-path-runtime-unjoined.md` -- a real bug (relative `model-path`
  validated against the repo root, loaded against cwd) in code scheduled for
  deletion. Probably closes unfixed; say so rather than leaving it to look
  outstanding.
- `plan/next/ci-configuration-coverage.md` -- its highest-value item is a
  `macos-latest` x `local-llm,metal` job, and it notes the macOS-only dependency
  block hardcodes `candle-core`/`mistralrs-core` pins instead of inheriting from
  `[workspace.dependencies]`. Both evaporate with the feature; the *rest* of that
  entry (a `cargo hack --each-feature` job) is independent and still wanted.
- `plan/next/streaming-path-has-no-http-retry.md` -- see above.
- `plan/next/macos-host-support.md` -- mentions `metal` as motivation; re-read.

## Deliverables (sketch)

- Delete the two source files, the two test files, and every `local-llm` /
  `cuda` / `metal` cfg site; drop the three features from
  `crates/outrig-cli/Cargo.toml` and the `[target.'cfg(target_os = "macos")']`
  block; drop `mistralrs-core`, `hf-hub`, `candle-core` and `indexmap` from
  `[workspace.dependencies]` (verified at deprecation time: `indexmap` has no
  other consumer in either crate).
- Drop the `build.rs` deprecation warning and the two cuda/metal warnings.
- Remove the CI `local-llm` matrix row (`.github/workflows/ci.yml`).
- Execute whichever config-surface decision is taken above, and regenerate
  `crates/outrig/public-api.txt` if the library surface moves.
- `doc/concepts/in-process-llm.md` goes away as a chapter. **Keep the migration
  recipe** -- move it into `doc/concepts/llm-providers.md` as a
  "pointing at a local server" section, since that is the surviving answer and
  the deprecation banners all point at it. Drop the row from `doc/SUMMARY.md` and
  `doc/concepts/README.md`. Note `doc/reference/config.md` is a **symlink** into
  `crates/outrig-cli/src/mcp_self/docs/reference/config.md` (as are seven other
  shared docs), so it is one edit, not two -- but the embedded copy is what
  `include_str!` ships, and `mcp_self/docs.rs`'s link test resolves relative
  links, so a dangling `../concepts/in-process-llm.md` there is a real failure.
- CHANGELOG: move the `Deprecated` entry to `Removed`, and state the trust
  property being dropped rather than only the keys.

## Acceptance

- `git grep -i mistralrs` returns nothing outside `plan/done/` and CHANGELOG
  history.
- Default build unchanged; `cargo tree -p outrig-cli` loses the ML stack
  (~300 crates at deprecation time -- re-measure, do not quote this number).
- A config carrying `style = "mistralrs"` behaves as the chosen decision says,
  with a test pinning the message either way.

## Dependencies

The deprecation must have shipped in a released version first.
