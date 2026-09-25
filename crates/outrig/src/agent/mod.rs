//! The agent loop: resolve a model, build an agent whose only tool submits
//! Python to the session's interpreter, and drive rounds.
//!
//! A copy of `outrig-cli`'s loop rather than a move of it, so the 0.2.x line
//! keeps editing its own without conflict; `plan/phase/0003-python/` records
//! why. rig stays a private dependency: nothing rig-typed crosses
//! [`PythonAgent`]'s signatures, and [`AgentError`] renders rig's error to text
//! at the point it is caught.

mod build;
mod resolve;
mod round;
mod tool;

use std::error::Error;

use rig::completion::{Message, PromptError};
use rig::tool::ToolDyn;

use crate::Outrig;
use crate::config::Config;
use crate::error::OutrigError;
use crate::python::host::Interpreter;

use self::build::RigAgent;
use self::resolve::{LlmResolveError, ResolvedAgent};
use self::round::RoundEnd;
use self::tool::SubmitPython;

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
        let interpreter = Interpreter::start(outrig.primary()).await?;
        Ok(Self::build(&resolved, interpreter)?)
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

    /// [`PythonAgent::start`] over an interpreter the caller started.
    #[cfg(test)]
    pub(crate) fn with_interpreter(
        interpreter: Interpreter,
        config: &Config,
        agent: Option<&str>,
        model: Option<&str>,
    ) -> Result<Self, AgentError> {
        Self::build(&resolve::resolve_agent(config, agent, model)?, interpreter)
    }

    fn build(resolved: &ResolvedAgent, interpreter: Interpreter) -> Result<Self, AgentError> {
        let tool = SubmitPython::new(interpreter, resolved.tool_result_max_bytes);
        let built = build::build_agent(resolved, vec![Box::new(tool) as Box<dyn ToolDyn>])?;
        Ok(Self {
            agent: built.agent,
            history: Vec::new(),
            tool_call_max: resolved.tool_call_max,
            max_tokens: built.max_tokens,
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
