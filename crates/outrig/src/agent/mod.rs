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
//! Each call retries a provider's transient failures ([`retry`]), and one that
//! still fails moves to the next model an alias names ([`failover`]), sent a
//! view assembled for that model's own window. Every request either sends is
//! an attempt the session's [`ledger`] records.
//!
//! A copy of `outrig-cli`'s loop rather than a move of it, so the 0.2.x line
//! keeps editing its own without conflict; `plan/phase/0003-python/` records
//! why. The public way in is [`crate::harness`], which owns an [`Agent`] and
//! the container and interpreter it runs over. rig stays a private dependency:
//! nothing rig-typed crosses that surface, and [`AgentError`] renders rig's
//! error to text at the point it is caught.

mod budget;
mod build;
mod channel;
mod failover;
mod history;
pub(crate) mod ledger;
mod orientation;
mod resolve;
mod retry;
pub(crate) mod round;
mod tool;

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use rig::completion::{PromptError, ToolDefinition};
use rig::tool::ToolDyn;

use crate::config::{Config, Secrets};
use crate::error::OutrigError;
use crate::harness::Closed;
use crate::harness::event::{self, Payload};
use crate::harness::lifecycle::Lifecycle;
use crate::python::host::{Interpreter, InterpreterError, Tasks};

pub use self::channel::UserChannel;

use self::budget::Budget;
use self::build::RigAgent;
use self::channel::Announcer;
#[cfg(test)]
pub(crate) use self::history::rig_json;
use self::history::{History, Window};
use self::ledger::Ledger;
use self::resolve::LlmResolveError;
pub(crate) use self::resolve::ResolvedAgent;
use self::round::Finished;
use self::tool::{Interrupts, SubmitPython};

/// How long the interpreter has to say which tasks a round left running. It
/// answers on the agent's loop, so only a loop something keeps from turning
/// takes this long, and then the answer is that it could not tell.
const TASKS_TIMEOUT: Duration = Duration::from_secs(2);

/// Resolve the agent and model `config` names, each key through `secrets`,
/// without starting anything.
pub(crate) fn resolve(
    config: &Config,
    agent: Option<&str>,
    model: Option<&str>,
    secrets: &dyn Secrets,
) -> Result<ResolvedAgent, AgentError> {
    resolve::resolve_agent(config, agent, model, secrets)
}

/// An agent that acts by writing Python, driven one round at a time.
///
/// Its only tool runs source in the session's persistent interpreter, in the
/// primary container, so names the agent binds survive from one round to the
/// next alongside the conversation. [`crate::harness::Session`] holds one.
pub(crate) struct Agent {
    pub(crate) agent: RigAgent,
    /// The whole conversation, which rounds commit to as they go.
    pub(crate) history: History,
    /// What each model call may carry of it: the head's allowance, whose
    /// `model` is the row every call is tried against first. A call moved to
    /// another model is held to that model's, which the chain holds.
    pub(crate) budget: Arc<Budget>,
    tool_call_max: usize,
    python_version: String,
    container_name: String,
    /// Shared with the tool and each round's hook; what
    /// [`Agent::interrupter`] presses.
    interrupts: Interrupts,
    /// The user's end of the agent's `user` channel.
    user: UserChannel,
    /// What the model has been told of what waits there. Shared with the tool.
    announcer: Announcer,
    /// The interpreter itself, for what is asked of it between rounds.
    pub(crate) interpreter: Interpreter,
    /// The session's state, which a round moves through.
    lifecycle: Arc<Lifecycle>,
}

impl Agent {
    /// The session's model calls and attempts.
    #[cfg(test)]
    pub(crate) fn ledger(&self) -> &Ledger {
        self.agent.model.slot().ledger()
    }

    /// The `[models.<name>]` row every model call is tried against first.
    pub(crate) fn model(&self) -> &str {
        &self.budget.model
    }

    pub(crate) fn python_version(&self) -> &str {
        &self.python_version
    }

    pub(crate) fn container_name(&self) -> &str {
        &self.container_name
    }

    /// What stops the Python the current round waits on: see
    /// [`crate::harness::Session::interrupter`].
    pub(crate) fn interrupter(&self) -> impl Fn() -> Option<String> + Send + Sync + 'static {
        let interrupts = self.interrupts.clone();
        move || interrupts.press()
    }

    /// See [`crate::harness::Session::stop_held`].
    pub(crate) fn stop_held(&self) -> Option<String> {
        self.interrupts.stop_held(&self.interpreter)
    }

    pub(crate) fn user_channel(&self) -> UserChannel {
        self.user.clone()
    }

    /// Drive one round, if anything was sent on the user channel since the
    /// model was last told what waits there; `None`, without calling the
    /// model, when nothing was. See [`crate::harness::Session::round`]. With
    /// it, the tasks the agent's code left running when it ended, or `None`
    /// when the interpreter could not say.
    pub(crate) async fn round(&mut self) -> Result<Option<(Finished, Option<Tasks>)>, AgentError> {
        let Some(announcement) = self.announcer.opening().await? else {
            return Ok(None);
        };
        // Refused here, with nothing opened, once the session has closed.
        let running = self
            .lifecycle
            .open_round()
            .map_err(|by| AgentError::Closed(by.into()))?;
        let opening = self.history.open_round(&self.budget, |omitted| {
            orientation::opening(&announcement, omitted)
        });
        self.history.events().emit(Payload::RoundStarted {
            round: self.history.round(),
        });
        let finished = round::round(
            &self.agent,
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
        // The model has yielded; what its code left running has not.
        drop(running);
        let tasks = tokio::time::timeout(TASKS_TIMEOUT, self.interpreter.tasks())
            .await
            .ok()
            .and_then(Result::ok);
        Ok(Some((finished, tasks)))
    }

    /// An agent over `interpreter`, in the container named `container_name`
    /// whose workspace is `workspace`, recording its spend in `ledger` and
    /// moving `lifecycle` through each round.
    pub(crate) fn build(
        resolved: &ResolvedAgent,
        interpreter: Interpreter,
        container_name: &str,
        workspace: Option<&Path>,
        ledger: Ledger,
        lifecycle: Arc<Lifecycle>,
    ) -> Result<Self, AgentError> {
        let python_version = interpreter.version().to_string();
        let announcer = Announcer::new(interpreter.clone());
        let user = UserChannel::new(interpreter.clone());
        let window = Window::DEFAULT;
        let history = History::new(interpreter.clone(), window);
        let tool = SubmitPython::new(
            interpreter.clone(),
            resolved.tool_result_max_bytes,
            announcer.clone(),
            Arc::clone(&lifecycle),
        );
        let interrupts = tool.interrupts();
        let preamble = orientation::preamble(workspace, resolved.preamble.as_deref(), window);
        let definition = ToolDefinition {
            name: tool.name(),
            description: tool.description(),
            parameters: tool.parameters(),
        };
        let overhead = budget::overhead(
            &preamble,
            [
                definition.name.as_str(),
                definition.description.as_str(),
                &definition.parameters.to_string(),
            ],
        );
        // Each candidate's budget is settled in building: the reply's reserve
        // is the ceiling that reaches the wire, which building may fill in or
        // lower.
        let agent = build::build_agent(
            resolved,
            &preamble,
            vec![Box::new(tool) as Box<dyn ToolDyn>],
            &history,
            &ledger,
            overhead,
        )?;
        let budget = agent
            .model
            .budgets()
            .next()
            .expect("a chain is never empty")
            .clone();
        let events = history.events();
        events.emit(Payload::AgentStarted {
            model: budget.model.clone(),
            python: python_version.clone(),
            container: container_name.to_string(),
            tool_call_max: resolved.tool_call_max as u64,
            tool_result_max: resolved.tool_result_max_bytes as u64,
        });
        events.emit(Payload::ModelInstructions {
            model: budget.model.clone(),
            preamble: preamble.clone(),
            tools: vec![event::ToolDefinition {
                name: definition.name,
                description: definition.description,
                parameters: definition.parameters,
            }],
            max_tokens: budget.max_tokens,
        });
        Ok(Self {
            agent,
            history,
            budget: Arc::new(budget),
            tool_call_max: resolved.tool_call_max,
            python_version,
            container_name: container_name.to_string(),
            interrupts,
            user,
            announcer,
            interpreter,
            lifecycle,
        })
    }
}

/// Why starting an agent or a round failed.
///
/// Crate-private: the harness carries it inside an opaque
/// [`Failure`](crate::harness::Failure), so no variant is a commitment.
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

    /// The session was closed to new work before the round opened.
    #[error(transparent)]
    Closed(Closed),

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
pub(crate) mod mock_http;

#[cfg(test)]
#[path = "agent_tests.rs"]
mod agent_tests;
