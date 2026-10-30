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
//! has already kept every turn it finished; and on every model call the chain
//! replaces what rig would send with the view the store assembles from them,
//! held to the budget of the model the call goes to. Nothing rig returns is
//! compared against the store.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use rig::agent::{
    AgentHook, CompletionCallAction, CompletionCallEvent, DispatchAction, DispatchEvent,
    HookContext, ModelTurnAction, ModelTurnFinished, OutcomeAction, OutcomeEvent, RequestPatch,
    StepEventKind,
};
use rig::completion::message::{
    AssistantContent, AssistantMessage, CallId as RigCallId, Origin, ToolResultContent, UserContent,
};
use rig::completion::{Message, PromptError};
use rig::run::policy::{InvalidToolCallAction, InvalidToolCallContext, InvalidToolCallReason};

use super::AgentError;
use super::budget::Budget;
use super::build::RigAgent;
use super::failover;
use super::history::{self, History};
use super::ledger::{CallSlot, Refusal};
use super::retry::{BudgetSpent, prompt_failure_text};
use super::tool::Interrupts;
use crate::config::RoleAlternation;
use crate::harness::event::{AttemptId, CallId, CallUsage, Payload, Usage};

/// Why a child has nothing left for its round to do, when it has none: asked
/// before each model call, and the round ends there, as the tool-call cap
/// ends it, when it answers.
pub(crate) type Quiet = Arc<dyn Fn() -> Option<String> + Send + Sync>;

/// How a round ended.
pub(crate) struct Finished {
    /// The model's closing text.
    pub(crate) reply: String,
    /// Why the round was cut short, when it was. `None` means the model
    /// finished on its own.
    pub(crate) stopped: Option<String>,
    /// The final message's reasoning, when it held no text: a round the model
    /// ended after thinking is not reported as one that said nothing. (One
    /// cut off at its output ceiling while thinking fails instead: rig takes
    /// reasoning alone for no answer, and the error names the ceiling.)
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
/// before it returns -- Ctrl-C at the REPL while no Python runs, or a child's
/// release that its model call outlasted -- keeps them the same way, as it is
/// dropped.
///
/// A child's round also ends when `quiet` says it has nothing left to do:
/// its submission was accepted and nothing else waits, or it was released. It
/// is asked before each model call, so the call that would only speak after
/// the work is not sent and the turn that completed it is kept whole.
///
/// Each model call is sent what the chain assembles from `history` within the
/// allowance of the model it goes to. A call whose latest turn fits no model's
/// on its own is not made: the round ends there, keeping its turns, and says
/// which turn it was.
///
/// An interrupt relayed through `interrupts` while a call waits on Python
/// stops that execution, and the round goes on: the model reads how it ended.
/// The turn's later calls are not run.
pub(crate) async fn round(
    agent: &RigAgent,
    opening: Message,
    history: &History,
    tool_call_max: usize,
    interrupts: &Interrupts,
    quiet: Option<&Quiet>,
) -> Result<Finished, AgentError> {
    let slot = agent.model.slot().clone();
    slot.open_round(history.round());
    let hook = RoundHook::new(
        tool_call_max,
        interrupts.clone(),
        history.clone(),
        slot,
        quiet.cloned(),
    );
    // rig's own budget is a backstop set above the hook's, so the hook -- with
    // its message the model can read -- is the limiter that fires first. It
    // counts every model call, the first included.
    let max_turns = hook.max + 2;
    // Cloned rather than moved: the clone shares the hook's state, so it can
    // still be asked afterwards whether the stop was OutRig's doing.
    let mut unfinished = KeptIfDropped {
        hook: hook.clone(),
        armed: true,
    };
    let result = agent
        .agent
        .prompt(opening)
        // Nothing from before the round: the chain sends that, on every call.
        .history(Vec::<Message>::new())
        .max_turns(max_turns)
        .add_hook(hook)
        .await;
    // From the next line on, nothing may wait.
    unfinished.armed = false;
    let hook = &unfinished.hook;

    let ended = match result {
        Ok(response) => {
            hook.flush(&response.messages);
            let reply = response.output();
            // A hook stop normally surfaces as an error, but reading the reason
            // back unconditionally means a stop can never be lost to a path
            // that ends the run cleanly instead.
            Ok(Finished {
                reasoning: is_blank(&reply)
                    .then(|| recover_non_text(&response.content))
                    .flatten(),
                reply,
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

/// The text parts of `content`, joined: what a model said, less its tool
/// calls and reasoning.
fn spoken(content: &[AssistantContent]) -> String {
    content
        .iter()
        .filter_map(|part| match part {
            AssistantContent::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect()
}

/// Whether `reply` says nothing a person could read.
fn is_blank(reply: &str) -> bool {
    reply.trim().is_empty()
}

/// The reasoning in `content`, the final message of a round whose text was
/// blank, joined into one block; `None` when there is none worth showing.
///
/// rig's `output` is the final message's text parts concatenated, so a turn
/// the model ended after thinking arrives as nothing, though it produced, and
/// was billed for, reasoning. Reasoning the provider withheld has no text to
/// show.
fn recover_non_text(content: &[AssistantContent]) -> Option<String> {
    let recovered = content
        .iter()
        .filter_map(|part| match part {
            AssistantContent::Reasoning(reasoning) if !reasoning.redacted => {
                (!is_blank(&reasoning.text)).then(|| reasoning.text.clone())
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
            let spent = self.hook.close();
            self.hook
                .history
                .events()
                .emit_with(|| Payload::ModelRoundDropped {
                    round: self.hook.round,
                    calls: self.hook.calls_so_far(),
                    usage: spent.usage,
                    attempts: spent.attempts,
                    descendants: spent.descendants,
                    total: spent.total,
                });
        }
    }
}

/// Turn a round-ending error into a round cut short, or pass it on.
///
/// `hook` is the round's own hook, consulted to tell OutRig's deliberate stops
/// from a cancellation rig raised for itself. Both arrive as
/// `PromptError::Cancelled`, and only a stop the hook recorded is one OutRig
/// chose; anything else is a fault and stays an error. The exception is a call
/// the chain did not make because no model's window could hold its latest
/// turn ([`failover::unfit`]), which ends the round as the cap does, keeping
/// the turn: the hook committed every turn the call answers before the chain
/// was asked. So does a request the session's spent token budget refused
/// ([`BudgetSpent`]), which ends the round as the hook's own check before a
/// call does. A provider's refusal of a request whose view put one role
/// after itself says so, since that is the one shape of request this loop sends
/// that a strict provider can refuse.
///
/// A failed model call arrives as the chain's report of it
/// (`PromptError::Report`), which names its class and whether it may clear;
/// one rig failed itself -- a reply it would not accept -- as the provider's
/// error. An error after the round has run Python keeps what it ran. Most
/// errors carry no history, so what is kept is what the hook committed and
/// journaled: see [`RoundHook::keep_what_ran`]. Dropping it would leave the
/// interpreter holding what the calls did and the conversation saying they
/// never ran, which is how a resent prompt runs a `git push` twice.
fn stopped_short(err: PromptError, hook: &RoundHook) -> Result<Finished, AgentError> {
    let (reason, chat_history) = match (err, hook.stop_reason()) {
        (PromptError::Cancelled { chat_history, .. }, Some(ours)) => (ours, chat_history),
        (
            PromptError::MaxTurns {
                max_turns,
                chat_history,
                ..
            },
            _,
        ) => (cap_reason(max_turns), chat_history),
        // As for an unfit call, every turn the call answers is in the store.
        (refused, _) if budget_spent(&refused) => (
            budget_reason(hook.slot.ledger().budget().unwrap_or_default()),
            Vec::new(),
        ),
        (other, _) => {
            let Some(reason) = unfit(&other) else {
                let text = format!(
                    "{}{}",
                    prompt_failure_text(&other),
                    hook.refusal_hint(&other)
                );
                let ran = hook.keep_what_ran();
                return Err(match (ran, may_clear(&other)) {
                    (false, true) => AgentError::Prompt(text),
                    (true, true) => AgentError::PromptAfterWork(text),
                    (false, false) => AgentError::PromptTerminal(text),
                    (true, false) => AgentError::PromptAfterWorkTerminal(text),
                });
            };
            // Nothing is left to flush: every turn the call answers is in the
            // store, and the round's opening, if no turn has begun with it,
            // becomes a turn of its own as the round finishes.
            (reason, Vec::new())
        }
    };
    hook.flush(&chat_history);
    Ok(Finished {
        reply: String::new(),
        stopped: Some(reason),
        reasoning: None,
    })
}

/// Whether `err` is the chain's refusal of a request for the session's spent
/// token budget.
fn budget_spent(err: &PromptError) -> bool {
    match err {
        PromptError::Report(report) => failover::budget_spent_report(report),
        PromptError::Provider(failed) => BudgetSpent::of(failed),
        _ => false,
    }
}

/// Why the chain did not make the call that failed with `err`, when it was
/// because no model's window could hold the call's latest turn.
fn unfit(err: &PromptError) -> Option<String> {
    match err {
        PromptError::Report(report) => failover::unfit_report(report).map(str::to_string),
        PromptError::Provider(failed) => failover::unfit(failed),
        _ => None,
    }
}

/// Whether sending again after `err` could end differently.
///
/// A failed model call answers by its kind: a rate limit that outlasted the
/// budget may lift, a fallback that could not hold the round's latest turn
/// will be sent a view without it next round, and a key every model refuses
/// will not clear. The chain says so of its report; rig says so of a failure
/// of its own. Any other error keeps the advice every failed round has had.
fn may_clear(err: &PromptError) -> bool {
    match err {
        PromptError::Report(report) => report.retryable,
        PromptError::Provider(failed) => failover::may_clear(failed),
        _ => true,
    }
}

fn cap_reason(max: usize) -> String {
    format!("tool-call iteration max ({max}) reached")
}

/// Why a round stopped once the session's token budget was spent: before a
/// model call, or as the call's request was refused.
pub(crate) fn budget_reason(budget: u64) -> String {
    format!(
        "token-budget ({budget} tokens) is spent: no model call is made from here on in this \
         session"
    )
}

/// What a round's attempts used: its own, and those of the children working
/// under it, each counted once.
struct Spent {
    usage: Option<Usage>,
    attempts: Vec<AttemptId>,
    descendants: Vec<AttemptId>,
    total: Option<Usage>,
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
/// round's turns as they complete -- so the chain finds in the store everything
/// each model call answers -- and journals what the round has done that is not
/// a turn yet.
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
    /// The round's number in the conversation.
    round: u32,
    /// Where the round's calls are named and its attempts recorded.
    slot: CallSlot,
    /// Whether the child has nothing left to do; none for the primary.
    quiet: Option<Quiet>,
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
    /// Each of the round's model calls rig accepted: its place, the model that
    /// answered it, and what it used, as the provider reported it.
    calls: Vec<CallUsage>,
}

/// The round's latest model call, while it is not yet a turn in the store.
#[derive(Default)]
struct Latest {
    /// Its reply, once rig accepted it, while its tool calls run.
    reply: Option<AssistantMessage>,
    /// What the chain said of the request that answered the call, as its reply
    /// arrived, kept until rig accepts the reply as the turn.
    answer: Option<Answer>,
    /// The reply's tool calls that have started, by id.
    started: Vec<RigCallId>,
    /// The results of the calls that came back, by id, as the model reads
    /// them -- and of each call rig answered without running it, since its
    /// arguments were no JSON object or named no tool, what it was answered.
    /// By id, not by place: such a call stays in the reply but is never
    /// dispatched, so a count of dispatches would pair the calls after it with
    /// the results before.
    results: Vec<(RigCallId, String)>,
}

/// Which request answered a model call: the `[models.<name>]` row the chain
/// sent it to, the call and attempt the ledger named, and the provider and
/// model the reply itself names.
struct Answer {
    model: Option<String>,
    ids: Option<(CallId, AttemptId)>,
    origin: Origin,
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
        slot: CallSlot,
        quiet: Option<Quiet>,
    ) -> Self {
        Self {
            calls: Arc::new(AtomicUsize::new(0)),
            stopped: Arc::default(),
            max,
            journal: Arc::new(Mutex::new(Journal {
                committed: 1,
                ran: false,
                latest: Latest::default(),
                calls: Vec::new(),
            })),
            interrupts,
            round: history.round(),
            history,
            slot,
            quiet,
        }
    }

    /// The round's model calls so far, by each call's place.
    fn calls_so_far(&self) -> Vec<CallUsage> {
        self.journal().calls.clone()
    }

    /// End the round, and say what its attempts used, each counted once:
    /// its own, and its children's while it ran. A child's attempt from here
    /// on is published apart, naming this round.
    fn close(&self) -> Spent {
        let (attempts, descendants) = self.slot.close_round();
        let ledger = self.slot.ledger();
        let all: Vec<AttemptId> = attempts.iter().chain(&descendants).copied().collect();
        Spent {
            usage: ledger.total(&attempts),
            total: ledger.total(&all),
            attempts,
            descendants,
        }
    }

    /// Record how the round ended: each call it made, and what its attempts
    /// used.
    fn record_end(&self, ended: &Result<Finished, AgentError>) {
        let spent = self.close();
        self.history.events().emit_with(|| {
            let calls = self.calls_so_far();
            let Spent {
                usage,
                attempts,
                descendants,
                total,
            } = spent;
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
                    descendants,
                    total,
                },
                Err(failed) => Payload::ModelRoundFailed {
                    round: self.round,
                    error: failed.to_string(),
                    calls,
                    usage,
                    attempts,
                    descendants,
                    total,
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
            ..
        } = std::mem::take(&mut journal.latest);
        if let (true, Some(reply)) = (journal.ran, reply) {
            let calls: Vec<_> = reply.tool_calls().collect();
            let answered = |call: &rig::completion::message::ToolCall| {
                results
                    .iter()
                    .find(|(id, _)| *id == call.id)
                    .map(|(_, text)| text.as_str())
            };
            let incomplete = calls.iter().any(|call| answered(call).is_none());
            let answers: Vec<UserContent> = calls
                .into_iter()
                .map(|call| {
                    let text = match answered(call) {
                        Some(result) => result,
                        None if started.contains(&call.id) => CALL_UNFINISHED,
                        None => CALL_NOT_RUN,
                    };
                    UserContent::ToolResult(call.result(vec![ToolResultContent::text(text)]))
                })
                .collect();
            let mut turn = vec![Message::Assistant(reply)];
            if !answers.is_empty() {
                turn.push(Message::User { content: answers });
            }
            self.history.commit(turn, incomplete);
        }
        journal.ran
    }

    /// Stop the loop for `reason`, and say so to rig: the reason, for the
    /// action that carries it.
    fn stop(&self, reason: String) -> String {
        *self.stopped.lock().unwrap_or_else(PoisonError::into_inner) = Some(reason.clone());
        reason
    }

    /// Why this hook stopped the loop, if it did: the tool-call cap.
    fn stop_reason(&self) -> Option<String> {
        self.stopped
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// What to add to `err`, when a provider refused the latest call and the
    /// view it was sent put one role after itself in the conversation. For a
    /// provider whose row does not say its turns must alternate, the pair may
    /// be why, and the row's `role-alternation` is the remedy. For one whose
    /// row does, the view withheld what it could, and the pair that stands is
    /// the turn the call answers opening on a reply with nothing kept before
    /// it that ends on the user's side: its round's opening did not fit, and
    /// only a larger window mends that. Either says what the conversation held
    /// rather than what the wire carried, which differs by provider
    /// ([`history::Adjacent`]).
    ///
    /// It names the row of the first model that refused such a view, which for
    /// an alias is the head's unless the head failed some other way: the chain
    /// assembled each model's view for it, and keeps that one's.
    fn refusal_hint(&self, err: &PromptError) -> String {
        // Only a failed call has a refusal to speak of; the slot holds the
        // latest call's, which an error from anywhere else is not about.
        if !matches!(err, PromptError::Report(_) | PromptError::Provider(_)) {
            return String::new();
        }
        let Some(Refusal { budget, adjacent }) = self.slot.refusal() else {
            return String::new();
        };
        let Budget {
            model, provider, ..
        } = &budget;
        match budget.alternation {
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

impl AgentHook for RoundHook {
    /// The dispatch kinds gate `on_dispatch` and `on_outcome`; the other two
    /// methods are asked of every hook.
    fn observes(&self, kind: StepEventKind) -> bool {
        matches!(
            kind,
            StepEventKind::CompletionCall
                | StepEventKind::ModelTurnFinished
                | StepEventKind::CompletionDispatch
                | StepEventKind::ToolDispatch
        )
    }

    async fn on_completion_call(
        &self,
        _ctx: &HookContext,
        event: CompletionCallEvent<'_>,
    ) -> CompletionCallAction {
        // Before the cap, so a round the budget stops says so whatever else
        // it has spent.
        if self.slot.ledger().budget_spent() {
            let budget = self.slot.ledger().budget().unwrap_or_default();
            return CompletionCallAction::Stop(self.stop(budget_reason(budget)));
        }
        if self.calls.load(Ordering::SeqCst) > self.max {
            return CompletionCallAction::Stop(self.stop(cap_reason(self.max)));
        }
        // After the budget and the cap, which say more of why. Here rather
        // than at the tool's result, so the turn's other calls run, and the
        // model is not sent a call that could only close the work with a word
        // to nobody.
        if let Some(reason) = self.quiet.as_ref().and_then(|quiet| quiet()) {
            return CompletionCallAction::Stop(self.stop(reason));
        }
        self.interrupts.clear_turn();
        // What rig holds is whole turns by now, since a call is made only once
        // the last one's tool calls have returned. Once they are committed,
        // the store holds everything this call answers, and the chain
        // assembles each model's view from it. rig is told to send none of
        // the round's history, which the chain replaces: left alone, it
        // copies the whole round into the request, and the chain copies that
        // again for every candidate it may move to.
        let mut journal = self.journal();
        journal.commit(
            event.history.iter().map(Arc::as_ref).chain([event.prompt]),
            &self.history,
        );
        journal.latest = Latest::default();
        CompletionCallAction::Patch(RequestPatch::new().history(Vec::<Message>::new()))
    }

    /// A reply arrived, or a tool call returned. A reply here is not yet a
    /// turn rig accepted -- it may yet reject it -- so only what the chain
    /// said of the request that answered is kept, for the turn to read.
    async fn on_outcome(&self, _ctx: &HookContext, event: OutcomeEvent<'_>) -> OutcomeAction {
        if let (Some(result), Some(call)) = (event.tool_result(), event.call_id) {
            self.journal()
                .latest
                .results
                .push((call.clone(), result.output().render()));
        } else if let Some(response) = event.completion() {
            self.journal().latest.answer = Some(Answer {
                model: self.slot.answered_model(),
                ids: self.slot.answered_by(),
                origin: response.origin.clone(),
            });
        }
        OutcomeAction::Proceed
    }

    /// A reply rig accepted as the turn, before its tool calls run.
    async fn on_model_turn_finished(
        &self,
        _ctx: &HookContext,
        event: ModelTurnFinished<'_>,
    ) -> ModelTurnAction {
        let mut journal = self.journal();
        let answer = journal.latest.answer.take();
        let text = spoken(event.content);
        if !is_blank(&text) {
            self.history
                .events()
                .emit_with(|| Payload::ModelText { text });
        }
        // Kept with the provider and model that produced it, which is what
        // lets that provider replay what it wrote as its own.
        let origin = answer.as_ref().map(|answer| answer.origin.clone());
        journal.latest.reply =
            Some(AssistantMessage::new(event.content.clone()).with_origin(origin));
        // rig counts its turns from one; the event log counts calls from zero.
        if let Some(Answer {
            model,
            ids: Some((call, attempt)),
            origin,
        }) = answer
        {
            journal.calls.push(CallUsage {
                index: event.turn.saturating_sub(1) as u64,
                model: model.unwrap_or(origin.model),
                call_id: call,
                attempt_id: attempt,
                usage: self.slot.ledger().usage_of(attempt),
            });
        }
        ModelTurnAction::Continue
    }

    /// A tool call rig will not dispatch: its arguments are no JSON object, or
    /// it names no tool. It is answered here with a note saying why it did
    /// not run -- rig's own default for a tool it does not know fails the
    /// whole round, where the model could simply call the right one -- and
    /// counted against the cap as a call that ran would be: a model that
    /// keeps sending one costs a model call each time. The answer is kept
    /// beside the call's id, for a round dropped before the turn is committed
    /// ([`keep_what_ran`]). rig answers the batch's other calls as not run
    /// itself when one names no tool.
    ///
    /// [`keep_what_ran`]: RoundHook::keep_what_ran
    async fn on_invalid_tool_call(
        &self,
        _ctx: &HookContext,
        event: &InvalidToolCallContext,
    ) -> Option<InvalidToolCallAction> {
        let why = match &event.reason {
            InvalidToolCallReason::MalformedArguments { error } => format!(
                "the arguments for `{}` are not a JSON object ({error}). Call the tool again \
                 with a JSON object as its arguments",
                event.tool_name
            ),
            InvalidToolCallReason::UnknownTool => format!(
                "there is no tool named `{}`. The tools are: {}",
                event.tool_name,
                event.available_tools.join(", ")
            ),
            _ => return None,
        };
        let reason = format!("[outrig] tool call not executed: {why}.");
        self.calls.fetch_add(1, Ordering::SeqCst);
        if let Some(id) = &event.tool_call_id {
            self.journal()
                .latest
                .results
                .push((id.clone(), reason.clone()));
        }
        Some(InvalidToolCallAction::Skip { reason })
    }

    /// A tool call is about to run.
    async fn on_dispatch(&self, _ctx: &HookContext, event: DispatchEvent<'_>) -> DispatchAction {
        let Some(tool_name) = event.tool_name() else {
            return DispatchAction::Proceed;
        };
        let mut journal = self.journal();
        if let Some(id) = event.call_id {
            journal.latest.started.push(id.clone());
        }
        // Not counted against the cap: it did not run.
        if self.interrupts.turn_stopped() {
            return DispatchAction::skip(CALL_STOPPED_TURN);
        }
        if self.calls.fetch_add(1, Ordering::SeqCst) >= self.max {
            return DispatchAction::skip(format!(
                "[outrig] tool call not executed: per-round tool-call max ({}) \
                 was reached before this call could run. The user may continue \
                 with a fresh max; repeat the tool call if still needed.",
                self.max
            ));
        }
        journal.ran = true;
        tracing::debug!(
            "tool call: {tool_name}({})",
            event.tool_args().unwrap_or_default()
        );
        DispatchAction::Proceed
    }
}
