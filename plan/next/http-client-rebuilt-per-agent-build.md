# Every agent build reloads the system trust store

## Context

`remote_http_client` (`crates/outrig-cli/src/llm.rs:538`) calls
`reqwest::Client::builder().timeout(..).build()` fresh on every invocation, and `build_agent`
calls it on both remote arms (`llm.rs:570` for OpenAI, `llm.rs:598` for the other hosted
provider). `build_agent` runs once per session start (`cli/run.rs:328`), once per subagent
launch (`subagent/mod.rs:738`), once per REPL rebuild (`cli/run.rs:967`), and once more on the
model-swap path (`llm.rs:885`).

`outrig-cli` pins reqwest to `features = ["json", "rustls"]` (`crates/outrig-cli/Cargo.toml:57`),
and building a rustls-backed client parses the whole system trust store. So each of those calls
re-reads and re-parses every certificate in `/etc/ssl/certs`.

Surfaced while landing 0109, whose tests launch the full 72-subagent tree. `strace -e openat`
over `full_idle_tree_shuts_down_within_the_grace`:

```
17881  openat total
17784  /etc/ssl/certs/...      (~247 files x 72 launches)
  144  /etc/ssl/certs/ca-certificates.crt
```

That is ~40 ms per subagent launch, and it scales linearly with the tree: the two 0109 tests
take ~3 s each, against the ~5 ms quantity they exist to measure, and the pre-existing
two-subagent `shutdown_reaps_nested_subagents_and_releases_their_tool_clones` runs in 0.09 s.

## Why it is worth fixing

The test cost is the visible symptom, not the reason. A user launching eight subagents pays the
same tax eight times on a path where nothing about the client varies: the only inputs are
`request_timeout_secs` and the retry policy, both of which come from the resolved agent. Session
startup pays it once more before the first token.

`reqwest::Client` is `Arc`-backed and documented as cheap to clone and intended to be reused --
reusing one also lets the connection pool survive across subagent launches, which the current
shape discards.

## Shape

Build the client once and clone it. Two candidate seams:

- Hang it off `SubagentContext`, beside `mcp_tools` and `resolved`, so the whole subagent tree
  shares the session's client. Fits the existing context-carries-shared-state pattern, but does
  not help `cli/run.rs`'s own two call sites.
- Memoize inside `remote_http_client`, keyed on `(request_timeout_secs, RetryPolicy)`. Narrower
  and covers every caller, but needs `RetryPolicy` to be hashable or comparable -- note 0113 is
  already scheduled to touch `RetryPolicy` (it prices `RetryPolicy: Copy`), so the two should be
  sequenced rather than landed blind.

The second is probably right, but the interaction with 0113 needs settling first.

## Not in scope

0109 deliberately left this alone: it is production behavior, and 0109 is a test-only task whose
whole point was to measure teardown rather than change the launch path. Its measurement is
unaffected either way -- the cert loading happens during setup, before the clock starts.

## See also

- `plan/done/0109-subagent-tree-shutdown-grace.md` -- where this was found.
- `crates/outrig-cli/src/llm.rs:538` -- `remote_http_client`.
- `plan/todo/0113-model-alias-failover.md` -- also touches `RetryPolicy`.
