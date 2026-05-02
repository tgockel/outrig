# 0001 -- Cargo skeleton

## Goal

Wire the Cargo dependencies and module skeleton so every later task has a place to land. After
this, `outrig --help` lists every documented subcommand even though none of them work yet.

## Deliverables

- `Cargo.toml` dependencies:
  - CLI: `clap` 4 with `derive`
  - Async runtime: `tokio` with features `rt-multi-thread`, `macros`, `io-util`, `process`,
    `signal`, `fs`, `time`
  - Serde: `serde` (derive) + `toml` + `serde_json`
  - Errors: `anyhow` + `thiserror`
  - Logging: `tracing` + `tracing-subscriber` (`env-filter`, `fmt`)
  - Hashing: `blake3`
  - Filesystem: `tempfile`, `directories`, `walkdir`, `ignore`
  - Interactive: `dialoguer`
  - LLM: `rig-core` (pinned to a known-working version)
  - MCP: `rmcp` with features `client` and `transport-child-process`
  - TOML editing (preserves surrounding content): `toml_edit`
- `[features]` block with an `e2e` feature flag (no deps; tests gate on it).
- `src/lib.rs` re-exporting empty modules: `cli`, `config`, `repo`, `process`, `image`,
  `container`, `mcp`, `rig_tool`, `llm`, `repl`, `session`, `init`, `error`. Each starts as a
  one-line file with a doc comment.
- Move `src/main.rs` -> `src/bin/outrig.rs`; delete `src/main.rs`.
- `src/bin/outrig.rs` with a `clap` derive `Cli { #[command(subcommand)] cmd: Cmd }` enum
  stubbing every documented subcommand: `Run`, `Build`, `Init`, `InitContainer`, `Ls`, `Logs`,
  `Discard`. Each handler prints `error: not implemented` to stderr and exits with code 1.
- `src/error.rs` with a `#[derive(thiserror::Error, Debug)]` enum `OutrigError` and a
  `pub type Result<T> = std::result::Result<T, OutrigError>` alias.
- `tracing_subscriber::fmt().with_env_filter(...).init()` in `main()`, honoring `OUTRIG_LOG`.

## Acceptance

- `cargo build` succeeds with zero warnings.
- `cargo run -- --help` lists every subcommand from `doc/reference/cli.md` (`run`, `build`,
  `init`, `init-container`, `ls`, `logs`, `discard`).
- `cargo run -- run` exits with code 1 and prints `error: not implemented` on stderr.
- `OUTRIG_LOG=debug cargo run -- run 2>&1 | grep -q DEBUG` succeeds (tracing is wired).

## Dependencies

None. This is the first task.

## Notes

- Pin every dependency to a major.minor (e.g. `clap = "4.5"`); tighten to exact versions only
  if a regression forces it.
- Don't add any business logic in this task. Modules are empty stubs.
- Mention in the commit message that any v0 deps may yet shift as later tasks discover what
  rig-core/rmcp actually expose.
