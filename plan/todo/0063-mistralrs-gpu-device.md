# 0063 -- GPU / non-CPU device support for the in-process mistralrs path

## Context

`src/llm/mistralrs.rs::load` (around line 157) hardcodes
`&candle_core::Device::Cpu` when calling
`Pipeline::load_model_from_hf`. Every in-process model -- regardless of
size or quant -- runs entirely on CPU.

The performance ceiling is brutal. Real numbers measured during 0029
follow-up work on a modern AMD desktop with AVX2+FMA, no AVX-512:

- 1.5B Q4_K_M, CPU: ~15 tok/s. Borderline usable for chat.
- 7B Q4_K_M, CPU: ~3-5 tok/s. Unusable for chat.
- 13B Q4_K_M, CPU: ~1-2 tok/s. Effectively offline.

Most laptops and workstations now ship with a CUDA-capable NVIDIA, an
Apple-Silicon Metal target, or a discrete AMD ROCm card. Even a modest
GPU is 10-100× faster than CPU on the same model. The in-process path
becomes genuinely useful only when those devices are reachable.

`candle-core` already exposes `Device::Cuda(_)`, `Device::Metal(_)`,
and (via feature flags) Vulkan-capable backends. mistralrs-core
forwards through; the work is in outrig's wrapper plus the build
matrix.

## Goal

Add explicit non-CPU device selection for mistralrs-backed models so users can run
the in-process backend on CUDA or Metal builds without changing CPU-only defaults.

## Goals and non-goals

**In scope:**

- A new `device` knob on the mistralrs model config (or provider
  config; see Open Questions below). Accepts `cpu`, `cuda`, `metal`,
  with `cuda:N` selecting a specific GPU index.
- `Default` is `cpu` -- nobody loses behavior on upgrade.
- Build-time feature gates: `mistralrs-cuda`, `mistralrs-metal`. Each
  is additive on top of `mistralrs` and brings in candle's matching
  feature.
- Validation: pick `cuda` without `mistralrs-cuda` compiled in -> the
  same kind of friendly "rebuild with --features ..." error
  `MistralrsFeatureDisabled` produces today.
- Update CI matrix to exercise at least the `cuda` feature on the GPU
  runner (if one exists; document the gap if not).

**Out of scope:**

- Auto-detection of available devices. v1 makes the user state intent.
  Auto-select can come later.
- Multi-GPU sharding. mistralrs-core supports it via
  `DeviceMapSetting::Auto`; that's already what `load` passes today
  (`AutoDeviceMapParams::default_text()`). Keep using it; this task
  just opens the door to *non-CPU* devices, not specifically multi-GPU.
- ROCm/AMD GPU. candle's ROCm support is less mature; defer to a
  follow-up. Document the gap; don't pretend.
- Quant conversions (e.g. F16 vs Q8 on GPU). The GGUF on disk is what
  loads; mistralrs handles the device transfer.

## Approach sketch

1. **Config**: add `pub device: Option<String>` to `Model` in
   `src/config/mod.rs` (mistralrs-only field; validation rejects it on
   openai-style models, like the existing weight fields). Accept the
   string forms `cpu`, `cuda`, `cuda:N`, `metal`. Parse to a small
   internal `enum DeviceSpec { Cpu, Cuda(usize), Metal }` at resolve
   time.
2. **Cargo features**:
   ```toml
   mistralrs-cuda  = ["mistralrs", "candle-core/cuda",  "mistralrs-core/cuda"]
   mistralrs-metal = ["mistralrs", "candle-core/metal", "mistralrs-core/metal"]
   ```
   (Confirm the exact `mistralrs-core` feature names against the 0.8.1
   manifest; the candle ones are stable.)
3. **Loader**: `load(...)` learns to map `DeviceSpec` to a real
   `candle_core::Device`. Wrap construction in `#[cfg(feature = ...)]`
   blocks so a non-feature build that somehow gets `cuda` returns the
   same friendly "rebuild with --features mistralrs-cuda" error.
4. **Validation**: a new `LlmResolveError::MistralrsDeviceUnavailable {
   model, device, build }` mirroring `MistralrsFeatureDisabled`.
5. **Doc**: `doc/concepts/in-process-llm.md` and
   `doc/reference/config.md` get a section on the `device` field plus
   the new feature flags.

## Deliverables

- `src/config/mod.rs` -- new `device` field on `Model`.
- `src/config/validate.rs` -- reject `device` on openai-style models;
  accept-list the four string forms.
- `src/llm.rs` -- thread the parsed `DeviceSpec` through
  `MistralrsWeights` and into `load`.
- `src/llm/mistralrs.rs` -- accept the device, construct the candle
  `Device`, pass to `load_model_from_hf`.
- `Cargo.toml` -- new `mistralrs-cuda` / `mistralrs-metal` features.
- `doc/concepts/in-process-llm.md`, `doc/reference/config.md` -- field
  + feature documentation.
- `.github/workflows/*.yml` (or wherever CI lives) -- add at least a
  build-only matrix entry for `--features mistralrs-cuda` so the
  feature flag stays buildable.
- `tests/config_provider_enum.rs` -- new validation test cases.

## Acceptance

- A config with `device = "cuda"` loads on a `--features
  mistralrs-cuda` build with a CUDA GPU present, and the model runs at
  GPU-class tok/s (verifiable via streaming -- depends on
  `streaming-mistralrs-output.md`).
- The same config on a non-CUDA build produces a friendly error at
  resolve time, not a panic at load time.
- `cargo build --features mistralrs` (no GPU feature) still produces
  a working CPU-only binary.

## Open questions

- **Device on model vs provider?** The provider lookup is lighter (one
  per `style = "mistralrs"` provider, shared across models). But
  different models may want different devices -- a small filter on
  CPU, a big coder on GPU. Argues for *model*. Match how
  `model-id` / `model-file` already live (post-0029).
- **Vulkan**: candle has experimental Vulkan support. Worth including
  in v1 or defer? Probably defer; CUDA + Metal cover ~95% of
  developers in our target audience.
- **Falling back from CUDA to CPU on init failure**: silent fallback
  is treacherous (user thinks they're on GPU, isn't). Loud failure
  with the exact reason (`cuda init: <err>`) is the v0 stance.

## Notes

- This is a substantial task. ~3-5 days end-to-end including doc and
  CI work, not counting actual GPU validation.
- Pairs naturally with `streaming-mistralrs-output.md` -- without
  streaming, the user can't *feel* the GPU speedup until the response
  is done.
- Don't gate this on Ollama-as-OpenAI-provider being viable. Some
  users want truly in-process for the policy-oracle use case
  documented in `doc/concepts/in-process-llm.md` -- the same use case
  motivates running on a fast device.

## Dependencies

- **Hard: 0062**. Streaming is needed first so GPU-class decode speed is visible
  during normal `outrig run` usage and the GPU acceptance path can verify token
  output incrementally.
