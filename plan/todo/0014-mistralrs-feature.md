# 0014 -- `mistralrs` feature flag and CI matrix

## Goal

Add a `mistralrs` Cargo feature, the optional crate dep behind it, an empty
`#[cfg(feature = "mistralrs")]` module, and the CI matrix entry that exercises both
configurations. This task adds **no runtime behavior** -- it's the build-system scaffolding
that lets 0015 attach the actual shim. After this task, `cargo build` and `cargo build
--features mistralrs` both succeed; the latter just compiles a heavier dep tree.

## Deliverables

- `Cargo.toml`:
  ```toml
  [features]
  default = []
  mistralrs = ["dep:mistralrs"]   # exact crate name pinned at task time

  [dependencies]
  mistralrs = { version = "...", optional = true, default-features = false }
  ```
  Crate variant (`mistralrs`, `mistralrs-core`, etc.) and exact version pin chosen at task
  time -- see notes. Pin tightly; the crate is pre-1.0. Default-off because the crate is
  heavy enough that users running outrig only against an OpenAI-compatible HTTPS endpoint
  shouldn't pay for the dep tree they won't touch.
- `src/llm.rs` (or `src/llm/mod.rs`) gains a `mistralrs` submodule under
  `#[cfg(feature = "mistralrs")]`. Empty for now; 0015 fills it. The non-feature path
  has no equivalent module -- the resolver handles the "not built with this feature" case
  uniformly.
- A `feature_off_explains_clearly` smoke check: the resolver in 0012 already produces
  some error for `style = "mistralrs"` when feature is off; here, harden the message:
  `mistralrs provider 'name' requested but this build of outrig does not include the
  'mistralrs' feature; rebuild with --features mistralrs to enable`.
- `.github/workflows/ci.yml` (or wherever 0026 lives -- this task may predate it; if so,
  drop a follow-up note in `plan/next/`):
  - Existing job: `cargo test` (default features off, mistralrs absent) keeps passing.
  - New job: `cargo test --features mistralrs` -- builds the dep, runs the test suite.
    Tests that need a real model file are gated behind an env var (set up in 0015), so
    this job runs the unit suite only.

## Acceptance

- `cargo build` (no features) succeeds.
- `cargo build --features mistralrs` succeeds.
- `cargo test` succeeds in both configurations.
- The "feature off but config asks for mistralrs" error message matches the wording
  pinned above and is covered by a regression test.

## Dependencies

- 0013-llm-provider-enum

## Notes

### Why `mistralrs`

`mistralrs` is the chosen backend because it's a Rust-native inference engine with
native support for GGUF, ISQ-quantized models, tool calling, and (the important one)
JSON-schema-constrained sampling -- the last of which matters for the eventual
policy-oracle path even though that path ships with a later feature.

Rejected alternatives, in order of how often they're likely to be re-proposed:

- **`candle`.** HuggingFace's pure-Rust inference primitives. Lower-level than what we
  need: we'd own the inference loop, sampler, KV cache, chat templating, and
  grammar-constrained sampling. More code to write and maintain than the policy-oracle
  use case justifies.
- **`llama-cpp-rs` / `llama_cpp_2`.** Thin FFI wrappers over `llama.cpp`. Pulls a C++
  build step (cmake + a C++ compiler), and the Rust API is a moving target. Mature
  GGUF support and a well-tested GBNF grammar implementation, but the FFI ergonomics
  and build dependencies weigh against it for a Rust-first project.
- **Ollama (or any localhost server) as a sidecar.** Rejected by the use case itself
  (see `doc/concepts/in-process-llm.md` "Why in-process and not localhost"). Worth
  naming explicitly so it isn't reproposed.
- **Capability-named feature flag (`in-process-llm`).** Considered as a way to hide
  the backend from the user. Rejected because future in-process backends (a `candle`
  variant, perhaps) would each need their own native dep tree; an umbrella flag
  becomes misleading. Implementation-named (`mistralrs`) is more honest, and the user
  picks the backend explicitly via `style = "mistralrs"`.

### Build / CI specifics

- Verify before pinning: a CPU-only `mistralrs` build genuinely doesn't pull a C++
  toolchain or BLAS dep. The crate has feature-gated paths for BLAS / CUDA / Metal; we
  want `default-features = false` plus whatever minimal CPU feature the crate exposes
  (sometimes named `cpu`, sometimes just absent). If the default features pull a heavy
  native dep, change `default-features = false` and select an explicit minimal set.
- Crate pick (`mistralrs` vs `mistralrs-core`): the high-level facade is convenient but
  may pull HTTP/server scaffolding we don't want. Inference primitives (`mistralrs-core`)
  are leaner. Try the high-level first; fall back to the core crate if the dep tree is
  bigger than expected.
- The HF download client also gets picked here implicitly: if `mistralrs` ships a
  built-in downloader we like, use it. If not, add `hf-hub` as an additional optional
  dep gated on the same feature flag, so the dep tree shape stays internal to the
  feature.
- This task interleaves with 0026-ci. If 0026 hasn't landed yet, the CI matrix entry can
  be a follow-up note; the local `cargo build`/`cargo test` checks are the binding ones.
