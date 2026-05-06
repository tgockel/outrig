# 0038 -- Add rmcp `server` + `transport-io` features

## Goal

Enable the rmcp Cargo features needed by `ProxyServer` (0039) and the stdio
transport binding in 0040. No code changes -- this is a one-line manifest update
verified by a clean build.

## Deliverables

- `Cargo.toml` -- extend the `[dependencies.rmcp]` feature list:
  ```toml
  [dependencies.rmcp]
  version = "0.1"
  default-features = false
  features = [
      "base64",
      "client",
      "macros",
      "server",            # NEW: ServerHandler trait, serve_server
      "transport-child-process",
      "transport-io",      # NEW: rmcp::transport::stdio()
  ]
  ```
- Verify against rmcp 0.1.5: `serve_server` is gated behind `server`;
  `rmcp::transport::stdio()` is gated behind `transport-io`. No other rmcp feature
  changes.

## Acceptance

- `cargo build` clean (default features).
- `cargo build --no-default-features` -- if the crate already supports this, it
  still builds; if not, no regression.
- `cargo test` clean; no new compiler warnings.
- `cargo doc --no-deps` clean.

## Dependencies

None.

## Notes

- This is intentionally a separate task to keep the `ProxyServer` PR (0039) focused
  on code rather than mixing in a Cargo.toml change.
- If the rmcp version pins drift (a `0.2` upgrade etc.) before this task is picked
  up, re-verify the feature names against the new release; they are stable through
  0.1.x.
