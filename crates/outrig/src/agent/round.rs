//! One round: a prompt, the model and its tool calls, and the reply.
//!
//! Copied from `outrig-cli`'s `llm.rs` down to what a round needs: the
//! tool-call cap and the partial history a stopped round keeps. The subagent
//! machinery -- labels, steering, the repeat breaker -- is not here, and
//! neither is anything that recovers from a failing endpoint, which arrives
//! with retry.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use rig::agent::{Agent, AgentHook, Flow, HookContext, StepEvent, StepEventKind};
use rig::completion::{CompletionModel, Message, Prompt, PromptError};

use super::AgentError;
use super::build::RigAgent;
use super::tool;

/// How a round ended.
pub(crate) struct RoundEnd {
    /// The model's closing text.
    pub(crate) reply: String,
    /// Why the round was cut short, when it was. `None` means the model
    /// finished on its own.
    pub(crate) stopped: Option<String>,
}

impl RigAgent {
    /// Run one round against `history`, extending it with everything the round
    /// emitted.
    ///
    /// A round cut short -- the tool-call cap, or rig's own turn budget -- keeps
    /// what it managed, so the next prompt can carry on from it. Any other
    /// failure is an error. It leaves `history` as it was when the round had
    /// run no Python; otherwise it keeps the tool calls that completed, since
    /// what they did stands and nothing is rolled back.
    pub(crate) async fn round(
        &self,
        prompt: &str,
        history: &mut Vec<Message>,
        tool_call_max: usize,
    ) -> Result<RoundEnd, AgentError> {
        let hook = RoundHook::new(tool_call_max);
        match self {
            RigAgent::OpenAi(agent) => run_round(agent, prompt, history, hook).await,
            RigAgent::Anthropic(agent) => run_round(agent, prompt, history, hook).await,
        }
    }
}

async fn run_round<M: CompletionModel + 'static>(
    agent: &Agent<M>,
    prompt: &str,
    history: &mut Vec<Message>,
    hook: RoundHook,
) -> Result<RoundEnd, AgentError> {
    // rig's own budget is a backstop set above the hook's, so the hook -- with
    // its message the model can read -- is the limiter that fires first. As of
    // rig 0.40 it counts every model call, the first included.
    let max_turns = hook.max + 2;
    // Cloned rather than moved: the clone shares the hook's state, so it can
    // still be asked afterwards whether the stop was OutRig's doing.
    let observer = hook.clone();
    let result = agent
        .prompt(prompt.to_string())
        .history(history.clone())
        .max_turns(max_turns)
        .add_hook(hook)
        .extended_details()
        .await;

    match result {
        Ok(response) => {
            let messages = response
                .messages
                .expect("rig populates messages on extended_details");
            history.extend(messages);
            // A hook stop normally surfaces as an error, but reading the reason
            // back unconditionally means a stop can never be lost to a path
            // that ends the run cleanly instead.
            Ok(RoundEnd {
                reply: response.output,
                stopped: observer.stop_reason(),
            })
        }
        Err(err) => stopped_short(err, history, &observer),
    }
}

/// Turn a round-ending error into a round cut short, or pass it on.
///
/// `hook` is the round's own hook, consulted to tell OutRig's deliberate stops
/// from a cancellation rig raised for itself. Both arrive as
/// `PromptError::PromptCancelled`, and only a stop the hook recorded is one
/// OutRig chose; anything else is a fault and stays an error.
///
/// An error after the round has run Python keeps what it ran. Most errors
/// carry no history -- a failed model call is `CompletionError`, which has
/// none -- so what is kept is what the hook saw the last model call sent: the
/// completed tool calls and their results, in an order the provider already
/// accepted. Dropping them would leave the interpreter holding what they did
/// and the conversation saying they never ran, which is how a resent prompt
/// runs a `git push` twice.
fn stopped_short(
    err: PromptError,
    history: &mut Vec<Message>,
    hook: &RoundHook,
) -> Result<RoundEnd, AgentError> {
    let (reason, chat_history) = match (err, hook.stop_reason()) {
        (PromptError::PromptCancelled { chat_history, .. }, Some(ours)) => (ours, chat_history),
        (
            PromptError::MaxTurnsError {
                max_turns,
                chat_history,
                ..
            },
            _,
        ) => (cap_reason(max_turns), *chat_history),
        (other, _) => {
            return Err(match hook.take_sent() {
                Some(sent) => {
                    extend_history_with_new_suffix(history, sent);
                    AgentError::PromptAfterWork(other.to_string())
                }
                None => other.into(),
            });
        }
    };
    extend_history_with_new_suffix(history, chat_history);
    Ok(RoundEnd {
        reply: String::new(),
        stopped: Some(reason),
    })
}

fn cap_reason(max: usize) -> String {
    format!("tool-call iteration max ({max}) reached")
}

/// Append what `returned` adds to `history`. rig hands back the *whole*
/// history on an error path, so the prefix already held is skipped.
fn extend_history_with_new_suffix(history: &mut Vec<Message>, returned: Vec<Message>) {
    let existing_len = history.len();
    if returned.len() >= existing_len && returned[..existing_len] == history[..] {
        history.extend(returned.into_iter().skip(existing_len));
    } else {
        history.extend(returned);
    }
}

/// Per-round hook: counts tool calls against the cap, stops the loop once it is
/// spent, and hands the tool's output to the model as the text it is.
///
/// Cloned by rig per request; the shared state keeps one round's calls
/// counting against the same cap.
#[derive(Clone)]
struct RoundHook {
    calls: Arc<AtomicUsize>,
    /// Whether this hook stopped the loop. Recorded here rather than recovered
    /// from the cancellation rig reports, because rig raises `PromptCancelled`
    /// for its own internal faults too.
    stopped: Arc<AtomicBool>,
    max: usize,
    /// What the latest model call after a tool call was sent: the history
    /// before it, then its prompt. `None` until a tool has run.
    sent: Arc<Mutex<Option<Vec<Message>>>>,
}

impl RoundHook {
    fn new(max: usize) -> Self {
        Self {
            calls: Arc::new(AtomicUsize::new(0)),
            stopped: Arc::new(AtomicBool::new(false)),
            max,
            sent: Arc::new(Mutex::new(None)),
        }
    }

    fn take_sent(&self) -> Option<Vec<Message>> {
        self.sent
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
    }

    /// Why this hook stopped the loop, if it did. The cap is the only reason
    /// it has.
    fn stop_reason(&self) -> Option<String> {
        self.stopped
            .load(Ordering::SeqCst)
            .then(|| cap_reason(self.max))
    }
}

impl<M: CompletionModel> AgentHook<M> for RoundHook {
    fn observes(&self, kind: StepEventKind) -> bool {
        matches!(
            kind,
            StepEventKind::CompletionCall | StepEventKind::ToolCall | StepEventKind::ToolResult
        )
    }

    async fn on_event(&self, _ctx: &HookContext, event: StepEvent<'_, M>) -> Flow {
        match event {
            StepEvent::CompletionCall { .. } if self.calls.load(Ordering::SeqCst) > self.max => {
                self.stopped.store(true, Ordering::SeqCst);
                Flow::terminate(cap_reason(self.max))
            }
            // Every model call after the first follows tool results, so from
            // here on the request carries work that has happened.
            StepEvent::CompletionCall {
                prompt,
                history,
                turn,
            } if turn > 1 => {
                let sent = history.iter().chain([prompt]).cloned().collect();
                *self.sent.lock().unwrap_or_else(PoisonError::into_inner) = Some(sent);
                Flow::cont()
            }
            StepEvent::ToolCall {
                tool_name, args, ..
            } => {
                if self.calls.fetch_add(1, Ordering::SeqCst) >= self.max {
                    return Flow::skip(format!(
                        "[outrig] tool call not executed: per-round tool-call max ({}) \
                         was reached before this call could run. The user may continue \
                         with a fresh max; repeat the tool call if still needed.",
                        self.max
                    ));
                }
                tracing::debug!("tool call: {tool_name}({args})");
                Flow::cont()
            }
            // rig reads a tool's output as JSON when it can, and hands an object
            // carrying `response` or `parts` to the model as that field alone --
            // so `print(json.dumps(reply))` of an API reply would lose every
            // other key. A result a hook rewrites is delivered verbatim, and
            // what a program printed is text. Every result is rewritten, rather
            // than only those that would parse, so no rule of rig's has to be
            // predicted here. rig 0.42's `ToolOutput::text` says this at the
            // tool, and makes this arm unnecessary.
            StepEvent::ToolResult {
                tool_name, result, ..
            } if tool_name == tool::NAME => Flow::rewrite_result(result.to_string()),
            _ => Flow::cont(),
        }
    }
}
