# 0003 -- Config schema

## Goal

Type the config schema as Rust structs, with serde round-trip against the example configs in
`doc/reference/config.md`. No merge or validation yet -- pure parsing only.

## Deliverables

- `src/config/mod.rs` with the following structs (all
  `#[serde(deny_unknown_fields, rename_all = "kebab-case")]`):
  - `Config` -- top-level: `default-container`, `default-agent`, `default-model`,
    `session-root`, `[providers]`, `[models]`, `[agents]`, `[workspace]`, `[containers]`.
    The maps are `BTreeMap<String, ...>` keyed on entry name.
  - `LlmProvider { style: String, base_url: String, api_key: String, request_timeout_secs:
    Option<u64> }` (api_key stays a `String` here; 0004 swaps it for `ApiKeyRef`).
  - `Model { provider: String, identifier: String }`.
  - `Agent { model: Option<String>, container: Option<String>, preamble: Option<String>,
    temperature: Option<f32>, max_tokens: Option<u32> }`.
  - `Workspace { host_path: PathBuf, container_path: PathBuf }` with serde defaults `"."` and
    `"/workspace"`.
  - `ContainerConfig { dockerfile: PathBuf, context: PathBuf, build_args: BTreeMap<String,
    String>, mcp: BTreeMap<String, McpServerSpec> }`.
  - `McpServerSpec` as a `#[serde(untagged)]` enum:
    ```rust
    pub enum McpServerSpec {
        Short(Vec<String>),
        Full { command: Vec<String>, #[serde(default)] env: BTreeMap<String, String> },
    }
    ```
    with a `normalize(self) -> (Vec<String>, BTreeMap<String, String>)` helper.
- `Config::load_from_str(s: &str) -> Result<Config>` -- pure parsing only.
- `tests/fixtures/config-full.toml` mirroring `doc/reference/config.md`'s "Full example" pair
  flattened into one TOML file (since this task doesn't merge; the fixture is one file
  containing every legal section).
- `tests/config_schema.rs`:
  - Round-trip the fixture: parse, serialize, parse again, structural equality.
  - Reject an unknown top-level key with a serde error pointing at the offending key.
  - Both `["bin", "arg1"]` short form and `{ command = ["bin", "arg1"] }` full form parse to
    `McpServerSpec::Short` and `McpServerSpec::Full` respectively, both normalize to the same
    `(command, env)` tuple.

## Acceptance

- `cargo test config_schema` passes.
- An unknown top-level key (`oops = "..."`) rejected at parse time with a clear message.
- Both MCP entry shapes parse correctly.
- Every key in the fixture matches what's documented in `doc/reference/config.md`.

## Dependencies

- 0001-cargo-skeleton

## Notes

- `BTreeMap` over `HashMap` for deterministic iteration order in tests and error messages.
- `inner_keys` (Dockerfile build-args, MCP env) deliberately keep the user's casing; only the
  outrig-defined keys are kebab-cased. This is enforced by serde at the right level.
- If `inner` `env` keys end up rejected because of `rename_all = "kebab-case"` on the parent
  table, restructure as a `HashMap<String, McpServerSpec>` field whose value isn't itself a
  serde-renamed struct -- the inner map keys aren't subject to outer rename rules.
