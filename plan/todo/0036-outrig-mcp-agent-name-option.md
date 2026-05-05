# 0036 -- Refactor: `Session::agent_name` -> `Option<String>`

## Goal

Make the session row honest about a session that has no agent -- `outrig mcp` will
write rows with no agent attached. A `None` value is meaningfully distinct from any
sentinel string ("-", "", "none") and lets `outrig ls` / `outrig logs` format
agent-less sessions without special-casing magic strings.

## Deliverables

- `src/session.rs`:
  ```rust
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub agent_name: Option<String>,
  ```
- Update writer site in `src/cli/run.rs` (currently `agent_name:
  resolved.agent_name.clone()`) to `agent_name: Some(resolved.agent_name.clone())`.
- Update display sites (e.g. `src/cli/ls.rs`) to render
  `session.agent_name.as_deref().unwrap_or("-")` (or equivalent).
- Update `tests/common/mod.rs` builder (currently
  `agent_name: "default".into()`) to `agent_name: Some("default".into())`.
- After the change, `grep -rn "agent_name" src/ tests/` returns only
  `Option<String>`-aware uses (no bare `String` indices, no `.as_str()` without an
  `unwrap_or`).
- On-disk JSON compatibility: `#[serde(default,
  skip_serializing_if = "Option::is_none")]` means existing rows
  (`"agent_name": "coding"`) still parse, and absent fields default to `None`. New
  rows from `outrig mcp` simply omit the field.

## Acceptance

- `cargo build` clean; `cargo test` clean.
- `cargo test --features e2e` continues to pass; the `tests/runtime_user.rs` and other
  e2e suites that look at session rows accept the new shape.
- Grep gate passes.
- A JSON file with `"agent_name": "foo"` from an older outrig still loads.
- A JSON file with `agent_name` omitted loads with `None`.

## Dependencies

None.

## Notes

- Independent of 0035 -- they touch different modules and can interleave. Numbered in
  this order purely because the master spec listed them sequentially.
- Display formatting policy ("-" placeholder for `None`) is the recommended default;
  the task author may pick a different placeholder if it reads better in
  `outrig ls`'s columnar output.
