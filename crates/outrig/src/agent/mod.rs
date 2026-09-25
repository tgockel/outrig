//! The agent loop: resolve a model, build an agent whose only tool submits
//! Python to the session's interpreter, and drive rounds.
//!
//! A copy of `outrig-cli`'s loop rather than a move of it, so the 0.2.x line
//! keeps editing its own without conflict; `plan/phase/0003-python/` records
//! why. rig stays a private dependency: nothing rig-typed crosses
//! [`PythonAgent`]'s signatures, and [`AgentError`] renders rig's error to text
//! at the point it is caught.

mod build;
mod orientation;
mod resolve;
mod round;
mod tool;

use std::error::Error;
use std::path::Path;
use std::sync::PoisonError;

use rig::completion::{Message, PromptError};
use rig::tool::ToolDyn;

use crate::Outrig;
use crate::config::Config;
use crate::error::OutrigError;
use crate::python::host::Interpreter;

use self::build::RigAgent;
use self::resolve::{LlmResolveError, ResolvedAgent};
use self::round::RoundEnd;
use self::tool::{ObserverSlot, SubmitPython};

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
    history: Vec<Message>,
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

    /// Drive one round: `prompt`, the model and whatever Python it submits, and
    /// the reply.
    ///
    /// A round cut short by the tool-call cap keeps what it managed, so the
    /// next prompt carries on from it, and its reply ends `(round ended:
    /// <reason>)`.
    ///
    /// An error leaves the conversation as it was if the round had run no
    /// Python. If it had, the completed tool calls and their results are kept,
    /// because what they did stands -- nothing is rolled back -- and the error
    /// says to continue rather than resend.
    ///
    /// A round whose future is dropped before it returns -- the REPL's Ctrl-C --
    /// keeps its completed tool calls and their results the same way.
    pub async fn round(&mut self, prompt: &str) -> Result<String, Box<dyn Error + Send + Sync>> {
        let RoundEnd { reply, stopped } = self
            .agent
            .round(prompt, &mut self.history, self.tool_call_max)
            .await?;
        Ok(match stopped {
            None => reply,
            Some(reason) if reply.trim().is_empty() => format!("(round ended: {reason})"),
            Some(reason) => format!("{reply}\n(round ended: {reason})"),
        })
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
        let tool = SubmitPython::new(interpreter, resolved.tool_result_max_bytes);
        let on_submit = tool.observer_slot();
        let preamble = orientation::preamble(workspace, resolved.preamble.as_deref());
        let built = build::build_agent(
            resolved,
            &preamble,
            vec![Box::new(tool) as Box<dyn ToolDyn>],
        )?;
        Ok(Self {
            agent: built.agent,
            history: Vec::new(),
            tool_call_max: resolved.tool_call_max,
            max_tokens: built.max_tokens,
            model: resolved.candidate.model_name.clone(),
            python_version,
            container_name: container_name.to_string(),
            on_submit,
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

    #[error("agent prompt failed: {0}")]
    Prompt(String),

    /// A model call failed after the round had run Python. What ran is kept
    /// in the conversation, so resending the prompt would ask for it again.
    #[error(
        "agent prompt failed after this round had already run Python: {0}. What it ran is kept \
         in the conversation -- send a prompt to continue rather than resending this one"
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
