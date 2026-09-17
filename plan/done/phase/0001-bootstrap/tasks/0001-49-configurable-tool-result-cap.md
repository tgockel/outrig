# 0049 -- Configurable per-tool-result truncation

## Context

A real run on 2026-05-05 hit a hard provider rejection:

```
litellm.ContextWindowExceededError: prompt is too long:
1321057 tokens > 1000000
```

A single tool call -- almost certainly `cat`-ing a giant log or a `find`
over a deep tree -- returned ~4.5 MiB of text. Rig appended that result
to `chat_history` verbatim and re-sent the history on the next turn; the
provider 400'd. The whole turn's work was lost, with no clean way to
resume.

This is a different problem from the per-turn iteration cap (see
`configurable-tool-call-cap.md`). That cap bounds *how many* tool calls
a turn may make. This one bounds *how big* any single tool result is
allowed to be. Either cap can be hit independently; the fixes are
orthogonal.

### Why we cannot fix this reactively in Rig 0.36

Three approaches were considered before settling on the adapter-side
fix:

1. **Hook-based shrinking.** Rig exposes
   `PromptHook::on_tool_result(tool_name, call_id, internal_id, args,
   result: &str) -> HookAction`
   (`rig-core-0.36.0/src/agent/prompt_request/hooks.rs:50-59`). The
   `result` parameter is `&str` (read-only) and `HookAction` is only
   `Continue` / `Terminate`. The hook *cannot rewrite* the result.
   `mod.rs:540-620` confirms: the hook fires between "compute output"
   and "construct `UserContent::tool_result(...)`," but the original
   `output` variable is what gets appended -- there is no mutation
   point exposed.
2. **Catch the 400 and recover.** `PromptError::PromptCancelled` carries
   `chat_history`, so the iteration-cap path can splice partial state
   forward. But a context-window-exceeded 400 surfaces as
   `PromptError::CompletionError(_)`, which has no partial-history
   field. The in-flight tool messages are gone by the time we observe
   the error.
3. **Take ownership of the completion loop.** Rewriting
   `run_turn_inner` to drive `agent.completion(...)` manually would
   put outrig in charge of the message buffer, retry, tool dispatch,
   and `max_turns` bookkeeping. That's a substantial rewrite of
   `src/llm.rs:run_turn_inner`; the cost outweighs the benefit when
   proactive truncation at the adapter solves the same problem.

The remaining viable path is **proactive truncation at the tool
adapter** (`src/rig_tool.rs:McpToolAdapter::call`) before the result
ever flows into Rig's history. Every tool result for the current
architecture passes through this single chokepoint, so applying the cap
here is both sufficient and complete.

## Goal

1. Bound every individual tool result's byte size at a configurable cap
   so a single oversized result cannot blow the LLM context window.
2. When truncation fires, leave a clear marker in the result that tells
   the LLM truncation happened, the original size, the cap, and a hint
   to issue a more targeted query next time.
3. Surface the cap at three layers (CLI, agent config, global config)
   with a sensible compiled-in default.

Non-goals: per-tool overrides, automatic retry-on-400-with-truncation,
hook-based reactive shrinking, manual completion-loop ownership,
token-count caps. See "Sub-decisions" and "Out of scope" below.

## User surface

```bash
# Tighten the per-result cap for this run only:
outrig run --max-tool-result-bytes 65536

# Per-agent default in repo config:
# outrig.toml
[agents.log-spelunker]
model = "claude-opus-4-7"
tool-result-cap = 524288   # 512 KiB

# Global default in the user-level config:
# ~/.config/outrig/config.toml
tool-result-cap = 262144   # 256 KiB
```

When truncation fires, the result string the LLM sees ends with a
clearly delimited marker (see "Truncation behavior" below). No stderr
line is printed -- the marker is on the result itself, where both the
LLM and the user (in the session log) can see it.

Resolution order, lowest to highest precedence:

1. The compiled-in default (`DEFAULT_TOOL_RESULT_CAP_BYTES`, 256 KiB).
2. Top-level `tool-result-cap` in config.
3. `[agents.<name>].tool-result-cap` for the resolved agent.
4. `--max-tool-result-bytes N` on the CLI.

### Default value: 256 KiB

The central trade-off is real: a default that's too low silently
truncates legitimate file reads; one that's too high loses the
protection on small-context models. Tokenizers vary, but a useful rule
of thumb for English / source code is ~3.5 bytes/token (BPE on UTF-8).
Translating:

| Cap     | Bytes     | Approx tokens | Fits in...                                              |
|---------|-----------|---------------|---------------------------------------------------------|
| 64 KiB  |    65,536 |       ~18 K   | Anything modern. Truncates a `cat` of any non-trivial log. |
| 256 KiB |   262,144 |       ~75 K   | Comfortably fits a 200 K window with room for history.  |
| 1 MiB   | 1,048,576 |      ~300 K   | Fits a 1 M-token window with margin; blows a 200 K window. |

The motivating bug was 1.3 M tokens (~4.5 MiB). 256 KiB cuts that by
~18x and leaves room for several large-but-not-pathological results to
coexist with conversation history. It also passes through typical
"read this source file" tool calls untouched -- the median Rust file
in this repo is well under 64 KiB; the largest top out around 30 KiB
-- so the *normal* development workflow is unaffected.

64 KiB was tempting because it fits a typical "read a single file"
budget. It was rejected because it would silently truncate every
moderately verbose tool output (a 100-line cargo-test error, a
`git log -p` of a small range, a `tree` of a deep directory). That's
a worse default failure mode than the rare oversized result.

The user can tighten globally (`tool-result-cap = 65536`) or loosen per
agent (`tool-result-cap = 1048576` on a 1 M-token model). The default
exists so users who don't care don't have to think about it.

## Architecture

### Truncation strategy: head-only, byte-bounded

When `result.len() > cap`, the truncated string is:

```
<first content_budget bytes of result, sliced at a UTF-8 boundary>

[outrig: tool result truncated]
  original size: 1,234,567 bytes
  cap:           262,144 bytes (--max-tool-result-bytes)
  kept:          first 261,xxx bytes; trailing 973,423 bytes dropped.

  This tool result was larger than the configured cap. Your next call
  should narrow the query: use head/tail/grep/--max-count, scope a
  directory or line range, or call a more specific tool. Re-running
  the same call will produce the same truncation.
```

The total length stays at or below `cap`, so a user who asks for a
256 KiB cap *gets* a 256 KiB ceiling on what reaches Rig. The marker
text is a fixed budget (~512 bytes); the head gets `cap - marker_budget`
bytes.

The unit is **bytes**, not tokens and not characters:

- Bytes are O(1) to check (`String::len()`).
- They match the user's mental model when picking a number.
- They give a hard upper bound regardless of model -- token<->byte
  ratio depends on the tokenizer, which we'd have to load and pin to
  the model in use.

#### Why head, not tail, not head+tail sandwich

- **Head-only** is right for the dominant case: `cat large_log.txt`,
  `find / -name '*.rs'`, `cargo build` flooding warnings. The signal
  is at the *front* (a process announces what it's doing, then floods
  stdout). Truncating the tail keeps the most semantically valuable
  bytes.
- **Tail-only** would help for trailing error output, but truncating
  the head loses the invocation context, which is harder for the LLM
  to reconstruct.
- **Head+tail sandwich** (e.g., 60% head + 40% tail with a `[... N
  bytes elided ...]` middle marker) catches both cases. Rejected for
  v0 because it complicates the implementation (two boundary-safe
  slices), produces visually harder-to-parse results, and the marker
  already tells the LLM "narrow the query" -- if the useful content
  was at the tail, the LLM can re-run with `tail -n` or equivalent.
  The marker format leaves room to add this later without breaking
  existing configs.

#### UTF-8 boundary safety

`String::len()` is bytes; a naive `&result[..cap]` panics if `cap`
falls mid-codepoint. The slice must back up to the last valid char
boundary at or before the budget. Rust's `str::floor_char_boundary`
(stable since 1.79) is the cleanest. If the project's MSRV pins below
that, walk back manually:

```rust
let mut cut = content_budget.min(result.len());
while !result.is_char_boundary(cut) {
    cut -= 1;
}
let head = &result[..cut];
```

Either approach guarantees the output is valid UTF-8.

### Plumbing the cap to each adapter

The cap value lives on `ResolvedAgent` (`src/llm.rs:115-129`), resolved
from the precedence chain at the same site that resolves
`tool_call_cap` in the sibling spec. From there it flows into
`from_client_tools(client, cap)` (one extra arg, one call site at
`src/cli/run.rs:204`), which stores it on each constructed
`McpToolAdapter`.

Two layouts were considered:

(a) **Adapter-side field.** Add `result_cap_bytes: usize` to
    `McpToolAdapter` and set it during `from_client_tools`.
(b) **Wrapper at build_agent time.** Wrap each adapter in a
    `CappingToolAdapter` that delegates `name` / `definition` and
    intercepts `call`.

**Pick (a).** `McpToolAdapter` is already the single chokepoint;
adding a field is surgical, while wrapping creates a parallel type to
keep in sync. `Clone` and `Debug` derives still hold for a `usize`
field.

The truncation logic itself is pulled into a free function
`truncate_for_llm(result: &str, cap: usize) -> String` so it's
unit-testable without spinning up an MCP server.

### ResolvedAgent change

`src/llm.rs:115-129` -- `ResolvedAgent` gains:

```rust
pub tool_result_cap_bytes: usize,
```

Note: `usize`, not `Option<usize>`. Resolution always produces a
concrete value (there's a compiled-in default), so the field reads as
"the agent's effective cap, regardless of where it came from."

### Config additions

`src/config/mod.rs:25` (`Config`):

```rust
#[serde(default, skip_serializing_if = "Option::is_none")]
pub tool_result_cap: Option<u32>,
```

`src/config/mod.rs:131-142` (`Agent`):

```rust
#[serde(default, skip_serializing_if = "Option::is_none")]
pub tool_result_cap: Option<u32>,
```

`u32` matches the existing `max_tokens` style. With `kebab-case` the
TOML key is `tool-result-cap`. Cast to `usize` at use site -- safe
because the upper-bound check in validation keeps it well under
`u32::MAX`.

### CLI

`src/cli/run.rs:35-50` (`RunArgs`):

```rust
/// Override the per-result truncation cap (bytes). Defaults to the
/// compiled-in 262144 (256 KiB), or whatever `tool-result-cap`
/// resolves to from agent/global config. Tool results larger than
/// this are truncated with a marker before being sent to the LLM.
#[arg(long = "max-tool-result-bytes", value_name = "N")]
pub max_tool_result_bytes: Option<u32>,
```

Resolution happens at the same site the sibling spec resolves
`tool_call_cap` -- immediately after `resolve_agent`, before container
resolution. The resolved value lands on `ResolvedAgent` and threads
forward into `from_client_tools`.

### Validation

`src/config/validate.rs` rejects:

- `tool-result-cap < 1024` -- below 1 KiB the marker dominates and
  truncation is meaningless.
- `tool-result-cap > 16 * 1024 * 1024` (16 MiB) -- arbitrary upper
  guard; well above any sane single tool result for a 1 M-token
  context window. Anything larger is almost certainly a config typo.

New error variants:

```rust
#[error("tool-result-cap must be at least 1024 bytes (got {got})")]
ToolResultCapTooSmall { got: u32 },

#[error("tool-result-cap must be at most {max} bytes (got {got})")]
ToolResultCapTooLarge { got: u32, max: u32 },
```

CLI validation rejects the same range with a clap-time error.

## Sub-decisions

- **Whether to also truncate MCP error results.** Yes. The
  `result.is_error == true` branch in `McpToolAdapter::call`
  (line 105-108) currently routes the text into
  `ToolError::ToolCallError`. A runaway error message can blow context
  just as easily as a runaway success message; apply the same
  truncation there.
- **Whether the cap should be configurable per-tool.** No, not for v0.
  Adds a fourth resolution layer (currently providers / models /
  agents / containers) and isn't justified until someone hits a
  concrete case. Defer to follow-up.
- **Whether to print a stderr line when truncation fires.** No. The
  marker on the result itself is visible in the session log; an extra
  stderr line is noise on busy turns. Reconsider if debugging
  truncation events turns out to be common.
- **Whether to surface the resolved cap in the banner.** Helpful for
  debugging when someone overrides it; cheap to add. Worth doing in
  the same task only if it falls out naturally, otherwise leave for
  follow-up. The sibling `tool-call-cap` spec makes the same call.
- **Whether `tool-result-cap = exactly cap_bytes` truncates.** No. The
  check is strict `>`; a result whose length equals the cap goes
  through unchanged. Less surprising than near-misses changing the
  output.
- **Empty result handling.** `result.is_empty()` returns immediately;
  no marker, no work.

## Files

- `src/rig_tool.rs:42-48` -- `McpToolAdapter` struct gains
  `result_cap_bytes: usize`.
- `src/rig_tool.rs:53-66` -- `from_client_tools(client, cap)`: extra
  arg, threaded into the constructed adapters.
- `src/rig_tool.rs:91-114` -- `call`: apply truncation to
  `result.content_text` (success) and to the error-text branch
  (line 105-108) before returning. Pull the truncation logic into a
  free function `truncate_for_llm(result: &str, cap: usize) -> String`
  for unit-testability.
- `src/llm.rs:115-129` -- `ResolvedAgent` gains
  `tool_result_cap_bytes: usize`.
- `src/llm.rs:138-240` -- `resolve_agent` computes the cap from the
  precedence chain. Signature gets an extra arg
  `cli_override: Option<u32>` (or take the whole `RunArgs`-equivalent
  -- match whatever pattern the sibling spec ends up using).
- `src/llm.rs` -- add
  `pub const DEFAULT_TOOL_RESULT_CAP_BYTES: usize = 256 * 1024;`
  near `MAX_TOOL_CALLS`.
- `src/config/mod.rs:25-49` -- `Config.tool_result_cap`.
- `src/config/mod.rs:131-142` -- `Agent.tool_result_cap`.
- `src/config/validate.rs` -- range check on the new field at both
  the top-level and per-agent locations. New error variants.
- `src/config/merge.rs` -- merge the new top-level field
  (`repo.tool_result_cap.or(global.tool_result_cap)`).
- `src/cli/run.rs:35-50` -- `RunArgs` gains `--max-tool-result-bytes`.
- `src/cli/run.rs:69` -- after `resolve_agent`, plumb
  `args.max_tool_result_bytes` into the resolution and onto
  `ResolvedAgent`.
- `src/cli/run.rs:199-208` -- pass `resolved.tool_result_cap_bytes`
  into `McpToolAdapter::from_client_tools(arc.clone(), cap)`.
- `tests/tool_result_cap.rs` (new) -- unit tests for
  `truncate_for_llm` (boundary conditions: below cap, exactly at cap,
  one byte over, far over, mid-codepoint UTF-8, empty input, marker
  presence) and resolution-precedence tests.
- `tests/` (e2e, behind `--features e2e`) -- optional. Stub MCP server
  whose only tool returns a deterministic blob of N bytes; configure
  `tool-result-cap = 4096`; assert the tool-result message in
  conversation history is `<= 4096` bytes and contains the marker;
  smoke-test that the next turn does not 400.

## Acceptance

- `outrig run --max-tool-result-bytes 65536` caps every tool result
  for that run at 64 KiB; results larger than that arrive at the LLM
  truncated with the documented marker.
- `tool-result-cap = 524288` at the top level of `outrig.toml` raises
  the cap to 512 KiB by default; an agent with its own
  `[agents.foo] tool-result-cap = 1048576` overrides for that agent;
  `--max-tool-result-bytes 32768` on the CLI overrides both.
- `tool-result-cap = 0` rejected at config-load time with a clear
  error; `tool-result-cap = 100_000_000` rejected the same way.
- A tool whose raw result is 5 MiB produces, in the LLM-visible
  conversation history, a string at or below the configured cap whose
  tail is the documented marker text including the original size and
  cap.
- The truncated string is valid UTF-8 even when the boundary falls
  inside a multi-byte codepoint.
- An MCP error result of similar size is also truncated (not just the
  success branch).
- Existing default behavior (no flag, no config) caps at 256 KiB,
  which is large enough that all existing test scenarios and routine
  development tool calls pass through untouched. The marker never
  appears in normal use.
- `DEFAULT_TOOL_RESULT_CAP_BYTES = 256 * 1024` is exported from
  `src/llm.rs` so a future banner-print or `outrig config show`-style
  command can reference it.

## Decisions

- The resolved result cap is printed in the `outrig run` banner next
  to the existing tool-call cap. This keeps both runtime safety limits
  visible after config and CLI overrides resolve.
- Config validation keeps path-qualified errors (`top-level
  tool-result-cap`, `agents.<name>.tool-result-cap`) like the existing
  `tool-call-cap` validator, rather than using pathless cap errors.
- Unit coverage checks the adapter's underlying MCP-error payload
  directly. Rig's `ToolError` display adds its own `ToolCallError:`
  prefix, so the adapter payload is the precise value outrig controls.

## Out of scope

These were considered and intentionally deferred:

- **Per-tool overrides.** Something like
  `[tools.shell.run] result-cap = 1048576` so a known-noisy tool gets
  more headroom while a known-quiet one stays tight. Adds a third
  config map and changes the resolution chain to four layers. Not
  worth the surface area until someone hits a concrete case.
- **Hook-based reactive shrinking.** Rejected upstream of this spec
  (Rig 0.36 hook surface doesn't support it; see Context). Could
  become viable if Rig adds `on_tool_result` mutation in a future
  release; revisit then.
- **Manual completion-loop ownership.** Outrig owning the message
  buffer, retry, and tool dispatch directly would unlock
  retry-on-400-with-truncation, smarter mid-turn shrinking, and
  partial-history recovery on any error. Substantial rewrite of
  `run_turn_inner`. Defer until at least one more feature also wants
  this -- combine the work then.
- **Automatic retry-on-400-with-truncation.** Requires either manual
  loop ownership (above) or pre-flight token estimation; both are
  large changes. Proactive truncation removes the *cause* of these
  400s in practice, so the reactive path is lower priority.
- **Head+tail sandwich strategy.** See "Why head, not tail" above.
  Marker format leaves room to add this without breaking existing
  configs.
- **Token-count cap instead of byte-count.** A token-count cap matches
  the provider's actual constraint better but requires running a
  tokenizer per result, with the tokenizer pinned to the model in
  use. Bytes are model-agnostic and a reasonable proxy. Revisit if
  outrig ever depends on a tokenizer for another reason.
- **Banner display of the resolved cap.** Cheap to add; do it in the
  same task only if it falls out naturally, otherwise leave for a
  follow-up.

## Dependencies

None. This is self-contained against current `trunk` and orthogonal to
the sibling `configurable-tool-call-cap.md` spec; either can land
first.
