//! A Python program on the host as an owner: `outrig run-new <script.py>`.
//!
//! The program is the owner's code, run by the engine and supervised by it. It reaches the
//! engine over a private connection, the owner protocol below, and owns sessions through it: it
//! asks for one, watches it, closes its admission and shuts it down, and every resource of the
//! session is the engine's, which applies each control before it acknowledges it. The engine
//! also outlives the program. Whatever the program leaves behind -- a session open when it exits,
//! raises or is killed, a start it never saw finish -- the engine stops, waits for, and accounts
//! for in [`ProgramExit`]; and a program that does not end after a forwarded signal is ended
//! (`program.rs`).
//!
//! # The owner protocol, version 2
//!
//! NDJSON over the two pipes `python::program` sets up, one object per line, both ways.
//!
//! - **The greeting.** The program's first line is `{"t": "hello", "id": 0, "protocol": 2}`. The
//!   engine answers the same at the same version, before the program's script runs; a program
//!   speaking another version is answered with an `error` and ended, and its start fails.
//! - **Requests**, answered by `id` with a line of the same `t`, or an `error`: `config`;
//!   `session.start` (`session`, `workspace`, `mounts`), answered with the session's `id` and
//!   its `requests_max` once it has started; `admission.close` (`session`), answered once the
//!   gate is closed; and `session.shutdown` (`session`, `grace`), answered with the report once
//!   the session stopped. `session` is a number the program chooses for each session, so what
//!   the engine pushes about one can be routed before its start is answered.
//! - **Its agent class instances**, root children of the session (`agent::children`): `agent.*`
//!   lines, as a kernel sends them -- `agent.spawn`, answered with the `instance` id the child's
//!   kernel gave it; `agent.release`, with `how`, which the engine records as it releases, and
//!   answers at once, releasing nothing, once admission has closed; and
//!   `agent.instance.handle.collected` -- and three notices about one of its requests, which the
//!   program numbers `ask`: `ask` (`agent`, `channel`, `body`), the request; `withdraw`
//!   (`agent`), its cancel; and `verdict` (`agent`, `reply`, `problems`), what the program found
//!   decoding reply `reply` to it, empty to accept it. Each names its `session`.
//! - **Pushes**, which carry no `id`: `event` (`session`, `record`), each event of the session as
//!   `events.jsonl` would hold it; `missed` (`session`, `count`), events the engine's subscription
//!   lost; and `events.end` (`session`), after which nothing more of the session is pushed. Of
//!   its instances: `oriented` and `child`, naming the program's number for the instance; each
//!   step of a request, as the interpreter says them, naming its `ask` -- `agent.request`,
//!   `agent.received`, `agent.reply` (each reply numbered), and `agent.ended`, with why it ended
//!   (`error`), the reply accepted (`reply`), or a rejected one's `last` reply and its problems;
//!   and `agent.closed` (`terminated`), the session's close, and again, `terminated`, once its
//!   interpreter has exited. Every push about a session precedes the answer that ends it.

mod connection;
mod program;
mod session;

#[cfg(test)]
pub(crate) mod tests;

use std::ffi::OsString;
use std::os::unix::process::ExitStatusExt as _;
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::time::Duration;

use serde_json::{Map, Value, json};
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;

use self::program::Forward;
use crate::config::{Config, LlmProvider};
use crate::error::OutrigError;
use crate::harness::{ShutdownReport, Stopped, Verdict, chosen_repo_root};
use crate::python::program::{Launched, Spec, cannot_run, launch};

/// A trusted Python program to run on the host, under the Python OutRig carries, as
/// `outrig run-new <script.py> [args...]` runs one.
///
/// The program is the owner's code, not a sandbox: it runs with this process's authority, its
/// environment, working directory and stdio, which it inherits, and its arguments, which are its
/// own. It runs as `__main__`, imports modules beside its script, and imports `outrig`, where
/// `outrig.config()` is a read-only view of the [`Config`] given here, holding no secret, and
/// `outrig.Session` enters a session with no primary agent, started by this process from that
/// configuration. Starting the program starts no container and calls no model.
///
/// It leads a process group of its own, and ends with this process: killed, this process takes
/// the program's group with it. It does not read the terminal, which its group does not hold;
/// a read from it fails.
#[derive(Debug)]
pub struct HostProgram {
    config: Config,
    script: PathBuf,
    args: Vec<OsString>,
    repo_root: Option<PathBuf>,
    grace: Option<Duration>,
    #[cfg(test)]
    testing: tests::Testing,
}

impl HostProgram {
    /// The program at `script`, given `config`, loaded and validated by the caller: what
    /// `outrig.config()` shows it, and what each session it enters is started from.
    ///
    /// A session runs in the image `default-image` names, ensured as `outrig run-new` ensures
    /// one, and records its events in `<session-root>/<id>/logs/events.jsonl` when `[events] mode
    /// = "record"`; with no `session-root`, under `outrig-sessions` in the system's temporary
    /// directory.
    pub fn new(config: Config, script: impl Into<PathBuf>) -> Self {
        Self {
            config,
            script: script.into(),
            args: Vec::new(),
            repo_root: None,
            grace: None,
            #[cfg(test)]
            testing: tests::Testing::default(),
        }
    }

    /// The program's arguments, after its script in its `sys.argv`.
    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    /// The repository the configuration belongs to, which a relative `[workspace]` host path
    /// resolves against; the working directory when not given.
    pub fn repo_root(mut self, root: impl Into<PathBuf>) -> Self {
        self.repo_root = Some(root.into());
        self
    }

    /// How long the program has to end on its own after the first signal forwarded to it
    /// ([`RunningProgram::interrupt`], [`RunningProgram::terminate`]), in place of `[host]
    /// grace-secs`. Time this process spends shutting down a session the program asked it to
    /// shut down does not count against it.
    pub fn grace(mut self, grace: Duration) -> Self {
        self.grace = Some(grace);
        self
    }

    /// Start the program, once it has connected: what it needs of the engine is there before its
    /// script runs. A script that cannot be opened as a file fails this before anything is
    /// unpacked or started, as does a configured workspace whose path is not UTF-8, which
    /// `outrig.config()` could not show; a program that cannot connect -- its interpreter fails
    /// first, or it speaks another version of the connection's protocol -- is ended, and fails
    /// it too.
    pub async fn start(self) -> Result<RunningProgram, OutrigError> {
        let repo_root = chosen_repo_root(self.repo_root.as_deref())?;
        let view = view(&self.config, &repo_root)?;
        let grace = self
            .grace
            .unwrap_or_else(|| Duration::from_secs(self.config.host.grace_secs()));
        #[cfg(test)]
        let (launching, sessions) = self.testing.split();
        let Launched {
            mut child,
            mut reads,
            mut writes,
            held,
        } = launch(Spec {
            script: &self.script,
            args: &self.args,
            #[cfg(not(test))]
            cwd: None,
            #[cfg(test)]
            cwd: launching.cwd.as_deref(),
            #[cfg(test)]
            adjust: launching.adjust,
            #[cfg(test)]
            package: launching.package,
        })
        .await?;
        if let Err(why) = connection::greet(&mut reads, &mut writes).await {
            child.kill().await;
            return Err(cannot_run(&self.script, std::io::Error::other(why)));
        }

        let gone = CancellationToken::new();
        let shutdowns = Arc::new(AtomicUsize::new(0));
        let (exited, exit) = watch::channel(None);
        let (ended, left) = watch::channel(None);
        let (forwards, forwarded) = mpsc::unbounded_channel();
        #[cfg(test)]
        let live = sessions.live.clone();
        let context = Arc::new(session::Context {
            config: self.config,
            repo_root,
            shutdowns: Arc::clone(&shutdowns),
            #[cfg(test)]
            testing: sessions,
        });
        tokio::spawn(program::supervise(program::Supervision {
            child,
            held,
            forwarded,
            grace,
            asked: shutdowns,
            exited,
            gone: gone.clone(),
        }));
        let out = connection::Out::new(writes, gone);
        tokio::spawn(connection::serve(reads, out, context, view, ended));
        Ok(RunningProgram {
            forwards,
            exit,
            left,
            #[cfg(test)]
            live,
        })
    }

    /// How a test starts the program and its sessions.
    #[cfg(test)]
    pub(crate) fn testing(mut self, testing: tests::Testing) -> Self {
        self.testing = testing;
        self
    }
}

/// A host program while it runs. Dropped before [`RunningProgram::wait`] returns, it kills the
/// program's process group.
#[derive(Debug)]
pub struct RunningProgram {
    forwards: mpsc::UnboundedSender<Forward>,
    exit: watch::Receiver<Option<Result<ExitStatus, String>>>,
    left: watch::Receiver<Option<Vec<LeftOpen>>>,
    /// Each started session's interpreter, by the program's number for it, for a test.
    #[cfg(test)]
    pub(crate) live: tests::Live,
}

impl RunningProgram {
    /// Forward `SIGINT` to the program's process group, as a terminal's Ctrl-C would reach it had
    /// it been in the foreground. The first signal forwarded starts the program's grace; past
    /// it, or at a second signal, its group is stopped -- `SIGTERM`, five seconds, `SIGKILL` --
    /// and a further signal sends `SIGKILL` at once. An owner forwards its own `SIGINT` this
    /// way; nothing here installs a handler.
    pub fn interrupt(&self) {
        let _ = self.forwards.send(Forward::Interrupt);
    }

    /// Forward `SIGTERM` to the program's process group, as [`RunningProgram::interrupt`]
    /// forwards `SIGINT`.
    pub fn terminate(&self) {
        let _ = self.forwards.send(Forward::Terminate);
    }

    /// Wait for the program to exit, and for every session it left open to be stopped.
    /// Cancel-safe. What the program started and left running in its group after a normal exit
    /// is not waited for, and keeps running.
    pub async fn wait(&mut self) -> Result<ProgramExit, OutrigError> {
        let gone = || OutrigError::Io(std::io::Error::other("the program's supervision ended"));
        let status = self
            .exit
            .wait_for(Option::is_some)
            .await
            .map_err(|_| gone())?
            .clone()
            .expect("waited for")
            .map_err(|why| OutrigError::Io(std::io::Error::other(why)))?;
        let left_open = self
            .left
            .wait_for(Option::is_some)
            .await
            .map_err(|_| gone())?
            .clone()
            .expect("waited for");
        Ok(ProgramExit { status, left_open })
    }
}

/// How a host program ended, and what the engine did about the sessions it left behind.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct ProgramExit {
    status: ExitStatus,
    left_open: Vec<LeftOpen>,
}

impl ProgramExit {
    /// The program's own exit status.
    pub fn status(&self) -> ExitStatus {
        self.status
    }

    /// Each session whose end the program did not see: open, or still starting, when the
    /// program ended, or stopped after it was gone. Also each start that failed, or that the
    /// program gave up on, whose cleanup is not proven: the program was told, and this outranks
    /// its status all the same. The engine has stopped each one, or says what it could not prove.
    pub fn left_open(&self) -> &[LeftOpen] {
        &self.left_open
    }

    /// The status for an owner to exit with: the program's, or 128 plus the signal's number when
    /// a signal ended it, as a shell reports one -- unless a session it left open is not proven
    /// stopped, which outranks it as 3: something it started may still be running.
    pub fn code(&self) -> i32 {
        if self.left_open.iter().any(LeftOpen::not_proven) {
            return 3;
        }
        match (self.status.code(), self.status.signal()) {
            (Some(code), _) => code,
            (None, Some(signal)) => 128 + signal,
            (None, None) => 1,
        }
    }
}

/// A session a host program did not see to its end, which the engine ended for it.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct LeftOpen {
    /// The session's id.
    pub id: String,
    pub end: SessionEnd,
}

impl LeftOpen {
    /// Whether something the session started may still be running.
    fn not_proven(&self) -> bool {
        match &self.end {
            SessionEnd::ShutDown(report) => report.verdict() == Verdict::NotProvenStopped,
            SessionEnd::NotStarted(stopped) => !matches!(stopped, Stopped::Proven),
        }
    }
}

/// How the engine ended a session a host program left open.
#[derive(Debug, Clone)]
#[non_exhaustive]
#[expect(
    clippy::large_enum_variant,
    reason = "held in a list read once, at the program's exit; a boxed report would only make \
              every reader dereference it"
)]
pub enum SessionEnd {
    /// It had started: the engine closed its admission and shut it down, and this is its report.
    ShutDown(ShutdownReport),
    /// It never finished starting -- its start was interrupted, because the program was gone or
    /// gave up on it, or failed -- and this is whether everything the start had made is proven
    /// stopped. Nothing of it ran, so nothing else is reported.
    NotStarted(Stopped),
}

/// What `outrig.config()` answers with: the models, aliases, providers and defaults of `config`,
/// and nothing that is or names a secret -- no key, no `${VAR}`, and nothing of the images,
/// sidecars or bindings, whose values may be literal ones.
///
/// A declared workspace whose path is not UTF-8 is a configuration error: the view is JSON, which
/// holds text, and a path with a byte replaced would name another directory.
pub(crate) fn view(config: &Config, repo_root: &Path) -> Result<Value, OutrigError> {
    let mut models = Map::new();
    let mut aliases = Map::new();
    for (name, model) in &config.models {
        match &model.alias {
            Some(targets) => {
                // Flattened as a call walks them; a configuration nobody validated may have no
                // such order, and shows what it declares.
                let candidates = config.model_candidates(name).map_or_else(
                    |_| targets.clone(),
                    |candidates| candidates.into_iter().map(str::to_owned).collect(),
                );
                aliases.insert(name.clone(), json!({ "candidates": candidates }));
            }
            None => {
                // The style a call uses, so a host program does not repeat the rule. A
                // configuration nobody validated may name no such provider, and shows the row's
                // own.
                let provider = model
                    .provider
                    .as_ref()
                    .and_then(|p| config.providers.get(p));
                let style = provider.map_or(model.style, |p| Some(model.effective_style(p)));
                models.insert(
                    name.clone(),
                    json!({
                        "provider": model.provider,
                        "style": style,
                        "identifier": model.identifier.as_deref().unwrap_or(name),
                        "context_window": model.context_window,
                        "max_tokens": model.max_tokens,
                    }),
                );
            }
        }
    }
    let providers: Map<String, Value> = config
        .providers
        .iter()
        .map(|(name, provider)| {
            let (LlmProvider::ChatCompletions { base_url, .. }
            | LlmProvider::Responses { base_url, .. }
            | LlmProvider::Anthropic { base_url, .. }) = provider;
            let row = json!({ "style": provider.style(), "base_url": base_url });
            (name.clone(), row)
        })
        .collect();
    // As a session mounts it: resolved against its base, `..` kept for the filesystem to follow,
    // through a symlink as the mount does.
    let workspace = config
        .workspace
        .declared_host_path()
        .map(|_| {
            let path = config.workspace.resolved_host_path(repo_root);
            path.into_os_string().into_string().map_err(|path| {
                OutrigError::Configuration(format!(
                    "[workspace] host-path resolves to {:?}, which is not UTF-8 and so cannot be \
                     shown as outrig.config().workspace",
                    Path::new(&path)
                ))
            })
        })
        .transpose()?;
    Ok(json!({
        "models": models,
        "aliases": aliases,
        "providers": providers,
        "default_model": config.default_model,
        "workspace": workspace,
        "events": { "mode": config.events.mode() },
    }))
}
