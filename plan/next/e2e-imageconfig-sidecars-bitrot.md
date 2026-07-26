# Fix e2e-gated test bit-rot: `ImageConfig { sidecars }`

`cargo test --features e2e` (and `cargo clippy --all-targets --features e2e`) fail to compile:
three test-only `ImageConfig { .. }` literals still set a `sidecars` field, but 0088 moved
sidecars to the top-level `Config.sidecars`. CI never compiles the `e2e` feature (only `default`
and `local-llm`), so the rot went unnoticed.

Sites (all `E0560: struct ImageConfig has no field named sidecars`):

- `crates/outrig/tests/network_interceptor.rs:50`
- `crates/outrig/tests/embedded_image.rs:78`
- `crates/outrig/tests/embedded_image.rs:292`

Fix: drop the stale `sidecars: ...` field from each literal (top-level sidecars are set on
`Config`, not `ImageConfig`). Mechanical.

Discovered while landing 0090, whose own gated e2e (`primary_view_e2e.rs`) compiles fine and is
runnable in isolation with `cargo test -p outrig-cli --features e2e --test primary_view_e2e`.
Left out of 0090's commit to keep it scoped; the whole e2e suite only compiles again once these
three are fixed.

Consider also a CI job that at least *compiles* the e2e suite (`cargo test --features e2e
--no-run`) so this class of rot is caught without needing podman.
