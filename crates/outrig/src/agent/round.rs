//! One round: a prompt, the model and its tool calls, and the reply.
//!
//! Copied from `outrig-cli`'s `llm.rs` down to what a round needs: the
//! tool-call cap and the partial history a stopped round keeps. The subagent
//! machinery -- labels, steering, the repeat breaker -- is not here, and
//! neither is anything that recovers from a failing endpoint, which arrives
//! with retry.
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
use rig::agent::{Agent, AgentHook, Flow, HookContext, RequestPatch, StepEvent, StepEventKind};
use rig::completion::message::{
    AssistantContent, ToolCall, ToolResult, ToolResultContent, UserContent,
};
use rig::completion::{CompletionModel, Message, Prompt, PromptError};

use super::AgentError;
use super::budget::Budget;
use super::build::RigAgent;
use super::history::{self, Adjacent, History};
use super::tool::{self, Interrupts};
use crate::events::{CallUsage, Event};

/// How a round ended.
pub(crate) struct RoundEnd {
    /// The model's closing text.
    pub(crate) reply: String,
    /// Why the round was cut short, when it was. `None` means the model
    /// finished on its own.
    pub(crate) stopped: Option<String>,
}

impl RigAgent {
    /// Run one round that `opening` opens, committing its turns to `history`
    /// as they complete. `history` has begun the round on the same message
    /// ([`History::open_round`]): rig's first message is the store's opening.
    ///
    /// A round cut short -- the tool-call cap, or rig's own turn budget -- keeps
    /// what it managed, so the next prompt can carry on from it. Any other
    /// failure is an error. It leaves the store as it was when the round had
    /// run no Python; otherwise the store keeps the tool calls that completed,
    /// since what they did stands and nothing is rolled back. A round whose
    /// future is dropped before it returns -- Ctrl-C at the REPL while no
    /// Python runs -- keeps them the same way, as it is dropped.
    ///
    /// Each model call is sent what `history` assembles for it within
    /// `budget`. A call whose latest turn would not fit on its own is not
    /// made: the round ends there, keeping its turns, and says which turn it
    /// was.
    ///
    /// An interrupt relayed through `interrupts` while a call waits on Python
    /// stops that execution, and the round goes on: the model reads how it
    /// ended. The turn's later calls are not run.
    pub(crate) async fn round(
        &self,
        opening: Message,
        history: &History,
        budget: &Arc<Budget>,
        tool_call_max: usize,
        interrupts: &Interrupts,
    ) -> Result<RoundEnd, AgentError> {
        let hook = RoundHook::new(
            tool_call_max,
            interrupts.clone(),
            history.clone(),
            Arc::clone(budget),
        );
        match self {
            RigAgent::OpenAi(agent) => run_round(agent, opening, hook).await,
            RigAgent::Anthropic(agent) => run_round(agent, opening, hook).await,
        }
    }
}

async fn run_round<M: CompletionModel + 'static>(
    agent: &Agent<M>,
    opening: Message,
    hook: RoundHook,
) -> Result<RoundEnd, AgentError> {
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
    // Waited for while the round can still be dropped and keep what it ran:
    // from the next line on, nothing may wait.
    unfinished.hook.history.events().ready().await;
    unfinished.armed = false;
    let hook = &unfinished.hook;

    let (ended, calls, usage) = match result {
        Ok(response) => {
            hook.flush(
                &response
                    .messages
                    .expect("rig populates messages on extended_details"),
            );
            let calls = response
                .completion_calls
                .iter()
                .map(|call| (call.call_index, call.usage))
                .collect();
            // A hook stop normally surfaces as an error, but reading the reason
            // back unconditionally means a stop can never be lost to a path
            // that ends the run cleanly instead.
            let end = RoundEnd {
                reply: response.output,
                stopped: hook.stop_reason(),
            };
            (Ok(end), calls, Some(response.usage))
        }
        // Only a run that succeeded has rig's account of its calls; any other
        // has the hook's, of each call's turn as rig accepted it.
        Err(err) => (stopped_short(err, hook), hook.calls_so_far(), None),
    };
    hook.record_end(&ended, calls, usage);
    ended
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
            let events = self.hook.history.events();
            if events.is_on() {
                events.emit(Event::ModelRoundDropped {
                    round: self.hook.round,
                    calls: call_usages(self.hook.calls_so_far()),
                });
            }
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
fn stopped_short(err: PromptError, hook: &RoundHook) -> Result<RoundEnd, AgentError> {
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
    Ok(RoundEnd {
        reply: String::new(),
        stopped: Some(reason),
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
    /// What each of the round's model calls used, as rig reported it when
    /// the call's turn was accepted: the only account of a round that does not
    /// end in rig's response.
    calls: Vec<(usize, rig::completion::Usage)>,
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
    fn new(max: usize, interrupts: Interrupts, history: History, budget: Arc<Budget>) -> Self {
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
        }
    }

    /// What the round's model calls have used so far, by each call's place.
    fn calls_so_far(&self) -> Vec<(usize, rig::completion::Usage)> {
        self.journal().calls.clone()
    }

    /// Record how the round ended, with what each of `calls` used and, when the
    /// provider's account of the whole run is to hand, `usage`.
    fn record_end(
        &self,
        ended: &Result<RoundEnd, AgentError>,
        calls: Vec<(usize, rig::completion::Usage)>,
        usage: Option<rig::completion::Usage>,
    ) {
        let events = self.history.events();
        if !events.is_on() {
            return;
        }
        let usage = usage.unwrap_or_else(|| {
            calls
                .iter()
                .fold(rig::completion::Usage::new(), |sum, (_, usage)| {
                    sum + *usage
                })
        });
        let input_tokens_max = calls
            .iter()
            .map(|(_, usage)| usage.input_tokens)
            .max()
            .unwrap_or(0);
        let calls = call_usages(calls);
        let error;
        events.emit(match ended {
            Ok(RoundEnd { stopped, .. }) => Event::ModelRoundCompleted {
                round: self.round,
                stopped: stopped.as_deref(),
                usage: usage.into(),
                calls,
                input_tokens_max,
            },
            Err(failed) => {
                error = failed.to_string();
                Event::ModelRoundFailed {
                    round: self.round,
                    error: &error,
                    calls,
                }
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
    /// call's view put one role after itself in the conversation: most
    /// providers take that, and one that requires turns to alternate may not,
    /// so the refusal may be for it. It says what the conversation held rather
    /// than what the wire carried, which differs by provider ([`Adjacent`]).
    fn refusal_hint(&self, err: &PromptError) -> String {
        let refused = err
            .provider_response_status()
            .is_some_and(|status| matches!(status.as_u16(), 400 | 422));
        match self.journal().adjacent {
            Some(adjacent) if refused => format!(
                ". The conversation it was sent left turns out, which put {adjacent}; a provider \
                 or gateway that requires the user's and the model's turns to alternate may \
                 refuse that (see `outrig run-new` in doc/reference/cli.md)"
            ),
            _ => String::new(),
        }
    }
}

impl<M: CompletionModel> AgentHook<M> for RoundHook {
    fn observes(&self, kind: StepEventKind) -> bool {
        matches!(
            kind,
            StepEventKind::CompletionCall
                | StepEventKind::ModelTurnFinished
                | StepEventKind::ToolCall
                | StepEventKind::ToolResult
        )
    }

    async fn on_event(&self, _ctx: &HookContext, event: StepEvent<'_, M>) -> Flow {
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
                // Before the journal's lock, which an await cannot be held
                // across: the turn committed below and the call's manifest
                // are both recorded.
                self.history.events().ready().await;
                self.interrupts.clear_turn();
                // What rig holds is whole turns by now, since a call is made
                // only once the last one's tool calls have returned. Once they
                // are committed, the store holds everything this call answers.
                let mut journal = self.journal();
                journal.commit(round.iter().chain([prompt]), &self.history);
                journal.latest = Latest::default();
                match self.history.assemble(&self.budget) {
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
            StepEvent::ModelTurnFinished {
                turn,
                content,
                usage,
            } => {
                let mut journal = self.journal();
                journal.latest.reply = Some(content.clone());
                // rig counts its turns from one, and its completion calls from
                // zero; the event log counts both from zero.
                journal.calls.push((turn.saturating_sub(1), usage));
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

/// Each call's place and what it used, as the event log records them.
fn call_usages(calls: Vec<(usize, rig::completion::Usage)>) -> Vec<CallUsage> {
    calls
        .into_iter()
        .map(|(index, usage)| CallUsage {
            index,
            usage: usage.into(),
        })
        .collect()
}
