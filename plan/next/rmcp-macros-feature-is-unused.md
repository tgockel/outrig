# rmcp's `macros` feature is switched on and never used

## Symptom

Both crates list `macros` among their rmcp features (`crates/outrig/Cargo.toml`,
`crates/outrig-cli/Cargo.toml`), and neither uses a macro from it: no `#[tool]`, `#[tool_router]`,
`#[tool_handler]`, or `#[prompt*]`, and no `rmcp::object!`. `crates/outrig/src/mcp_proxy.rs` says
in its module docs that the proxy avoids `#[tool]` on purpose, since its tool set is unknown until
startup. None of the other rmcp features either crate enables implies `macros`.

The feature still builds `rmcp-macros` and the `darling` family behind it in every configuration.
Since the rmcp 3.4 bump (#191) that is `darling` 0.24, while `serde_with_macros` under `local-llm`
still holds `darling` 0.23, so that row compiles darling twice.

## Where it goes

Drop `"macros"` from both feature lists; `pastey` stays in the graph, because `server` pulls it in.
rmcp is public through `outrig::mcp_proxy`, so a downstream crate that leaned on outrig to switch
`rmcp/macros` on would have to name the feature itself. That wants a line in
`crates/outrig/CHANGELOG.md`, and is why this did not ride along with #191.
