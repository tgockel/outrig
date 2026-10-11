//! One round: a prompt, the model and its tool calls, and the reply.
//!
//! Copied from `outrig-cli`'s `llm.rs` down to what a round needs: the
//! tool-call cap and the partial history a stopped round keeps. The subagent
//! machinery -- labels, steering, the repeat breaker -- is not here. Recovering
//! from a failing endpoint happens below the round, inside each model call
//! ([`super::retry`], [`super::failover`]), and the round reads which model
//! answered each call off the response the chain hands rig.
//!
//! The conversation is the store's ([`History`]), not the round's. rig is
//! handed nothing from before the round, so everything it holds, and hands
//! back on success or in an error, is the round's own messages. The round's
//! turns are committed as they complete, so a round that fails or is dropped
//! has already kept every turn it finished; and on every model call the hook
//! replaces what rig would send with the view the store assembles from them,
//! held to the model's [`Budget`]. Nothing rig returns is compared against the
//! store.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use rig::OneOrMany;
use rig::agent::{AgentHook, Flow, HookContext, RequestPatch, StepEvent, StepEventKind};
use rig::completion::message::{
    AssistantContent, ToolCall, ToolResult, ToolResultContent, UserContent,
};
use rig::completion::{Message, Prompt, PromptError};

use super::AgentError;
use super::budget::Budget;
use super::build::RigAgent;
use super::failover::FailoverModel;
use super::history::{self, Adjacent, History};
use super::ledger::CallSlot;
use super::tool::{self, Interrupts};
use crate::config::RoleAlternation;
use crate::harness::event::{AttemptId, CallUsage, Payload, Usage};

/// How a round ended.
pub(crate) struct Finished {
    /// The model's closing text.
    pub(crate) reply: String,
    /// Why the round was cut short, when it was. `None` means the model
    /// finished on its own.
    pub(crate) stopped: Option<String>,
    /// The final message's reasoning, when it held no text: a round the model
    /// spent thinking -- often cut off at its output ceiling -- is not
    /// reported as one that said nothing.
    pub(crate) reasoning: Option<String>,
}

/// Run one round of `agent` that `opening` opens, committing its turns to
/// `history` as they complete. `history` has begun the round on the same
/// message ([`History::open_round`]): rig's first message is the store's
/// opening.
///
/// A round cut short -- the tool-call cap, or rig's own turn budget -- keeps
/// what it managed, so the next prompt can carry on from it. Any other failure
/// is an error. It leaves the store as it was when the round had run no
/// Python; otherwise the store keeps the tool calls that completed, since what
/// they did stands and nothing is rolled back. A round whose future is dropped
/// before it returns -- Ctrl-C at the REPL while no Python runs -- keeps them
/// the same way, as it is dropped.
///
/// Each model call is sent what `history` assembles for it within `budget`,
/// the head's allowance. A call whose latest turn would not fit on its own is
/// not made: the round ends there, keeping its turns, and says which turn it
/// was. A call the chain moves to another model is assembled again for that
/// model's allowance.
///
/// An interrupt relayed through `interrupts` while a call waits on Python
/// stops that execution, and the round goes on: the model reads how it ended.
/// The turn's later calls are not run.
pub(crate) async fn round(
    agent: &RigAgent,
    opening: Message,
    history: &History,
    budget: &Arc<Budget>,
    tool_call_max: usize,
    interrupts: &Interrupts,
) -> Result<Finished, AgentError> {
    let slot = agent.model.slot().clone();
    slot.open_round();
    let hook = RoundHook::new(
        tool_call_max,
        interrupts.clone(),
        history.clone(),
        Arc::clone(budget),
        slot,
    );
    // rig's own budget is a backstop set above the hook's, so the hook -- with
    // its message the model can read -- is the limiter that fires first. As of
    // rig 0.40 it counts every model call, the first included.
    let max_turns = hook.max + 2;
    // Cloned rather than moved: the clone shares the hook's state, so it can
    // still be asked afterwards whether the stop was OutRig's doing.
    let mut unfinished = KeptIfDropped {
        hook: hook.clone(),
        armed: true,
    };
    let result = agent
        .prompt(opening)
        // Nothing from before the round: the hook sends that, on every call.
        .history(Vec::<Message>::new())
        .max_turns(max_turns)
        .add_hook(hook)
        .extended_details()
        .await;
    // From the next line on, nothing may wait.
    unfinished.armed = false;
    let hook = &unfinished.hook;

    let ended = match result {
        Ok(response) => {
            hook.flush(
                &response
                    .messages
                    .expect("rig populates messages on extended_details"),
            );
            // A hook stop normally surfaces as an error, but reading the reason
            // back unconditionally means a stop can never be lost to a path
            // that ends the run cleanly instead.
            Ok(Finished {
                reasoning: is_blank(&response.output)
                    .then(|| recover_non_text(&response.content))
                    .flatten(),
                reply: response.output,
                stopped: hook.stop_reason(),
            })
        }
        Err(err) => stopped_short(err, hook),
    };
    // The round's total is its attempts', rather than rig's: rig cannot see
    // a usage recorded after the run, nor the attempts that failed.
    hook.record_end(&ended);
    ended
}

/// Whether `reply` says nothing a person could read.
fn is_blank(reply: &str) -> bool {
    reply.trim().is_empty()
}

/// The reasoning in `content`, the final message of a round whose text was
/// blank, joined into one block; `None` when there is none worth showing.
///
/// rig's `output` is the final message's text parts concatenated, so a turn
/// the model spent thinking -- cut off at its output ceiling, typically --
/// arrives as nothing, though it produced, and was billed for, reasoning.
fn recover_non_text(content: &OneOrMany<AssistantContent>) -> Option<String> {
    let recovered = content
        .iter()
        .filter_map(|part| match part {
            AssistantContent::Reasoning(reasoning) => {
                let text = reasoning.display_text();
                (!is_blank(&text)).then_some(text)
            }
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    (!is_blank(&recovered)).then_some(recovered)
}

/// Keeps what a round was running when its future is dropped. rig works on a
/// copy of the round, so without this a dropped round would leave the
/// conversation saying its latest Python never ran while the interpreter holds
/// what it did.
struct KeptIfDropped {
    hook: RoundHook,
    /// Cleared once the round has returned and its own ending takes over.
    armed: bool,
}

impl Drop for KeptIfDropped {
    fn drop(&mut self) {
        if self.armed {
            self.hook.keep_what_ran();
            self.hook.history.events().emit_with(|| {
                let (usage, attempts) = self.hook.spent();
                Payload::ModelRoundDropped {
                    round: self.hook.round,
                    calls: self.hook.calls_so_far(),
                    usage,
                    attempts,
                }
            });
        }
    }
}

/// Turn a round-ending error into a round cut short, or pass it on.
///
/// `hook` is the round's own hook, consulted to tell OutRig's deliberate stops
/// from a cancellation rig raised for itself. Both arrive as
/// `PromptError::PromptCancelled`, and only a stop the hook recorded is one
/// OutRig chose; anything else is a fault and stays an error. A provider's
/// refusal of a request whose view put one role after itself says so, since
/// that is the one shape of request this loop sends that a strict provider can
/// refuse.
///
/// An error after the round has run Python keeps what it ran. Most errors
/// carry no history -- a failed model call is `CompletionError`, which has
/// none -- so what is kept is what the hook committed and journaled: see
/// [`RoundHook::keep_what_ran`]. Dropping it would leave the interpreter
/// holding what the calls did and the conversation saying they never ran,
/// which is how a resent prompt runs a `git push` twice.
fn stopped_short(err: PromptError, hook: &RoundHook) -> Result<Finished, AgentError> {
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
            let text = format!("{other}{}", hook.refusal_hint(&other));
            return Err(if hook.keep_what_ran() {
                AgentError::PromptAfterWork(text)
            } else {
                AgentError::Prompt(text)
            });
        }
    };
    hook.flush(&chat_history);
    Ok(Finished {
        reply: String::new(),
        stopped: Some(reason),
        reasoning: None,
    })
}

fn cap_reason(max: usize) -> String {
    format!("tool-call iteration max ({max}) reached")
}

/// What a call that had started, but not returned, reads as when a round ends
/// around it. Its execution may still hold the interpreter's slot, and its
/// outcome arrives with a later tool result as a late one.
const CALL_UNFINISHED: &str = "[outrig: this call had not returned when the round ended. If its \
     code started, it may still be running, and its outcome will be reported with a later \
     call. Do not run it again.]";

/// What a call the round ended before reaching reads as.
const CALL_NOT_RUN: &str = "[outrig: not run -- the round ended before this call started.]";

/// What a call reads as when the user stopped an earlier one in its turn.
const CALL_STOPPED_TURN: &str = "[outrig] not run: the user interrupted an earlier call in this \
     turn. Read how that call ended, then decide whether this one is still wanted.";

/// Per-round hook: counts tool calls against the cap, stops the loop once it is
/// spent, hands the tool's output to the model as the text it is, commits the
/// round's turns as they complete, sends each model call the view the store
/// assembles within the budget, and journals what the round has done that is
/// not a turn yet.
///
/// Cloned by rig per request; the shared state keeps one round's calls
/// counting against the same cap.
#[derive(Clone)]
struct RoundHook {
    calls: Arc<AtomicUsize>,
    /// Why this hook stopped the loop, once it has. Recorded here rather than
    /// recovered from the cancellation rig reports, because rig raises
    /// `PromptCancelled` for its own internal faults too.
    stopped: Arc<Mutex<Option<String>>>,
    max: usize,
    journal: Arc<Mutex<Journal>>,
    interrupts: Interrupts,
    /// Where the round's turns are committed.
    history: History,
    /// What each model call may carry.
    budget: Arc<Budget>,
    /// The round's number in the conversation.
    round: u32,
    /// Where the round's calls are named and its attempts recorded.
    slot: CallSlot,
}

/// The round as far as it has got: how much of it is in the store, and the
/// model call not yet a turn there, kept for when the round ends without rig
/// handing back its messages -- a failed model call, or the round's future
/// dropped.
struct Journal {
    /// How many of the round's messages the store holds. At least one: rig's
    /// first is the round's opening, which the store holds from the start.
    committed: usize,
    /// Whether any call has started this round. Until one has, nothing the
    /// round did stands, and the conversation is left as it was.
    ran: bool,
    /// The latest model call. Taken when what it ran is kept.
    latest: Latest,
    /// Where the latest model call's view first put one role after itself.
    adjacent: Option<Adjacent>,
    /// Each of the round's model calls rig accepted: its place, the model that
    /// answered it, and what it used, as the provider reported it.
    calls: Vec<CallUsage>,
}

/// The round's latest model call, while it is not yet a turn in the store.
#[derive(Default)]
struct Latest {
    /// Its reply, once rig accepted it, while its tool calls run.
    reply: Option<OneOrMany<AssistantContent>>,
    /// How many of the reply's tool calls have started.
    started: usize,
    /// The results of those that came back, as the model reads them. rig runs
    /// a turn's calls one at a time and in order -- its default
    /// `tool_concurrency`, which this loop does not change -- so the n-th
    /// result answers the n-th call.
    results: Vec<String>,
}

impl Journal {
    /// Commit what of `round` -- the round's messages, as rig holds them at a
    /// turn's end -- the store does not hold yet, as the turns it makes up.
    /// rig only ever appends to a round, so a count says where the rest
    /// begins, and only the rest is copied.
    fn commit<'a>(&mut self, round: impl Iterator<Item = &'a Message>, history: &History) {
        let rest: Vec<Message> = round.skip(self.committed).cloned().collect();
        for turn in history::split_turns(rest) {
            self.committed += turn.len();
            history.commit(turn, false);
        }
    }
}

impl RoundHook {
    fn new(
        max: usize,
        interrupts: Interrupts,
        history: History,
        budget: Arc<Budget>,
        slot: CallSlot,
    ) -> Self {
        Self {
            calls: Arc::new(AtomicUsize::new(0)),
            stopped: Arc::default(),
            max,
            journal: Arc::new(Mutex::new(Journal {
                committed: 1,
                ran: false,
                latest: Latest::default(),
                adjacent: None,
                calls: Vec::new(),
            })),
            interrupts,
            round: history.round(),
            history,
            budget,
            slot,
        }
    }

    /// The round's model calls so far, by each call's place.
    fn calls_so_far(&self) -> Vec<CallUsage> {
        self.journal().calls.clone()
    }

    /// What the round's attempts used, each counted once, and the attempts.
    fn spent(&self) -> (Option<Usage>, Vec<AttemptId>) {
        let attempts = self.slot.round_attempts();
        (self.slot.ledger().total(&attempts), attempts)
    }

    /// Record how the round ended: each call it made, and what its attempts
    /// used.
    fn record_end(&self, ended: &Result<Finished, AgentError>) {
        self.history.events().emit_with(|| {
            let calls = self.calls_so_far();
            let (usage, attempts) = self.spent();
            match ended {
                Ok(Finished { stopped, .. }) => Payload::ModelRoundCompleted {
                    round: self.round,
                    stopped: stopped.clone(),
                    usage,
                    input_tokens_max: calls
                        .iter()
                        .filter_map(|call| call.usage.map(|usage| usage.input_tokens))
                        .max(),
                    calls,
                    attempts,
                },
                Err(failed) => Payload::ModelRoundFailed {
                    round: self.round,
                    error: failed.to_string(),
                    calls,
                    usage,
                    attempts,
                },
            }
        });
    }

    fn journal(&self) -> std::sync::MutexGuard<'_, Journal> {
        self.journal.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// End the round with what of `round` -- every message of the round, as
    /// rig handed them back at its end -- is not in the store yet.
    fn flush(&self, round: &[Message]) {
        self.journal().commit(round.iter(), &self.history);
        self.history.finish_round();
    }

    /// Commit what the round's latest model call did, if the round ran any
    /// tool call, and say whether it did. What is kept is taken, so it is kept
    /// at most once. The turns the round finished are in the store already.
    ///
    /// That is, when the call's tool calls were still running, its reply and
    /// one result for every call in it: the result of each that returned, and
    /// for the rest a note that it had not returned or had not started. A
    /// provider refuses a tool call without its result, and a call left out
    /// would leave the model unaware of code that ran. A turn with such a note
    /// is committed incomplete, which is how the agent's code can tell it from
    /// one whose calls all came back.
    fn keep_what_ran(&self) -> bool {
        let mut journal = self.journal();
        let Latest {
            reply,
            started,
            results,
        } = std::mem::take(&mut journal.latest);
        if let (true, Some(reply)) = (journal.ran, reply) {
            let calls: Vec<&ToolCall> = reply
                .iter()
                .filter_map(|content| match content {
                    AssistantContent::ToolCall(call) => Some(call),
                    _ => None,
                })
                .collect();
            let incomplete = results.len() < calls.len();
            let answers: Vec<UserContent> = calls
                .into_iter()
                .enumerate()
                .map(|(n, call)| {
                    let text = match results.get(n) {
                        Some(result) => result.as_str(),
                        None if n < started => CALL_UNFINISHED,
                        None => CALL_NOT_RUN,
                    };
                    UserContent::ToolResult(ToolResult {
                        id: call.id.clone(),
                        call_id: call.call_id.clone(),
                        content: OneOrMany::one(ToolResultContent::text(text)),
                    })
                })
                .collect();
            let mut turn = vec![Message::Assistant {
                id: None,
                content: reply,
            }];
            if let Ok(answers) = OneOrMany::many(answers) {
                turn.push(Message::User { content: answers });
            }
            self.history.commit(turn, incomplete);
        }
        journal.ran
    }

    /// Stop the loop for `reason`, and say so to rig.
    fn stop(&self, reason: String) -> Flow {
        *self.stopped.lock().unwrap_or_else(PoisonError::into_inner) = Some(reason.clone());
        Flow::terminate(reason)
    }

    /// Why this hook stopped the loop, if it did: the tool-call cap, or a
    /// latest turn too large for the budget.
    fn stop_reason(&self) -> Option<String> {
        self.stopped
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// What to add to `err`, when a provider refused the latest call and that
    /// call's view put one role after itself in the conversation. For a
    /// provider whose row does not say its turns must alternate, the pair may
    /// be why, and the row's `role-alternation` is the remedy. For one whose
    /// row does, the view withheld what it could, and the pair that stands is
    /// the turn the call answers opening on a reply with nothing kept before
    /// it that ends on the user's side: its round's opening did not fit, and
    /// only a larger window mends that. Either says what the conversation held
    /// rather than what the wire carried, which differs by provider
    /// ([`Adjacent`]).
    fn refusal_hint(&self, err: &PromptError) -> String {
        let refused = err
            .provider_response_status()
            .is_some_and(|status| matches!(status.as_u16(), 400 | 422));
        let Some(adjacent) = self.journal().adjacent.filter(|_| refused) else {
            return String::new();
        };
        let Budget {
            model, provider, ..
        } = &*self.budget;
        match self.budget.alternation {
            RoleAlternation::Relaxed => format!(
                ". The conversation it was sent left turns out, which put {adjacent}; a provider \
                 or gateway that requires the user's and the model's turns to alternate may \
                 refuse that. If this one does, set [providers.{provider}].role-alternation = \
                 \"strict\", and each call is sent a conversation that alternates (see `outrig \
                 run-new` in doc/reference/cli.md)"
            ),
            RoleAlternation::Strict => format!(
                ". The conversation it was sent left turns out, which put {adjacent}, and nothing \
                 kept before the turn this call answers ends on the user's side, so no turn could \
                 be withheld to clear it: the round's opening did not fit the window. Set \
                 [models.{model}].context-window to the model's whole window, or lower \
                 tool-result-max, so that it does (see `outrig run-new` in doc/reference/cli.md)"
            ),
        }
    }
}

impl AgentHook<FailoverModel> for RoundHook {
    fn observes(&self, kind: StepEventKind) -> bool {
        matches!(
            kind,
            StepEventKind::CompletionCall
                | StepEventKind::CompletionResponse
                | StepEventKind::ToolCall
                | StepEventKind::ToolResult
        )
    }

    async fn on_event(&self, ctx: &HookContext, event: StepEvent<'_, FailoverModel>) -> Flow {
        if let StepEvent::ToolResult { result, .. } = &event {
            self.journal().latest.results.push(result.to_string());
        }
        match event {
            StepEvent::CompletionCall { .. } if self.calls.load(Ordering::SeqCst) > self.max => {
                self.stop(cap_reason(self.max))
            }
            StepEvent::CompletionCall {
                prompt,
                history: round,
                ..
            } => {
                self.interrupts.clear_turn();
                let call = self.slot.reserve_call();
                // What rig holds is whole turns by now, since a call is made
                // only once the last one's tool calls have returned. Once they
                // are committed, the store holds everything this call answers.
                let mut journal = self.journal();
                journal.commit(round.iter().chain([prompt]), &self.history);
                journal.latest = Latest::default();
                match self.history.assemble(&self.budget, call) {
                    Ok((mut sent, manifest)) => {
                        // rig sends the prompt itself, after the history.
                        let last = sent.pop();
                        debug_assert_eq!(
                            last.as_ref(),
                            Some(prompt),
                            "the view ends on rig's prompt"
                        );
                        journal.adjacent = manifest.adjacent.first().copied();
                        Flow::patch_request(RequestPatch::new().history(sent))
                    }
                    Err(too_large) => self.stop(too_large.to_string()),
                }
            }
            // A call rig accepted. rig fires this and `ModelTurnFinished`
            // back to back, under the same rules, and only this one carries
            // the chain's word for which of its models answered, in place of
            // the provider's raw response.
            StepEvent::CompletionResponse { response, .. } => {
                let mut journal = self.journal();
                journal.latest.reply = Some(response.choice.clone());
                // rig counts its turns from one; the event log counts calls
                // from zero.
                // The chain has just recorded the request that answered.
                if let Some((call, attempt)) = self.slot.answered_by() {
                    journal.calls.push(CallUsage {
                        index: ctx.turn().saturating_sub(1) as u64,
                        model: response.raw_response.model.clone(),
                        call_id: call,
                        attempt_id: attempt,
                        usage: self.slot.ledger().usage_of(attempt),
                    });
                }
                Flow::cont()
            }
            StepEvent::ToolCall {
                tool_name, args, ..
            } => {
                let mut journal = self.journal();
                journal.latest.started += 1;
                // Not counted against the cap: it did not run.
                if self.interrupts.turn_stopped() {
                    return Flow::skip(CALL_STOPPED_TURN);
                }
                if self.calls.fetch_add(1, Ordering::SeqCst) >= self.max {
                    return Flow::skip(format!(
                        "[outrig] tool call not executed: per-round tool-call max ({}) \
                         was reached before this call could run. The user may continue \
                         with a fresh max; repeat the tool call if still needed.",
                        self.max
                    ));
                }
                journal.ran = true;
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
