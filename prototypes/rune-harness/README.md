# Event-driven Rune agent prototype

This crate replaces the former chatbot/tool-loop CLI with a long-lived, event-driven agent driver. The persistent entity is host state: Rune binding values, capability definitions, filesystem read accounting, the event queue, and bounded observations. **There is no persistent `Vec<rig::Message>` and the CLI never calls `RigAgent::run_turn`.**

```text
independent stdin task ──ExternalEvent::UserInput──▶ AgentDriver
                                                     │
       ┌──────────── Idle ◀──── Decision::Emit ◀── Activating
       │                         │                    │ fresh, history-free
       │                         └── output channel   │ completion
       │                                              ▼
       └── next input                    Decision::ExecuteRune
                                                     │
                                                     ▼
                                     Running owned async Rune VM
                                      │                    │
                         waiter exists│                    │no waiter
                                      ▼                    ▼
                              satisfy oneshot       drop/cancel run
                              same VM resumes       exact event → activation
                                      │
                                      ▼
                           bounded RuneObservation → activation
```

`Decision` is exactly `ExecuteRune { source }` or `Emit { text }`. Only `Emit` reaches the user-facing output channel. Emit returns the driver to Idle; process lifetime and terminal EOF own termination.

## Architecture

* `ModelBackend::activate(ActivationRequest) -> Decision` is fresh for every activation. `ActivationRequest` contains stable instructions, the exact cause, one bounded observation/diagnostic, capability summaries, and retained-binding inventory—never provider transcript/history.
* `RealModel` reuses OutRig's config resolution, provider construction, retries, failover, and token ceilings through temporary `outrig-cli` internal APIs. Its Rig agent is built with **no tools**. `RigAgent::activate_text_once` performs one prompt with no supplied history; the host parses the tagged decision JSON. There is no Rig tool loop.
* `ScriptedModel` records every request for deterministic tests.
* Each Rune snippet is separately compiled to an async entrypoint. Host binding names are predeclared with `Statics`; a fresh unit-specific `Globals` is seeded by name, then extracted by name after completion. Values remain Rune `Value`s and are not serialized into model context.
* A bounded transformer promotes top-level `let IDENT = expression;` to assignment into a persistent root static. It handles arbitrary ASCII Rune identifiers and skips strings, line comments, and nested delimiters. This is intentionally a prototype transformer rather than a complete Rune AST rewrite.
* `events::next().await` installs one oneshot waiter before yielding. Tests wait on its registration notification rather than sleeping. Cancellation drops the native future and clears the epoch-matched waiter.
* `FileSystem` is read-only. A persistent global `fs` value is seeded into every unit; `fs.read(relative_path)` synchronously canonicalizes beneath `--repo`, rejects traversal, symlink escape and non-files, requires UTF-8, and tracks reads. The model—not a CLI `--file`—selects paths.
* `FILE_SYSTEM` is the canonical capability definition used both for Rune trait metadata and Python-like dynamic model documentation. `doc(fs)` renders `trait FileSystem` and its actual synchronous `fn read(path: String) -> String` signature inside Rune.
* `Context::with_config(false)` disables normal stdio. Replacement `std::io::{print,println}` hooks retain at most 16 KiB including the metadata suffix and report attempted/captured/truncated bytes plus binding inventory. `preview(value,start,end)` slices a retained String before formatting.

## Rune dependency

This is deliberately pinned to unreleased Rune 0.15 main commit:

```text
1a423577a9813406043bea15da25714eccadf37d
```

That Git revision supplies `Statics`/`Globals`, which released Rune 0.14 cannot provide for general bare persistent bindings across separately compiled units.

## Run

From the OutRig repository root (there is no `--file`):

```sh
cargo run --manifest-path prototypes/rune-harness/Cargo.toml \
  --bin outrig-harness -- --repo . [--agent NAME] [--model NAME]
```

`--repo` defaults to the current directory and must be the configured project root. The CLI loads `.agents/outrig/config.toml`. `--agent` defaults to `default-agent`; `--model` overrides its model.

Example configuration:

```toml
default-agent = "harness"

[providers.hosted]
style = "openai"
base-url = "https://api.openai.com/v1"
api-key = "OPENAI_API_KEY"

[models.fast]
provider = "hosted"
identifier = "gpt-4.1-mini"
max-tokens = 4096

[agents.harness]
model = "fast"
preamble = "Investigate with Rune before making unsupported claims."
```

Export the environment variable named by `api-key` (here `OPENAI_API_KEY`) before launch. Anthropic providers and model aliases/failover use the existing OutRig resolver and runtime. No Podman, image, MCP, or sidecar is started.

## Manual real-model scenario

1. Configure credentials, then run the command above.
2. Enter: `First run println!("{}", doc(fs)); then read Cargo.toml once with fs.read("Cargo.toml") into a retained binding named manifest_source and report its package name.`
3. Enter: `Without rereading, use preview(manifest_source, 0, 120) and summarize that range.`
4. Ask it to run `let followup = events::next().await;` and then type a second line; the same Rune execution receives that event and resumes.
5. Confirm model-facing observations stay under 16 KiB and inventory shows `manifest_source`/`followup`.

## Verification

```sh
cargo fmt --manifest-path prototypes/rune-harness/Cargo.toml -- --check
cargo test --manifest-path prototypes/rune-harness/Cargo.toml
cargo run --manifest-path prototypes/rune-harness/Cargo.toml --bin rune-harness-scripted
cargo check --manifest-path prototypes/rune-harness/Cargo.toml --bin outrig-harness
```

Tests cover fresh activations, scope across Units and arbitrary names, one-read large-file retention plus bounded preview, waiter-synchronized awaited events, exact-cause unawaited cancellation with a drop counter, path/symlink boundaries, Emit/Idle behavior, and the total output cap.

## Honest limitations

* **Unreleased Git dependency:** Rune API stability is not promised until a release.
* **Cooperative drop cancellation:** interruption drops the owned Rune execution/native futures; there is no Rune exception injection. CPU-bound code that never yields can monopolize this current-thread prototype (production should drive budgeted VM slices).
* **No rollback:** cancellation is commit-as-executed. Host/native side effects already performed remain. In this prototype host bindings are extracted after successful completion, so an interrupted unit's in-VM static writes are not surfaced; filesystem reads already performed still count.
* **Bounded retention, not bounded temporary formatting:** output retained and sent to the model is <=16 KiB, but Rune's normal `println!` formats a complete temporary String before the sink sees it. Prefer `preview` for large retained values.
* **Prototype transformer:** only simple top-level `let name = value` promotion is supported; destructuring, attributes, and full AST-preserving rewriting are not.
* **Temporary CLI-internals dependency:** `outrig-cli`'s `internal-test-api` and `activate_text_once` are a prototype seam, not a stable library API. JSON schema is stated in the prompt and strictly deserialized; Rig/provider-native typed structured output is not uniformly used because the shared failover enum abstracts heterogeneous concrete model types.
* One active `events::next` waiter is supported. External events are ordered and never silently coalesced.
* UTF-8 files only. `preview` ranges are byte offsets and invalid UTF-8 boundaries yield an empty preview.
