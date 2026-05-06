# 0048 -- Configurable + resumable tool-call cap

## Context

The per-turn tool-call cap is hardcoded at `MAX_TOOL_CALLS = 50` in
`src/llm.rs:18`. It fires from `OutrigPromptHook::on_tool_call`
(`src/llm.rs:400-418`) and is mirrored in `agent.prompt(...).max_turns(...)`
(`src/llm.rs:361`) as a defense in depth. There's no CLI flag, env var, or
config field to change it; the only way to raise the cap today is to
recompile.

50 fits a typical "explain this file" or "edit this function" turn with
plenty of margin. It does not fit *loop-shaped* prompts: rebase + watch CI
+ classify failure + retry on flake. A naive iteration of that prompt costs
~13-22 tool calls when the agent polls CI status, and two rounds of polling
overshoots before any analysis happens. A run on 2026-05-05 hit the cap on
exactly that prompt.

There's a second symptom worth fixing in the same change. When the cap
trips, `run_turn_inner` (`src/llm.rs:374-377`) catches `PromptCancelled`,
prints `[outrig] tool-call iteration cap (50) reached; ending turn` to
stderr, returns `"(turn ended; tool-call cap reached)"` as the assistant
reply, and -- critically -- *does not extend `history`*. So the user can't
just say "keep going": the rebase the agent did, the CI poll it watched,
the failure log it read -- all of that is gone from the next turn's
context. The user has to repeat it from a fresh turn.

`rig::completion::PromptError::PromptCancelled` actually carries
`chat_history: Vec<Message>` (rig 0.36 at
`registry/src/.../rig-core-0.36.0/src/completion/request.rs:144-147`), so
the partial history is recoverable; outrig is throwing it away.

## Goal

1. Make the cap configurable at three layers (CLI, agent config, global
   config) with a sensible compiled-in default.
2. When the cap fires, preserve the partial conversation history so a
   follow-up prompt can continue from where the previous turn stopped, and
   surface a hint to the user about how to do that.

Non-goals: per-tool budgets (e.g., "at most 10 shell calls"), automatic
resume without user opt-in, persisting history to disk between sessions.

## User surface

```bash
# Raise the cap for this run only:
outrig run --max-tool-calls 200

# Per-agent default in repo config:
# outrig.toml
[agents.long-running]
model = "claude-opus-4-7"
tool-call-cap = 300

# Global default in the user-level config:
# ~/.config/outrig/config.toml
tool-call-cap = 100
```

When the cap fires:

```
[outrig] tool-call iteration cap (50) reached; ending turn
[outrig] partial history retained -- send another prompt (e.g. "continue")
        to keep going, or "/reset" to drop it.
```

Resolution order, lowest to highest precedence:

1. The compiled-in default (`MAX_TOOL_CALLS`, currently 50).
2. Top-level `tool-call-cap` in config.
3. `[agents.<name>].tool-call-cap` for the resolved agent.
4. `--max-tool-calls N` on the CLI.

## Architecture

### Plumb the cap value through

Today `MAX_TOOL_CALLS` is referenced twice in `run_turn_inner`
(`src/llm.rs:343` and `src/llm.rs:361`). The cap is *per turn*, not per
agent build-time, so the natural place to carry it is alongside the
resolved agent (`ResolvedAgent` at `src/llm.rs` -- the struct that
already carries preamble/temperature/max_tokens). Add a
`pub tool_call_cap: usize` field, populated during agent resolution from
the precedence chain above.

`RigAgent::run_turn` (`src/llm.rs:342-349`) reads
`self.tool_call_cap` and passes it to both `OutrigPromptHook::new(...)`
and `.max_turns(...)`. The constant `MAX_TOOL_CALLS` stays as the
compiled-in default but is no longer the only call-site value.

### Config additions

`src/config/mod.rs:25` (`Config`):

```rust
#[serde(default, skip_serializing_if = "Option::is_none")]
pub tool_call_cap: Option<u32>,
```

`src/config/mod.rs:131-142` (`Agent`):

```rust
#[serde(default, skip_serializing_if = "Option::is_none")]
pub tool_call_cap: Option<u32>,
```

`u32` matches the existing `max_tokens: Option<u32>` style; cast to
`usize` at use site. `kebab-case` rename means TOML key is
`tool-call-cap`.

### CLI

`src/cli/run.rs:35-50` (`RunArgs`):

```rust
/// Override the per-turn tool-call cap. Defaults to the compiled-in 50,
/// or whatever `tool-call-cap` resolves to from agent/global config.
#[arg(long = "max-tool-calls", value_name = "N")]
pub max_tool_calls: Option<u32>,
```

Resolution happens during agent resolve (or just after it), wherever
agent + model + container precedence is currently merged. Look for the
function that already merges agent/global config layers and add the cap
to it.

### Validation

`src/config/validate.rs` rejects:

- `tool-call-cap = 0` -- terminates on first call, useless.
- `tool-call-cap > 2000` -- arbitrary upper guard so a misconfigured
  agent can't burn unbounded model quota. The number is debatable; pick
  a value that's clearly a "you really should think about this" line.

CLI validation rejects the same range with a clap-time error.

### Preserve history on cancellation

The load-bearing change. `src/llm.rs:374-377` currently:

```rust
Err(rig::completion::PromptError::PromptCancelled { reason, .. }) => {
    eprintln!("[outrig] {reason}");
    Ok("(turn ended; tool-call cap reached)".to_string())
}
```

becomes:

```rust
Err(rig::completion::PromptError::PromptCancelled { reason, chat_history }) => {
    eprintln!("[outrig] {reason}");
    eprintln!(
        "[outrig] partial history retained -- send another prompt \
         (e.g. \"continue\") to keep going, or \"/reset\" to drop it."
    );
    history.extend(chat_history);
    Ok("(turn ended; tool-call cap reached)".to_string())
}
```

The `chat_history` field on the `PromptCancelled` variant is the
prompt + assistant + tool-call messages rig accumulated up to the
moment the hook tripped. Extending `history` with it leaves the next
turn fully grounded.

The existing comment at `src/llm.rs:339-342` ("splicing partial mid-turn
state cleanly is a correctness rabbit hole; the next user turn just
re-grounds") needs updating too -- it's about future-cancellation paths
broadly, but the specific `PromptCancelled` carries clean history that's
safe to splice.

There is no separate `/continue` command -- the user just types
`continue` (or anything else) and the next turn picks up. A dedicated
slash command is nice-to-have but adds REPL surface that doesn't earn
its keep when typing the word "continue" works.

### REPL hint

The hint is two extra `eprintln!` lines next to the existing one. No
state machine, no flag, no special prompt. The REPL keeps prompting at
`> ` as it does today.

## Sub-decisions

- **Whether the cap counts across turns or per turn.** Today it's
  per turn (the `OutrigPromptHook` counter is fresh on each `run_turn`
  call). Keep that. A session-wide cap is a different feature and would
  fight the resume affordance below. Document this in the CLI help text
  so users don't expect "100 calls total per `outrig run`."
- **Whether to skip the upper guard** when the value comes from a
  trusted source (CLI vs. config). Probably no -- treat all sources the
  same. Easier to reason about.
- **Whether `chat_history` from rig might be empty** when cancellation
  fires before any tool call resolves. Unlikely (the cap fires *after*
  N calls have been recorded), but the splice is harmless if it's
  empty: `Vec::extend(empty)` is a no-op.
- **Whether to also surface the cap value in the banner**
  (`src/cli/run.rs:272+`, `print_banner`). Helpful for debugging when
  someone overrides it; cheap to add. Worth doing in the same task.

## Files

- `src/llm.rs` -- `ResolvedAgent` gains `tool_call_cap`; `run_turn` and
  `run_turn_inner` use it; `PromptCancelled` arm extends history.
- `src/config/mod.rs` -- `Config` and `Agent` gain `tool_call_cap`.
- `src/config/validate.rs` -- range check on the new field.
- `src/config/merge.rs` -- if agent/global merge happens here, the new
  field merges the same way the existing optional fields do.
- `src/cli/run.rs` -- `RunArgs` gains `--max-tool-calls`; `execute`
  resolves precedence and threads the value into agent build; banner
  optionally prints the resolved cap.
- `tests/llm_resolve.rs` (or a new `tests/tool_call_cap.rs`) -- unit
  tests on precedence resolution and validate-time range check.
- `tests/` (e2e, behind `--features e2e`) -- a test that sets the cap
  to `2`, prompts the agent into 3 tool calls, asserts the cancellation
  message appears, then sends a second prompt and asserts the agent
  references state from the cancelled turn (proving history was
  preserved).

## Acceptance

- `outrig run --max-tool-calls 200` raises the cap to 200 for that run;
  `[outrig] tool-call iteration cap (200) reached` is the message that
  fires if 200 is exceeded.
- `tool-call-cap = 100` at the top level of `outrig.toml` raises the
  cap to 100 by default; an agent with its own
  `[agents.foo] tool-call-cap = 300` overrides; `--max-tool-calls 50`
  on the CLI overrides both.
- `tool-call-cap = 0` rejected at config-load time with a clear error;
  `tool-call-cap = 5000` rejected the same way.
- After the cap fires, the next user prompt re-enters a turn whose
  history includes the prior turn's tool calls and assistant text. A
  prompt of literally `continue` produces a turn that continues the
  prior work rather than starting over. (Verified by checking the
  prompt sent to the model includes the prior tool-call messages.)
- Existing default behavior (no flag, no config) is unchanged: cap of
  50, same canned message, same stderr line. Turning the new feature
  off requires no action.

## Dependencies

None. This is a self-contained change against current `trunk`.
