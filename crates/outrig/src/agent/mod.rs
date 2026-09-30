//! The agent loop: resolve a model, build an agent whose only tool submits
//! Python to the session's interpreter, and drive rounds.
//!
//! The user reaches the agent through its `user` channel ([`UserChannel`]),
//! not through the prompt. A round opens by telling the model how many
//! messages wait there, and its code reads them.
//!
//! The conversation is kept whole in a store ([`History`]) the rounds commit
//! to, and mirrored into the interpreter for the agent's code to read. Each
//! model call is sent a view of it: the first rounds, the most recent, and
//! what the agent promoted.
//!
//! A copy of `outrig-cli`'s loop rather than a move of it, so the 0.2.x line
//! keeps editing its own without conflict; `plan/phase/0003-python/` records
//! why. rig stays a private dependency: nothing rig-typed crosses
//! [`PythonAgent`]'s signatures, and [`AgentError`] renders rig's error to text
//! at the point it is caught.

mod budget;
mod build;
mod channel;
mod history;
mod orientation;
mod resolve;
mod round;
mod tool;

use std::error::Error;
use std::path::Path;
use std::sync::{Arc, PoisonError};
use std::time::Duration;

use rig::completion::PromptError;
use rig::tool::ToolDyn;

use crate::Outrig;
use crate::config::Config;
use crate::error::OutrigError;
use crate::python::host::{Interpreter, InterpreterError};

pub use self::channel::UserChannel;

use self::budget::Budget;
use self::build::RigAgent;
use self::channel::Announcer;
use self::history::{History, Window};
use self::resolve::{LlmResolveError, ResolvedAgent};
use self::round::RoundEnd;
use self::tool::{Interrupts, ObserverSlot, SubmitPython};

/// An agent that acts by writing Python, driven one round at a time.
///
/// Its only tool runs source in the session's persistent interpreter, in the
/// primary container, so names the agent binds survive from one round to the
/// next alongside the conversation.
///
/// **Provisional.** This is the entry point `outrig-cli` drives, and it exists
/// because the loop lives in this crate and the binary has to reach it -- not
/// as an interface to build on. It will change shape without notice while the
/// agent loop is being built out.
pub struct PythonAgent {
    agent: RigAgent,
    /// The whole conversation, which rounds commit to as they go.
    history: History,
    /// What each model call may carry of it.
    budget: Arc<Budget>,
    tool_call_max: usize,
    /// The output-token ceiling the model is held to: the configured one,
    /// filled in or lowered to what the model publishes.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "0003-13 records it as an event")
    )]
    max_tokens: Option<u32>,
    /// The concrete `[models.<name>]` row the agent runs against.
    model: String,
    python_version: String,
    container_name: String,
    /// Shared with the tool, which calls what [`PythonAgent::on_submit`] puts
    /// here.
    on_submit: ObserverSlot,
    /// Shared with the tool and each round's hook; what
    /// [`PythonAgent::interrupter`] presses.
    interrupts: Interrupts,
    /// The user's end of the agent's `user` channel, handed out by
    /// [`PythonAgent::user_channel`].
    user: UserChannel,
    /// What the model has been told of what waits there. Shared with the tool.
    announcer: Announcer,
}

impl PythonAgent {
    /// Start the interpreter in `outrig`'s primary container and build an agent
    /// over it.
    ///
    /// `agent` names an `[agents.<name>]` block, or `None` for a session that
    /// names none. `model` overrides the model the agent or `default-model`
    /// would choose. Resolution runs first, so a config that names no usable
    /// model fails before anything starts in the container.
    pub async fn start(
        outrig: &Outrig,
        config: &Config,
        agent: Option<&str>,
        model: Option<&str>,
    ) -> Result<PythonAgent, Box<dyn Error + Send + Sync>> {
        let resolved = resolve::resolve_agent(config, agent, model)?;
        let primary = outrig.primary();
        let interpreter = Interpreter::start(primary).await?;
        // A container launched without a workspace holds an empty path.
        let workspace = Some(primary.container_workspace()).filter(|w| !w.as_os_str().is_empty());
        Ok(Self::build(
            &resolved,
            interpreter,
            primary.name(),
            workspace,
        )?)
    }

    /// Whether [`PythonAgent::start`] would resolve a model from these
    /// arguments, checked without starting anything.
    ///
    /// Resolution reads nothing from a container, so a caller can run this
    /// before it pulls an image or launches one, and fail on a config that
    /// names no usable model before paying for either.
    pub fn check(
        config: &Config,
        agent: Option<&str>,
        model: Option<&str>,
    ) -> Result<(), Box<dyn Error + Send + Sync>> {
        resolve::resolve_agent(config, agent, model)?;
        Ok(())
    }

    /// The `[models.<name>]` row the agent runs against. An alias has already
    /// been resolved to one of its models, so this never names an alias.
    pub fn model(&self) -> &str {
        &self.model
    }

    /// The Python version the interpreter reported when it started, such as
    /// `3.13.15`.
    pub fn python_version(&self) -> &str {
        &self.python_version
    }

    /// The name of the container the interpreter runs in: `outrig`'s primary.
    pub fn container_name(&self) -> &str {
        &self.container_name
    }

    /// Call `observer` with the source of each submission the interpreter
    /// accepts, as it starts running. A call the tool-call cap refuses is not
    /// reported, because it never reaches the interpreter.
    pub fn on_submit(&mut self, observer: impl Fn(&str) + Send + Sync + 'static) {
        *self
            .on_submit
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(Box::new(observer));
    }

    /// A function that stops the Python the current round is waiting on --
    /// what Ctrl-C does -- callable from another task while
    /// [`PythonAgent::round`] runs.
    ///
    /// The first call cancels that execution, and interrupts it too if its
    /// event loop has stopped turning; the model then reads how it ended, and
    /// the round goes on. A second call on the same execution stops waiting
    /// for it: it keeps the interpreter until it finishes, and its result
    /// reaches the model later. Each returns what it did, as a sentence for
    /// the user.
    ///
    /// It returns `None` when no Python is running -- the model is being
    /// called, or no round is -- and does nothing. Dropping the round's future
    /// is how to stop one there.
    pub fn interrupter(&self) -> impl Fn() -> Option<String> + Send + Sync + 'static {
        let interrupts = self.interrupts.clone();
        move || interrupts.press()
    }

    /// The user's end of the agent's `user` channel: how what the user types
    /// reaches the agent, and how what the agent sends reaches them. Every
    /// call hands out the same channel.
    pub fn user_channel(&self) -> UserChannel {
        self.user.clone()
    }

    /// Drive one round, if anything was sent on the user channel since the
    /// model was last told what waits there: the model, told how many messages
    /// wait -- never what they say -- and whatever Python it submits, then its
    /// reply. `None`, without calling the model, when nothing new arrived or
    /// the agent's code has already read what did.
    ///
    /// A message arriving while the round runs is announced in the next result
    /// the model reads, and ends a `runtime.wait` the round's code is in: that
    /// call returns, and the round goes on. One arriving after the last of them
    /// is left for the next call, which is why a caller asks again once a round
    /// ends.
    ///
    /// The reply is the model's own text: commentary, where a message the agent
    /// means the user to have is one it sends. A round cut short by the
    /// tool-call cap keeps what it managed, and its reply ends `(round ended:
    /// <reason>)`.
    ///
    /// Each model call is sent this round, any turn the agent's code promoted
    /// and has not demoted, and the conversation's first two rounds and the
    /// six before this one -- as much of that as fits the model's context
    /// window, after room for the reply. What does not fit goes in that order
    /// reversed: the window first, nearest the part already left out, then
    /// promotions, oldest first, then this round's earlier turns. The rest
    /// stays in the interpreter, where the agent's code reads all of it, and
    /// the opening line says how many turns were left out.
    ///
    /// The turn a call answers -- the model's last tool calls and their
    /// results -- is never left out. When it cannot fit on its own, the call is
    /// not made: the round ends, keeping what it did, and its reply ends
    /// `(round ended: <reason>)` naming the turn. Later rounds leave that turn
    /// out like any other, so the session goes on.
    ///
    /// The conversation belongs to the agent rather than to the round, which
    /// commits each turn as it completes. An error leaves the conversation as
    /// it was if the round had run no Python. If it had, the completed tool
    /// calls and their results are kept, because what they did stands --
    /// nothing is rolled back. Either way the messages the round did not read
    /// are still waiting, and the next round announces them again.
    ///
    /// A round whose future is dropped before it returns keeps everything
    /// before it and its completed tool calls the same way. Python it was
    /// waiting on keeps running, so stopping that is
    /// [`PythonAgent::interrupter`]'s.
    pub async fn round(&mut self) -> Result<Option<String>, Box<dyn Error + Send + Sync>> {
        let Some(announcement) = self.announcer.opening().await? else {
            return Ok(None);
        };
        let opening = self.history.open_round(&self.budget, |omitted| {
            orientation::opening(&announcement, omitted)
        });
        let RoundEnd { reply, stopped } = self
            .agent
            .round(
                opening,
                &self.history,
                &self.budget,
                self.tool_call_max,
                &self.interrupts,
            )
            .await?;
        // Only now: a round that failed or was dropped may have left the
        // model unaware of what it announced, and the next one says it again.
        self.announcer.keep();
        Ok(Some(match stopped {
            None => reply,
            Some(reason) if reply.trim().is_empty() => format!("(round ended: {reason})"),
            Some(reason) => format!("{reply}\n(round ended: {reason})"),
        }))
    }

    /// [`PythonAgent::start`] over an interpreter the caller started on the
    /// host, which has no container and no workspace.
    #[cfg(test)]
    pub(crate) fn with_interpreter(
        interpreter: Interpreter,
        config: &Config,
        agent: Option<&str>,
        model: Option<&str>,
    ) -> Result<Self, AgentError> {
        let resolved = resolve::resolve_agent(config, agent, model)?;
        Self::build(&resolved, interpreter, "(host)", None)
    }

    fn build(
        resolved: &ResolvedAgent,
        interpreter: Interpreter,
        container_name: &str,
        workspace: Option<&Path>,
    ) -> Result<Self, AgentError> {
        let python_version = interpreter.version().to_string();
        let announcer = Announcer::new(interpreter.clone());
        let user = UserChannel::new(interpreter.clone());
        let window = Window::DEFAULT;
        let history = History::new(interpreter.clone(), window);
        let tool = SubmitPython::new(
            interpreter,
            resolved.tool_result_max_bytes,
            announcer.clone(),
        );
        let on_submit = tool.observer_slot();
        let interrupts = tool.interrupts();
        let preamble = orientation::preamble(workspace, resolved.preamble.as_deref(), window);
        let overhead = budget::overhead(
            &preamble,
            [
                tool.name().as_str(),
                tool.description().as_str(),
                &tool.parameters().to_string(),
            ],
        );
        let built = build::build_agent(
            resolved,
            &preamble,
            vec![Box::new(tool) as Box<dyn ToolDyn>],
        )?;
        // After building: the reply's reserve is the ceiling that reaches the
        // wire, which building may have filled in or lowered.
        let budget = Budget::new(&resolved.candidate, built.max_tokens, overhead)?;
        tracing::debug!(
            model = %budget.model,
            window = budget.window,
            assumed = budget.window_assumed,
            reserve = budget.reserve,
            overhead = budget.overhead,
            "each model call is held to this budget"
        );
        Ok(Self {
            agent: built.agent,
            history,
            budget: Arc::new(budget),
            tool_call_max: resolved.tool_call_max,
            max_tokens: built.max_tokens,
            model: resolved.candidate.model_name.clone(),
            python_version,
            container_name: container_name.to_string(),
            on_submit,
            interrupts,
            user,
            announcer,
        })
    }
}

/// Why starting an agent or a round failed.
///
/// Crate-private: [`PythonAgent`] boxes it, so no variant is a commitment.
/// rig's errors are rendered to text on the way in rather than carried, so a
/// rig release cannot change what crosses the boundary.
#[derive(Debug, thiserror::Error)]
pub(crate) enum AgentError {
    #[error(transparent)]
    Resolve(#[from] LlmResolveError),

    #[error(transparent)]
    Outrig(#[from] OutrigError),

    #[error(transparent)]
    Interpreter(#[from] InterpreterError),

    /// The interpreter did not say what waits on the agent's channels.
    #[error("the Python interpreter did not say what messages are waiting within {0:?}")]
    Unanswered(Duration),

    /// A model call failed before the round ran any Python. The messages it
    /// announced are still waiting.
    #[error(
        "agent round failed: {0}. The messages it was told of are still waiting; send another \
         to try again"
    )]
    Prompt(String),

    /// A model call failed after the round had run Python. What ran is kept
    /// in the conversation, and a message that repeated an earlier one would
    /// ask for it again.
    #[error(
        "agent round failed after it had already run Python: {0}. What it ran is kept in the \
         conversation, and the messages it did not read are still waiting -- send another to \
         continue rather than repeating one"
    )]
    PromptAfterWork(String),
}

/// Rendered rather than carried: a `PromptError` holds rig's types and, on some
/// paths, the whole conversation.
impl From<PromptError> for AgentError {
    fn from(e: PromptError) -> Self {
        AgentError::Prompt(e.to_string())
    }
}

#[cfg(test)]
mod mock_http;

#[cfg(test)]
#[path = "agent_tests.rs"]
mod agent_tests;
