//! The session an owner holds: its rounds, its user channel, its interrupter,
//! and how it stops.

use std::sync::Arc;
use std::time::Duration;

use super::builder::SessionBuilder;
use super::event::{Payload, SessionState};
use super::lifecycle::Lifecycle;
use super::report::{
    self, EventDelivery, ExecutionOutcome, ExecutionStatus, ShutdownReport, Stopped,
};
use super::{Closed, ClosedBy, Failure, SessionError, UserChannel};
use crate::Outrig;
use crate::agent::{Agent, AgentError};
use crate::config::Config;

/// How long an execution an interrupt reached has to end of its own accord --
/// run its `finally` blocks, report an error -- before the container stops.
const TERMINATE_GRACE: Duration = Duration::from_secs(1);

/// How long, once the container has stopped, the interpreter's exit is waited
/// for, so the session records it before its report: the host's own grace for
/// an exiting interpreter, and a second.
const EXIT_WAIT: Duration = Duration::from_secs(6);

/// A running session: a container, the Python interpreter in it, and the
/// agent that acts by writing Python there, driven one round at a time.
///
/// Its owner drives rounds ([`Session::round`]), talks to the agent through
/// its `user` channel ([`Session::user_channel`]), watches it through the
/// subscriptions it was started with, and stops it with
/// [`Session::shutdown`], whose report says whether everything it started has
/// stopped. A round ending is not the work being done: Python a round started
/// can still be running, which its outcome names.
pub struct Session {
    id: String,
    pub(crate) agent: Agent,
    container: Option<Outrig>,
    lifecycle: Arc<Lifecycle>,
}

impl Session {
    pub(crate) fn new(
        id: String,
        agent: Agent,
        container: Option<Outrig>,
        lifecycle: Arc<Lifecycle>,
    ) -> Self {
        Self {
            id,
            agent,
            container,
            lifecycle,
        }
    }

    /// Start a session of `agent` over `config`, with every default: keys from
    /// the process environment, no subscriber, nothing recorded, and a
    /// container described by `config` alone.
    ///
    /// That container runs the agent's `image`, or `default-image`, as its
    /// `[images.<name>]` block describes, with no MCP server and no sidecar
    /// (see [`container_spec`](super::container_spec)). It mounts a workspace
    /// only if `[workspace] host-path` declares one; repository-relative paths
    /// resolve against the current directory. Its logs go under
    /// `session-root`, or a directory of the system's temporary directory,
    /// as `<root>/<session id>/logs`. Its image is not pulled under the
    /// block's name: one `image-name` names must be present already.
    pub async fn start(
        config: Config,
        agent: Option<&str>,
        model: Option<&str>,
    ) -> Result<Session, SessionError> {
        SessionBuilder::new(config, agent, model).start().await
    }

    /// The session's id, which names its container `outrig-<id>` and its
    /// events' `source`.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// The `[models.<name>]` row every model call is tried against first. An
    /// alias has already been resolved to its models, so this never names an
    /// alias: it is the first of them this build can reach. A call that fails
    /// there moves to the next, and the session's events name the model that
    /// answered each call.
    pub fn model(&self) -> &str {
        self.agent.model()
    }

    /// The Python version the interpreter reported when it started, such as
    /// `3.13.15`.
    pub fn python_version(&self) -> &str {
        self.agent.python_version()
    }

    /// The name of the container the interpreter runs in.
    pub fn container_name(&self) -> &str {
        self.agent.container_name()
    }

    /// The user's end of the agent's `user` channel: how what the user types
    /// reaches the agent, and how what the agent sends reaches them. Every
    /// call hands out the same channel.
    pub fn user_channel(&self) -> UserChannel {
        self.agent.user_channel()
    }

    /// A function that stops the Python the current round is waiting on --
    /// what Ctrl-C does -- callable from another task while
    /// [`Session::round`] runs.
    ///
    /// The first call cancels that execution, and interrupts it too if its
    /// event loop has stopped turning; the model then reads how it ended, and
    /// the round goes on. A second call on the same execution stops waiting
    /// for it: it keeps the interpreter until it finishes, and its result
    /// reaches the model later. Each returns what it did, as a sentence for
    /// the user.
    ///
    /// It returns `None` when no call is waiting on Python -- the model is
    /// being called, or no round is -- and does nothing. Dropping the round's
    /// future is how to stop one there. Python left holding the interpreter
    /// with nothing waiting on it, after a second call or a dropped round, is
    /// [`Session::stop_held`]'s, between rounds.
    pub fn interrupter(&self) -> impl Fn() -> Option<String> + Send + Sync + 'static {
        self.agent.interrupter()
    }

    /// Stop the execution left holding the interpreter with nothing waiting
    /// for it -- what Ctrl-C at the prompt does -- and say so, as a sentence
    /// for the user; `None`, doing nothing, when there is none.
    ///
    /// A second [`Session::interrupter`] call leaves one that way, as does
    /// dropping a round's future while a call waits on it; every submission is
    /// refused behind it until it ends. This cancels it and interrupts it,
    /// which ends one suspended on an await or blocked in a call of its own.
    /// For one that never started, because code an earlier execution left
    /// running holds the event loop, the interrupt lands in that code instead,
    /// as a first [`Session::interrupter`] call's would, and ending it is what
    /// lets the execution start. Code that catches both keeps the interpreter:
    /// call again, or end the session. How it ended reaches the model with the
    /// next call's result.
    pub fn stop_held(&self) -> Option<String> {
        self.agent.stop_held()
    }

    /// Close the session to new work, at once: from now on [`Session::round`]
    /// returns [`SessionError::Closed`], and every submission is refused. A
    /// round already running runs on until its model yields, each submission
    /// it makes refused with the reason as its result, so the model can say
    /// why its code did not run. What is already running is left to finish,
    /// or to [`Session::shutdown`] to stop.
    ///
    /// Not a request the agent can see or refuse, and it waits on nothing.
    pub fn close_admission(&self) {
        self.lifecycle.close(ClosedBy::Owner);
    }

    /// What closes the session's admission from another task while a round
    /// runs, as [`Session::interrupter`] interrupts one.
    pub fn closer(&self) -> Closer {
        Closer(Arc::clone(&self.lifecycle))
    }

    /// Drive one round, if anything was sent on the user channel since the
    /// model was last told what waits there: the model, told how many messages
    /// wait -- never what they say -- and whatever Python it submits, then its
    /// reply. [`RoundOutcome::NothingNew`], without calling the model, when
    /// nothing new arrived or the agent's code has already read what did.
    ///
    /// A message arriving while the round runs is announced in the next result
    /// the model reads, and ends a `runtime.wait` the round's code is in: that
    /// call returns, and the round goes on. One arriving after the last of them
    /// is left for the next call, which is why a caller asks again once a round
    /// ends.
    ///
    /// The reply is the model's own text: commentary, where a message the agent
    /// means the user to have is one it sends. A round cut short -- the
    /// tool-call cap, or a turn too large for the model's window -- keeps what
    /// it managed, and says why it stopped. A round whose final message held
    /// only reasoning carries the reasoning.
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
    /// The conversation belongs to the session rather than to the round, which
    /// commits each turn as it completes. An error leaves the conversation as
    /// it was if the round had run no Python. If it had, the completed tool
    /// calls and their results are kept, because what they did stands --
    /// nothing is rolled back. Either way the messages the round did not read
    /// are still waiting, and the next round announces them again.
    ///
    /// A round whose future is dropped before it returns keeps everything
    /// before it and its completed tool calls the same way. Python it was
    /// waiting on keeps running, and holds the interpreter until it ends;
    /// [`Session::stop_held`] stops it.
    pub async fn round(&mut self) -> Result<RoundOutcome, SessionError> {
        if let Some(by) = self.lifecycle.closed() {
            return Err(Closed::from(by).into());
        }
        match self.agent.round().await {
            Ok(None) => Ok(RoundOutcome::NothingNew),
            Ok(Some((finished, tasks))) => Ok(RoundOutcome::Ended(RoundEnd {
                reply: finished.reply,
                stopped: finished.stopped,
                reasoning: finished.reasoning,
                still_running: StillRunning {
                    tasks: match tasks {
                        Some(tasks) => Tasks::Listed {
                            names: tasks.names,
                            more: tasks.more,
                        },
                        None => Tasks::CouldNotTell,
                    },
                },
            })),
            Err(AgentError::Closed(closed)) => Err(closed.into()),
            Err(e) => Err(SessionError::Round(Failure(e))),
        }
    }

    /// Stop the session, and report what it found.
    ///
    /// Admission closes first, if the owner has not closed it. Executions
    /// already running are then left to finish until `deadline` has passed --
    /// [`DEFAULT_DRAIN`](super::DEFAULT_DRAIN) is a few seconds -- and one
    /// still running is interrupted, given a second to end of its own accord,
    /// and then the container is stopped, which ends the interpreter and
    /// whatever an interrupt could not reach. Nothing waits on a subscriber.
    ///
    /// Returns a bounded time after `deadline`: typically a few seconds, while
    /// the container stops and the event log is finished; at worst the
    /// container engine's own time limits, about a minute.
    pub async fn shutdown(self, deadline: Duration) -> ShutdownReport {
        let Self {
            id: _,
            agent,
            container,
            lifecycle,
        } = self;
        lifecycle.close(ClosedBy::Owner);
        let events = lifecycle.events();
        let interpreter = agent.interpreter.clone();
        let gate = interpreter.gate();
        if tokio::time::timeout(deadline, gate.drained())
            .await
            .is_err()
        {
            // Interrupted first, which agent code can answer: a `finally` runs,
            // and the execution ends `error` rather than `unknown`.
            for (id, end) in gate.live_at_close() {
                if end.is_none() {
                    interpreter.cancel(id);
                    interpreter.interrupt(id, false);
                }
            }
            let _ = tokio::time::timeout(TERMINATE_GRACE, gate.drained()).await;
        }
        let gone = interpreter.gone();
        interpreter.expect_exit();
        let stopped = match container {
            Some(outrig) => {
                let failed = outrig.stop().await;
                // Recorded before the report, so the log says so too.
                let _ = tokio::time::timeout(EXIT_WAIT, gone).await;
                match failed.is_empty() {
                    true => Stopped::Proven,
                    false => Stopped::NotProven {
                        reason: failed.join("; "),
                    },
                }
            }
            // A host session: its interpreter is stopped once it is seen to
            // exit after its input closes.
            None => {
                interpreter.hang_up();
                match tokio::time::timeout(EXIT_WAIT, gone).await {
                    Ok(()) => Stopped::Proven,
                    Err(_) => Stopped::NotProven {
                        reason: format!("the interpreter did not exit within {EXIT_WAIT:?}"),
                    },
                }
            }
        };
        let executions: Vec<ExecutionOutcome> = gate
            .live_at_close()
            .into_iter()
            .map(|(id, end)| ExecutionOutcome {
                id: id.public(),
                status: end.unwrap_or(ExecutionStatus::Unknown),
            })
            .collect();
        let closed_by = gate.closed().unwrap_or(ClosedBy::Owner);
        drop(agent);
        events.emit(Payload::AgentStopped {});
        events.emit(Payload::SessionReport {
            closed_by: closed_by.clone(),
            stopped: stopped.clone(),
            executions: executions.clone(),
            verdict: report::verdict(&stopped, &executions),
        });
        let log = events
            .close_after(Payload::SessionState {
                state: SessionState::Reported,
            })
            .await
            .err();
        let tally = events.tally();
        ShutdownReport {
            closed_by,
            stopped,
            executions,
            events: EventDelivery {
                last_sequence: tally.last,
                missed: tally.missed,
                log,
            },
        }
    }

    /// The session's interpreter, for a test.
    #[cfg(test)]
    pub(crate) fn interpreter(&self) -> &crate::python::host::Interpreter {
        &self.agent.interpreter
    }

    /// The session's container, for a test.
    #[cfg(all(test, feature = "e2e"))]
    pub(crate) fn outrig(&self) -> Option<&Outrig> {
        self.container.as_ref()
    }
}

/// What closes a session's admission from wherever its owner holds it, while
/// a round runs. Cheap to clone.
#[derive(Clone)]
pub struct Closer(Arc<Lifecycle>);

impl Closer {
    /// [`Session::close_admission`].
    pub fn close_admission(&self) {
        self.0.close(ClosedBy::Owner);
    }
}

/// How a round went.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum RoundOutcome {
    /// Nothing new had arrived on the user channel, so the model was not
    /// called.
    NothingNew,
    /// The round ran, and ended.
    Ended(RoundEnd),
}

/// How a round that ran ended.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct RoundEnd {
    /// The model's closing text: commentary, where a message the agent means
    /// the user to have is one it sends.
    pub reply: String,
    /// Why a limit stopped the round, when one did: the tool-call cap, or a
    /// turn too large for the model's window. `None` when the model yielded.
    pub stopped: Option<String>,
    /// The reasoning in the round's final message, when it held no text: a
    /// turn the model spent thinking, likely cut off at its output ceiling.
    pub reasoning: Option<String>,
    /// What the round's code left running.
    pub still_running: StillRunning,
}

/// What a round left running when it ended. A round ending is the model
/// yielding, not the work being done.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct StillRunning {
    /// The tasks still running on the agent's event loop.
    pub tasks: Tasks,
}

impl StillRunning {
    /// Whether nothing is known to be running.
    pub fn is_empty(&self) -> bool {
        matches!(&self.tasks, Tasks::Listed { names, more: 0 } if names.is_empty())
    }
}

/// The tasks running on an agent's event loop -- those its code started with
/// `asyncio.create_task`, say -- by name. An unnamed task is `Task-N`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Tasks {
    /// The first of them, sorted, and how many more there are.
    #[non_exhaustive]
    Listed { names: Vec<String>, more: usize },
    /// The interpreter did not say: its event loop was not turning.
    CouldNotTell,
}
