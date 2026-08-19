# Rune harness prototype

This isolated crate contains two scenario-specific proof binaries:

- `rune-harness-scripted`: the original deterministic, provider-free vertical proof.
- `outrig-harness`: a real-model interactive CLI using OutRig's existing provider, model, agent, retry, history, and tool-call-limit runtime.

The real CLI has **no Podman, image, sidecar, or MCP startup path**. Its only model-visible tool is `execute_rune`. That host tool can read only the one canonicalized `--file` beneath the canonicalized `--repo`; the file is read once into the persistent Rune `source` value and later calls reuse that value. Provider-visible tool results are capped at 16 KiB.

## Temporary internal dependency

`Cargo.toml` intentionally depends on the sibling `outrig-cli` crate with feature `internal-test-api`. This is prototype-only and not a SemVer-supported library API. It reuses `resolve_agent_with_overrides`, `build_agent`, `RigAgent`, and `SessionTool` instead of duplicating provider clients or the agent loop. No existing `outrig` or `outrig-cli` source/behavior is changed.

## Run

From the OutRig repository root:

```sh
cargo run --manifest-path prototypes/rune-harness/Cargo.toml \
  --bin outrig-harness -- \
  --repo . --file crates/outrig/src/lib.rs \
  [--agent NAME] [--model NAME]
```

`--file` is required. `--repo` defaults to the current directory. `--agent` defaults to `default-agent` from config; `--model` overrides the selected agent/config model. The CLI loads `.agents/outrig/config.toml` through `outrig::load_project` and requires `--repo` to be that configured repository root.

A minimal OpenAI-compatible configuration follows; the complete schema and keys are documented in the repository's `doc/reference/config.md` and by the public `outrig::config` API.

```toml
default-agent = "harness"

[providers.hosted]
style = "openai"
base-url = "https://api.openai.com/v1"
api-key = "OPENAI_API_KEY"

[models.fast]
provider = "hosted"
identifier = "gpt-4.1-mini"

[agents.harness]
model = "fast"
preamble = "Be concise and verify claims with the provided capability."
tool-call-max = 12
```

Export the API-key environment variable named by `api-key` before launch (for the example, `OPENAI_API_KEY`). Anthropic-style providers and configured model aliases work through the same existing OutRig resolver/runtime. No live provider call is part of automated tests.

Each nonempty stdin line is one user turn. The process keeps one Rig message history and one Rune invocation scope across turns. EOF exits.

## Supported Rune scenario

The model preamble documents these accepted forms (ordinary whitespace and numeric slice ranges are tolerated):

```rune
println!("{}", doc(fs));
let source = fs.read(path).await?; println!("{}", source);
println!("{}", source[START..END]);
```

Everything else is rejected with a recoverable, model-visible error naming the accepted forms. Results include attempted/captured bytes, truncation, and retained binding inventory.

## Verification

```sh
cargo fmt --manifest-path prototypes/rune-harness/Cargo.toml -- --check
cargo test --manifest-path prototypes/rune-harness/Cargo.toml
cargo run --manifest-path prototypes/rune-harness/Cargo.toml --bin rune-harness-scripted
cargo check --manifest-path prototypes/rune-harness/Cargo.toml --bin outrig-harness
```

## Manual real-model test

1. Configure a real provider/model/agent and export its API-key variable.
2. Launch the command above.
3. Ask: `Read the selected file once, report its byte length, then show bytes 100 through 200.`
4. Send a second terminal line: `Now inspect bytes 300 through 350 without reading the file again.`
5. Confirm tool traces show `execute_rune`, the second turn sees prior conversation, and retained inventory continues to list `source` while the host read count remains one (also covered by deterministic tests).

## Limitations

- Prototype, Linux-only indirectly because it links the current OutRig crates.
- UTF-8 text files only; Rune string slices use byte indices and invalid UTF-8 boundaries can fail.
- Exactly one selected regular file and three transformer shapes; no general Rune evaluator.
- A file changed or removed after CLI startup can make the first read fail; provider/config failures remain CLI errors.
- Real provider behavior, credentials, cost, and network availability are intentionally outside automated tests.
