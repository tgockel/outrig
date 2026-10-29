//! `outrig run-new`: an interactive session whose agent acts by writing Python.
//!
//! The loop lives in `outrig`, and this module and `converse` are the terminal
//! around it: the session is started, driven, watched and stopped through
//! [`outrig::harness`] and nothing else of the loop, as any other owner of one
//! would. It shares none of `run`'s session setup, and `run`'s code is left
//! exactly as it was. Nor does it share `run`'s REPL, which reads a line only
//! between rounds: here what the user types reaches the agent's `user`
//! channel while a round runs, too.
//!
//! **No MCP server and no sidecar starts**: see
//! [`harness::container_spec`], which describes the container.
//!
//! The session is recorded like any other, in the order that keeps
//! `session.json` honest: the record is written once the interpreter is up,
//! carrying the container name the session reports, so a live record always
//! has a container behind it. The container is `outrig-<sid>`, labeled
//! `org.outrig.session=<sid>`, as `run`'s is: the session id reaches the
//! launch through its `LaunchSpec`, and `discard` and `clean` read the name
//! back to tell a live session from a finished one. Until the record is
//! written, the session directory is held by a lock instead, so nothing else
//! starts a session in it meanwhile.
//!
//! It leaves through the session's report: `/quit`, end of input, or a second
//! Ctrl-C at the prompt close the session to new work, stop it, print what the
//! report found, and exit with the status the report decides. A conversation
//! that fails, or a record that cannot be written, closes the session the same
//! way, and its error is printed after the report.
//!
//! On a terminal the conversation is held on a [`screen`]: a status line and an
//! input box below a conversation that keeps only what the user and the agent
//! sent each other. Anywhere else it is plain lines, as `converse` describes.

use std::ffi::OsString;
use std::fs::File;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use clap::Parser;
use directories::BaseDirs;
use nix::fcntl::{Flock, FlockArg};
use outrig::config::{Config, EnvSecrets, EventsMode, HOST_GRACE_SECS_CEILING, ImageConfig};
use outrig::error::IoPathExt;
use outrig::harness::{
    self, DEFAULT_DRAIN, DirectorySource, Progress, SessionBuilder, Step, Verdict,
};
use outrig::image::{self, BuildOutput, ImageTag};
use outrig::LaunchSpec;

use crate::builtin_image;
use crate::cli::report::{Summary, render_report, summarize};
use crate::cli::log_divert::LogWriter;
use crate::cli::session_setup::{ProgressSpan, Say};
use crate::cli::signals::{EndSignal, Signals, session_ending_line};
use crate::error::{CliError, OutrigError, Result};
use crate::paths::{
    RepoConfig, default_session_root, refuse_home_workspace, refuse_repo_declared_mounts,
    refuse_workspace_covering_global_config,
};
use crate::session::{self, Session, SessionId, SessionStore, resolve_session_root};

use self::converse::{Conversed, Frontend, Interrupts};
use self::output::{Failures, Output};
use self::screen::{Screen, Started};

mod approve;
mod converse;
mod output;
mod screen;

/// How long what the start said is waited for before the screen takes the
/// terminal. The screen draws on `/dev/tty` apart from `Output`'s writers, so
/// a banner still being written would land on its first frame; past this, the
/// session is in plain lines instead.
const HANDOFF_GRACE: Duration = Duration::from_secs(2);

/// How long, once the session is stopped and its record finished, the report
/// is waited for. A destination that has stopped -- a pager paused, a pipe
/// nobody reads -- delays what is printed and not the exit, so the wait ends
/// here with whatever was not written left unsaid.
const REPORT_GRACE: Duration = Duration::from_secs(5);

/// The ids of [`RunNewArgs`]' session flags, which a program, choosing its own session's inputs,
/// refuses. Listed rather than named as a group, so that clap's refusal names the flag given
/// rather than every flag in the group.
const SESSION_FLAGS: [&str; 4] = ["agent", "model", "image", "session_dir"];

#[derive(Debug, Parser)]
pub struct RunNewArgs {
    /// Pick an `[agents.<name>]` block. Defaults to `default-agent` from
    /// config; with neither, the session runs with no agent preamble.
    #[arg(long, value_name = "NAME")]
    pub agent: Option<String>,

    /// Pick a `[models.<name>]` block for this run. Overrides the agent's
    /// `model` and the top-level `default-model`.
    #[arg(long, value_name = "NAME")]
    pub model: Option<String>,

    /// Pick an `[images.<name>]` block. Overrides the agent's `image` and the
    /// top-level `default-image`. Unlike `outrig run`, a local image ref that
    /// no block names is not accepted.
    #[arg(long, value_name = "NAME")]
    pub image: Option<String>,

    /// Write the session into this directory, which must be empty or not exist
    /// yet (it is created, with any missing parents). The session root gets a
    /// symlink at `<root>/<sid>` pointing at this path.
    #[arg(long = "session-dir", value_name = "PATH")]
    pub session_dir: Option<PathBuf>,

    /// With a script: how many seconds the program has to end on its own after the first SIGINT
    /// or SIGTERM forwarded to it, before its process group is stopped -- SIGTERM, five seconds,
    /// SIGKILL. Time spent shutting down a session the program asked to shut down does not count.
    /// Overrides `[host] grace-secs`; 5 when neither is given.
    #[arg(
        long,
        value_name = "SECS",
        requires = "program",
        value_parser = clap::value_parser!(u64).range(..=HOST_GRACE_SECS_CEILING)
    )]
    pub grace: Option<u64>,

    /// A Python program to run on the host, then its own arguments (preview). Everything after
    /// it is the program's, `--help` and OutRig's own flags included.
    #[arg(
        value_name = "SCRIPT",
        trailing_var_arg = true,
        conflicts_with_all = SESSION_FLAGS
    )]
    pub program: Vec<OsString>,
}

/// Run one `outrig run-new` invocation end to end. Returns the process exit
/// code. With `verbose`, a screen shows the agent's working in the
/// conversation too.
pub async fn execute(
    repo: &RepoConfig,
    global_cfg_path: Option<&Path>,
    session_root_flag: Option<&Path>,
    args: &RunNewArgs,
    verbose: u8,
) -> Result<i32> {
    let repo_root = repo.root.clone();
    let span = ProgressSpan::start("loading config");
    let mut cfg = repo.load_for_run(global_cfg_path, args.agent.as_deref(), args.model.as_deref())?;
    span.done("config loaded");
    if !repo.config_path().exists() {
        eprintln!(
            "[outrig] no repo config found; using current directory as workspace ({})",
            repo_root.display()
        );
    }
    // A workspace no config declared is outrig's pick, and outrig never picks
    // the home directory or one above it, nor one that contains the global
    // config; and a repo config may not mount either -- the rules `run`
    // applies.
    let workspace: PathBuf = cfg
        .workspace
        .resolved_host_path(&repo_root)
        .components()
        .collect();
    if cfg.workspace.declared_host_path().is_none() {
        refuse_home_workspace(&workspace, &repo_root)?;
        refuse_workspace_covering_global_config(&workspace, &repo_root, global_cfg_path)?;
    }
    refuse_repo_declared_mounts(&cfg, &repo_root, global_cfg_path)?;

    // What the session needs from outside, given before it starts: the
    // terminal sees each submission through a subscription of its own, and
    // the event log is a second one when the user asked for it.
    let agent_name = args.agent.clone().or_else(|| cfg.default_agent.clone());
    // Made before the container starts, so a process that cannot start its
    // threads fails with nothing left to stop. The conversation writes through
    // it, and so do the start's own progress, the banner, the closing line and
    // the report, in turn with what the agent sends: the task that starts the
    // session is the one that hears a signal, and a write of its own to a
    // stderr whose reader has stopped would hold it up for good.
    let (out, mut failures) = Output::stdio()?;
    let say = out.say();
    let mut builder = SessionBuilder::new(cfg.clone(), agent_name.as_deref(), args.model.as_deref())
        .secrets(EnvSecrets)
        .repo_root(&repo_root)
        .approver(approve::TerminalApprover)
        .progress(progress_lines(say.clone()))
        .skill_source("project", DirectorySource::within(&repo_root, SKILLS_DIR));
    let home = BaseDirs::new().map(|dirs| dirs.home_dir().to_path_buf());
    if let Some(home) = user_skills_home(&repo_root, home) {
        builder = builder.skill_source("user", DirectorySource::new(home.join(SKILLS_DIR)));
    }
    if matches!(cfg.events.mode(), EventsMode::Record) {
        builder = builder.record_events();
    }
    let mut shown = builder.subscribe();
    // Before anything is pulled or started, so a config that names no usable
    // model costs nothing.
    builder.check()?;

    let (image_cfg_name, builtin_default) =
        image_config_name(&mut cfg, args.image.as_deref(), agent_name.as_deref())?;
    let image_cfg = cfg.images.get(&image_cfg_name).ok_or_else(|| {
        OutrigError::Configuration(format!(
            "image-config {image_cfg_name:?} does not match any [images.<name>]; `outrig run-new` \
             takes a configured image-config, not a local image ref"
        ))
    })?;

    let store = SessionStore::new(resolve_session_root(
        session_root_flag,
        &cfg,
        &default_session_root(),
    ));
    // Installed before anything a signal could orphan exists, as `run`'s
    // setup installs them: from here SIGINT, SIGTERM and SIGHUP are never at
    // their default disposition, so none of them can kill outrig with a
    // container started and a record unfinished. SIGINT is the conversation's
    // own once it begins -- a Ctrl-C there stops the round, not the session --
    // and a listener that nobody polls keeps the signal for the next wait that
    // does, never the default action.
    let mut signals = Signals::install()?;
    // Asked now, while a terminal cannot yet have hung up; see [`Terminals`].
    let terminals = Terminals::now();
    let sid = SessionId::new();
    // Held until this returns: the directory is this invocation's from here.
    let (_reserved, log_dir) = reserve_session_dir(&store, &sid, args.session_dir.as_deref())?;

    let mut session = Session {
        id: sid.clone(),
        started_at: SystemTime::now(),
        ended_at: None,
        container_name: String::new(),
        sidecar_container_names: Vec::new(),
        image_tag: String::new(),
        image_config_name: Some(image_cfg_name.clone()),
        agent_name: agent_name.clone(),
        working_dir: repo_root.clone(),
        session_dir: PathBuf::new(), // set by `create` below
        exit_code: None,
        link_target: None,
    };
    // Log records go the same way while the start runs, until the screen
    // diverts them to itself.
    // Held to the end of the session, and what the screen diverts after goes
    // back to it, never to a write to stderr that waits on its reader.
    let _logs = LogWriter::global().divert({
        let say = say.clone();
        move |record| say(&record.text)
    });
    // Raced, as `run`'s setup races its own: a signal here drops the start,
    // and what it had made -- a build's working containers, a container
    // already up -- is removed on the way out rather than left behind. What
    // the signal says is queued on `out` and not written here: a build's own
    // output can be stuck writing to a stderr nobody reads, holding the stream
    // a write from here would wait for, and this is where the start is
    // dropped from.
    let started = signals
        .race_settled_saying(
            start(Start {
                builder,
                cfg: &cfg,
                image_cfg_name: &image_cfg_name,
                image_cfg,
                repo_root: &repo_root,
                log_dir,
                record: &mut session,
                say: &say,
            }),
            |line| out.text_best_effort(format!("{line}\n")),
        )
        .await;

    // Written only now, once the container runs under the name recorded. A
    // session that failed to start is recorded too, and at once finalized, so
    // it is never a live record without a container.
    if let Err(e) = store.create_prepared(&sid, args.session_dir.as_deref(), &mut session) {
        let Ok(running) = started else {
            out.finish(tokio::time::sleep(REPORT_GRACE)).await;
            return Err(e);
        };
        let summary = summarize(&running.shutdown(DEFAULT_DRAIN).await);
        if summary.verdict != Verdict::Clean {
            say(&render_report(&summary));
        }
        // After the record is as it will be, as for any other end.
        out.finish(tokio::time::sleep(REPORT_GRACE)).await;
        // No conversation ran, so no signal ended one.
        return outcome_of(Err(e), &summary, None);
    }
    let mut running = match started {
        Ok(running) => running,
        Err(e) => {
            // The signal's status for a start a signal ended, so the record
            // says what the process exits with, as a finished session's does.
            let _ = store.finalize(&sid, SystemTime::now(), e.exit_code());
            // After the record, so a destination that has stopped delays
            // what the signal said and not the exit.
            out.finish(tokio::time::sleep(REPORT_GRACE)).await;
            return Err(e);
        }
    };

    // What is said from here on is best effort: a stderr whose reader has gone
    // must not skip the stop and the record that follow.
    say(&render_banner(&Banner {
        image_config_row: builtin_image::banner_image_config_row(&image_cfg_name, builtin_default),
        image_tag: &session.image_tag,
        model: running.model().unwrap_or_default(),
        python_version: running.python_version(),
        container_name: running.container_name(),
        session_id: sid.as_str(),
    }));

    let conversed = conversation(
        &mut running,
        &mut shown,
        &out,
        &mut failures,
        &mut signals,
        verbose > 0,
    )
    .await;
    // A hangup that surfaced as a failed read or write, or as input ending,
    // is the signal's, wherever in the conversation it did.
    let conversed = conversed.reconciled(&mut failures);
    // The signal that ended the conversation, if one did: the session stops
    // as it does for `/quit`, and the status it exits with is the signal's,
    // whatever else went wrong on the way.
    let signalled = hangup_behind(conversed.signal, terminals);

    // Said after the screen has given the terminal back, which it drew over
    // and cleared, so a signal's notice is not lost with the screen; said
    // first here, since stopping can take a few seconds and a Ctrl-C there
    // does nothing. Queued, not written here: with stdout and stderr one pipe,
    // a write waits for the message being written to it, and for as long as
    // nobody reads the pipe, which the stop must not wait for.
    if let Some(sig) = signalled {
        out.text_best_effort(session_ending_line(sig));
    }
    out.text_best_effort("[outrig] closing the session\n");
    let report = running.shutdown(DEFAULT_DRAIN).await;
    let summary = summarize(&report);
    out.text_best_effort(render_report(&summary));
    // `/quit` did not wait for what the conversation had queued, so a write of
    // it that has failed since, as the stop ran, fails the session here.
    let conversed = conversed.reconciled(&mut failures);
    let signalled = hangup_behind(signalled, terminals);
    let outcome = outcome_of(conversed.result, &summary, signalled);
    // The status the process exits with, so the record never says otherwise.
    let exit = outcome
        .as_ref()
        .copied()
        .unwrap_or_else(CliError::exit_code);
    if let Err(e) = store.finalize(&sid, SystemTime::now(), exit) {
        tracing::warn!(target: "outrig::cli::run_new", "finalizing session {sid}: {e}");
    }
    // Last, so the record is finished whether or not the report could be
    // written, and the `error:` line printed once this returns follows it.
    out.finish(tokio::time::sleep(REPORT_GRACE)).await;
    outcome
}

/// A standard stream, as [`Terminals`] asks about it.
trait Stream: IsTerminal + std::os::fd::AsRawFd {}
impl<T: IsTerminal + std::os::fd::AsRawFd> Stream for T {}

/// Which of stdin, stdout and stderr were this process's controlling terminal
/// when the session began.
///
/// A terminal that closes is hung up before anything blocked on it is woken:
/// every descriptor open on it fails its reads, writes and ioctls from then on,
/// and `isatty` says it is no terminal. So a descriptor that was the
/// controlling terminal and is not has certainly hung up, whether or not its
/// SIGHUP has been heard yet -- a read or write can fail with `EIO`, or a read
/// end as input ending, before it -- and the session ends as that signal ends
/// it. Nothing is waited for.
///
/// Only the controlling terminal: its hangup is what sends this process
/// SIGHUP. Another terminal a stream was redirected to hangs up on its own
/// session, and a pipe that closes, or input ending on a terminal that has not,
/// is not a hangup at all.
#[derive(Clone, Copy)]
struct Terminals([bool; 3]);

impl Terminals {
    fn now() -> Self {
        Self::of(
            [&std::io::stdin() as &dyn Stream, &std::io::stdout(), &std::io::stderr()],
            |stream| crate::repl::is_controlling_tty(&stream.as_raw_fd()),
        )
    }

    fn of(streams: [&dyn Stream; 3], controlling: impl Fn(&dyn Stream) -> bool) -> Self {
        Self(streams.map(|stream| stream.is_terminal() && controlling(stream)))
    }

    /// Whether a stream that was the controlling terminal has hung up. The
    /// streams are the same three, asked again.
    fn hung_up(&self, streams: [&dyn Stream; 3]) -> bool {
        self.0
            .iter()
            .zip(streams)
            .any(|(was, now)| *was && !now.is_terminal())
    }
}

/// The signal that ended the conversation: the one it returned or, failing
/// that, the SIGHUP a controlling terminal that has hung up sends.
fn hangup_behind(signal: Option<EndSignal>, started: Terminals) -> Option<EndSignal> {
    let streams: [&dyn Stream; 3] = [&std::io::stdin(), &std::io::stdout(), &std::io::stderr()];
    signal.or_else(|| started.hung_up(streams).then_some(EndSignal::Hangup))
}

/// Where skills live, beneath a repository and beneath a home directory.
const SKILLS_DIR: &str = ".agents/skills";

/// The home directory whose `.agents/skills` holds the user's skills, served
/// after the repository's: none when there is no home, or when the home is the
/// repository root, whose `.agents/skills` the project's source already
/// serves.
fn user_skills_home(repo_root: &Path, home: Option<PathBuf>) -> Option<PathBuf> {
    let home = home?;
    let same = match (std::fs::canonicalize(&home), std::fs::canonicalize(repo_root)) {
        (Ok(home), Ok(root)) => home == root,
        _ => home == repo_root,
    };
    (!same).then_some(home)
}

/// The conversation: on the screen when the terminal can hold one, and in
/// plain lines on `out` otherwise. The screen gives the terminal back before
/// this returns, so what the session says as it closes is written below the
/// conversation, through `out`, as in the line mode.
async fn conversation(
    running: &mut harness::Session,
    shown: &mut harness::event::Subscription,
    out: &Output,
    failures: &mut Failures,
    signals: &mut Signals,
    verbose: bool,
) -> Conversed {
    // Before the screen takes the terminal, which turns Ctrl-C into a key.
    let mut interrupts = match Interrupts::new() {
        Ok(interrupts) => interrupts,
        Err(e) => return Conversed::failed(e.into()),
    };
    let handed = if screen::wanted() {
        match handed_off(out, signals).await {
            Ok(handed) => handed,
            Err(sig) => {
                return Conversed {
                    signal: Some(sig),
                    result: Ok(()),
                };
            }
        }
    } else {
        false
    };
    let started = if handed {
        // Raced: the screen's first write can wait on a terminal that has
        // stopped reading, and a signal must still reach the stop.
        let start = Screen::start(verbose);
        tokio::pin!(start);
        let start = tokio::select! {
            start = &mut start => start,
            sig = signals.termination() => {
                screen::give_up(start).await;
                return Conversed { signal: Some(sig), result: Ok(()) };
            }
        };
        start
            .inspect_err(|e| {
                tracing::debug!(target: "outrig::cli::run_new", "no screen, reading plain lines: {e}");
            })
            .ok()
    } else {
        None
    };
    let Some(Started {
        mut screen,
        typed,
        keys,
        failures: mut drawn,
    }) = started
    else {
        let typed = match converse::read_lines() {
            Ok(typed) => typed,
            Err(e) => return Conversed::failed(e.into()),
        };
        return converse::converse(
            running,
            shown,
            Frontend::Lines(out),
            typed,
            &mut interrupts,
            failures,
            signals,
        )
        .await;
    };
    interrupts.listen(keys);
    let conversed = converse::converse(
        running,
        shown,
        Frontend::Screen(&screen),
        typed,
        &mut interrupts,
        &mut drawn,
        signals,
    )
    .await;
    // The terminal is the shell's again from here, so what the session says
    // as it closes -- the signal's notice, the report -- is written below the
    // conversation rather than drawn over by the screen.
    screen.finish().await;
    conversed.reconciled(&mut drawn)
}

/// Whether what the start said has been written, so the screen may take the
/// terminal; or the signal that ended the wait.
async fn handed_off(out: &Output, signals: &mut Signals) -> std::result::Result<bool, EndSignal> {
    tokio::select! {
        drained = tokio::time::timeout(HANDOFF_GRACE, out.drained()) => Ok(drained.is_ok()),
        sig = signals.termination() => Err(sig),
    }
}

/// What [`execute`] returns once the session is stopped: the status its report
/// decides, or the error that cut the session short -- the conversation's, or a
/// record that could not be written. The error keeps its text and its status 1,
/// except that a stop not proven outranks it, as it does everything in
/// [`exit_status`] -- a script reading the status must still be told that
/// something may be running.
///
/// A session a signal ended exits with the signal's own 128 plus its number,
/// as it would had the signal killed outrig outright, and as `run` and `mcp`
/// do; a stop not proven still outranks it. The signal is not an error: the
/// session was asked to end and did, so nothing is printed for it beyond the
/// notice already given. Nor does an error take its status: a write that
/// failed as the session closed is still printed, and the session exits as
/// it was asked to, since the stop it proved is the same.
fn outcome_of(ended: Result<()>, summary: &Summary, signalled: Option<EndSignal>) -> Result<i32> {
    let status = exit_status(summary);
    let status = match signalled {
        Some(sig) if status != 3 => sig.exit_code(),
        _ => status,
    };
    match ended {
        Err(e) if status == 3 || signalled.is_some() => Err(e.with_status(status)),
        other => other.map(|()| status),
    }
}

/// The exit status a session's report decides: 0 for a clean stop, 2 for a
/// stop that cut some Python off, 3 for one not proven stopped -- which
/// outranks everything, since something may still be running. A session the
/// interpreter's exit closed keeps its documented 1.
fn exit_status(summary: &Summary) -> i32 {
    match summary.verdict {
        Verdict::NotProvenStopped => 3,
        _ if summary.closed.is_some() => 1,
        Verdict::StoppedWithUnknown => 2,
        Verdict::Clean => 0,
        // A verdict this build does not know is not one it can call clean.
        _ => 3,
    }
}

/// The two lines the start prints for each of its steps, as each begins and
/// ends.
fn progress_lines(say: Say) -> impl FnMut(Progress) + Send + 'static {
    let mut span = None;
    move |progress| match progress {
        Progress::Started(step) => {
            span = Some(ProgressSpan::start_saying(&say, match step {
                Step::Packages => "installing binding packages",
                Step::Bindings => "starting bindings",
                Step::Container => "starting container",
                Step::Python => "starting python",
                _ => "starting",
            }));
        }
        Progress::Finished(step) => {
            if let Some(span) = span.take() {
                span.done(match step {
                    Step::Packages => "binding packages installed",
                    Step::Bindings => "bindings started",
                    Step::Container => "container ready",
                    Step::Python => "python ready",
                    _ => "ready",
                });
            }
        }
        _ => {}
    }
}

/// The image-config the session runs: `--image`, the agent's, `default-image`,
/// or outrig's built-in default, in that order. Also says whether it was the
/// built-in, for the startup line. The built-in comes without its servers and
/// sidecars, which a session that starts none has no use for.
fn image_config_name(
    cfg: &mut Config,
    image_flag: Option<&str>,
    agent: Option<&str>,
) -> Result<(String, bool)> {
    let chosen = image_flag
        .or_else(|| agent.and_then(|name| cfg.agents.get(name)?.image.as_deref()))
        .or(cfg.default_image.as_deref())
        .map(str::to_string);
    let Some(injection) = builtin_image::primary_for(cfg, chosen.as_deref()) else {
        return Ok((chosen.expect("a block of the user's was chosen"), false));
    };
    if chosen.is_none() && injection.applied {
        eprintln!(
            "[outrig] no --image, agent image, or default-image configured; using outrig's \
             built-in default"
        );
    }
    for note in &injection.notes {
        eprintln!("[outrig] {note}");
    }
    let name = match chosen {
        Some(name) => name,
        // A reserved name other than `[images.outrig-default]` is declared, so
        // injection was vetoed and there is nothing to fall back to. Name the
        // block that did it -- the one the note above names.
        None => injection.resolved.map(str::to_string).map_err(|block| {
            OutrigError::Configuration(format!(
                "no --image or default-image configured, and {block} shadows outrig's \
                 built-in default, leaving no [images.outrig-default] to fall back to"
            ))
        })?,
    };
    Ok((name, injection.applied))
}

/// Take the session's directory for this invocation, and create its `logs/`,
/// which the launch writes to before the record exists.
///
/// The record is written only once the interpreter is up, so until then the
/// directory is claimed by an exclusive lock on the directory itself. Without
/// it, a second `run-new --session-dir` naming the same directory would pass
/// every check while the first is still starting, and whichever failed would
/// write its record into the other's directory. The lock is not waited for:
/// a second invocation fails at once, having written nothing. The kernel
/// drops it with the process, so a killed invocation leaves nothing stale.
///
/// `explicit` is held to the rule `run`'s follows -- created if absent, refused
/// unless empty -- here rather than when the record is written, so a bad
/// `--session-dir` fails before anything starts.
fn reserve_session_dir(
    store: &SessionStore,
    sid: &SessionId,
    explicit: Option<&Path>,
) -> Result<(Flock<File>, PathBuf)> {
    let dir = match explicit {
        // Created now if absent, so there is a directory to lock; whether it
        // may be used is decided under the lock.
        Some(dir) => {
            if !dir.exists() {
                std::fs::create_dir_all(dir).path_ctx("create directory", dir)?;
            }
            dir.to_path_buf()
        }
        None => {
            let dir = store.symlink_path(sid);
            std::fs::create_dir_all(&dir).path_ctx("create directory", &dir)?;
            dir
        }
    };
    let handle = File::open(&dir).path_ctx("open", &dir)?;
    let reserved = Flock::lock(handle, FlockArg::LockExclusiveNonblock).map_err(|(_, e)| {
        OutrigError::Configuration(format!(
            "session directory {} is in use by another `outrig run-new` ({e})",
            dir.display()
        ))
    })?;
    // Checked under the lock, so two invocations cannot both find it free.
    if let Some(explicit) = explicit {
        session::prepare_explicit_dir(explicit)?;
    }
    let log_dir = dir.join("logs");
    std::fs::create_dir_all(&log_dir).path_ctx("create directory", &log_dir)?;
    Ok((reserved, log_dir))
}

struct Start<'a> {
    builder: SessionBuilder,
    cfg: &'a Config,
    image_cfg_name: &'a str,
    image_cfg: &'a ImageConfig,
    repo_root: &'a Path,
    log_dir: PathBuf,
    /// Filled in with the image tag and the container name as each is learned.
    record: &'a mut Session,
    /// Where the start's progress is said.
    say: &'a Say,
}

/// Ensure the image, then start the session in it. On failure nothing is left
/// running.
async fn start(args: Start<'_>) -> Result<harness::Session> {
    let Start {
        builder,
        cfg,
        image_cfg_name,
        image_cfg,
        repo_root,
        log_dir,
        record,
        say,
    } = args;

    // Ensured here, under the image-config's name as `run` and `build` do,
    // rather than left to `launch`: it never pulls an `image-name`, and it
    // would build a Dockerfile under a nameless tag of its own. A cache miss
    // shows its build as it runs, as `run`'s does, so minutes of building do
    // not read as a hang.
    let span = ProgressSpan::start_saying(say, format!("ensuring image for {image_cfg_name}"));
    let tag = image::compute_tag_for(image_cfg_name, image_cfg, repo_root).await?;
    let image = image::ensure_tagged_image_for_with_output(
        image_cfg_name,
        image_cfg,
        repo_root,
        &tag,
        false,
        None,
        BuildOutput::Stderr,
    )
    .await?;
    span.done(format!(
        "image ready: {} ({})",
        image.tag,
        if image.cache_hit { "cache hit" } else { "built" }
    ));
    record.image_tag = image.tag.to_string();

    let (spec, skipped) =
        launch_spec(cfg, image_cfg_name, &image.tag, repo_root, log_dir, &record.id).await?;
    if !skipped.is_empty() {
        say(&format!(
            "[outrig] run-new starts no MCP servers; not started: {}",
            skipped.join(", ")
        ));
    }

    let session = builder.container(spec).start().await?;
    record.container_name = session.container_name().to_string();
    Ok(session)
}

/// The launch for `image_cfg_name`, running the already-ensured `tag`, with no
/// MCP server and no sidecar in it -- and the names of the servers left out.
/// Its container is named for `sid` and carries it as `org.outrig.session`, as
/// `run`'s is.
///
/// The image-config is replaced by one naming `tag`, keeping only its
/// security, so the image that runs is the one the session records and
/// `launch` has nothing to build.
async fn launch_spec(
    cfg: &Config,
    image_cfg_name: &str,
    tag: &ImageTag,
    repo_root: &Path,
    log_dir: PathBuf,
    sid: &SessionId,
) -> Result<(LaunchSpec, Vec<String>)> {
    let container =
        harness::container_spec(cfg, image_cfg_name, Some(tag), repo_root, log_dir).await?;
    Ok((
        container.spec.with_session_id(sid.as_str()),
        container.left_out,
    ))
}

struct Banner<'a> {
    image_config_row: String,
    image_tag: &'a str,
    model: &'a str,
    python_version: &'a str,
    container_name: &'a str,
    session_id: &'a str,
}

fn render_banner(banner: &Banner<'_>) -> String {
    format!(
        "{}\n\
         [outrig] image:         {}\n\
         [outrig] model:         {}\n\
         [outrig] python {} ready in {}\n\
         [outrig] session id: {}   (Ctrl-D to exit, /help for slash commands)\n",
        banner.image_config_row,
        banner.image_tag,
        banner.model,
        banner.python_version,
        banner.container_name,
        banner.session_id,
    )
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;
    use outrig::EmbeddedMcpPolicy;

    use super::*;

    /// Every session flag is refused before a script: a flag added to [`RunNewArgs`] and not to
    /// [`SESSION_FLAGS`] would be taken and then ignored.
    #[test]
    fn every_session_flag_is_refused_before_a_script() {
        let mut ids: Vec<String> = RunNewArgs::command()
            .get_arguments()
            .map(|arg| arg.get_id().to_string())
            .filter(|id| !["help", "grace", "program"].contains(&id.as_str()))
            .collect();
        ids.sort();
        let mut listed = SESSION_FLAGS.map(str::to_string).to_vec();
        listed.sort();
        assert_eq!(ids, listed);
    }

    /// A controlling terminal whose other end closed is no longer one, and
    /// that is the whole of what says it hung up: not a pipe that closes, not
    /// a terminal that is merely quiet, and not another terminal a stream was
    /// redirected to, whose hangup is its own session's (#644).
    #[test]
    fn only_a_controlling_terminal_that_closed_has_hung_up() {
        let pty = nix::pty::openpty(None, None).expect("a pty");
        let slave = File::from(pty.slave);
        let all = || [&slave as &dyn Stream, &slave, &slave];
        let controlling = Terminals::of(all(), |_| true);
        let unrelated = Terminals::of(all(), |stream| {
            crate::repl::is_controlling_tty(&stream.as_raw_fd())
        });
        assert!(!controlling.hung_up(all()), "a quiet terminal");

        let (read, write) = std::io::pipe().expect("a pipe");
        let write = File::from(std::os::fd::OwnedFd::from(write));
        let on_a_pipe = Terminals::of([&write, &write, &write], |_| true);
        drop(read);
        assert!(!on_a_pipe.hung_up([&write, &write, &write]), "a pipe");

        drop(pty.master);
        assert!(controlling.hung_up(all()), "a hung-up controlling terminal");
        assert!(
            !unrelated.hung_up(all()),
            "a terminal that is not this process's own"
        );
    }

    /// The user's skills are served after the project's, from the home directory, unless the
    /// repository is the home directory -- reached through a link or not -- whose
    /// `.agents/skills` the project's source already serves.
    #[test]
    fn the_users_skills_are_served_unless_home_is_the_repository() {
        let home = tempfile::tempdir().expect("a home");
        let repo = home.path().join("repo");
        std::fs::create_dir(&repo).expect("a repository");
        let at_home = Some(home.path().to_path_buf());
        assert_eq!(user_skills_home(&repo, at_home.clone()), at_home);
        assert_eq!(user_skills_home(home.path(), at_home.clone()), None);
        let link = home.path().join("link");
        std::os::unix::fs::symlink(&repo, &link).expect("a link to the repository");
        assert_eq!(user_skills_home(&link, Some(repo.clone())), None);
        assert_eq!(user_skills_home(&repo, None), None);
    }

    /// A config whose image carries a primary server holding a secret and a
    /// sidecar-hosted server, the two ways an MCP server could start.
    const WITH_SERVERS: &str = r#"
[images.primary]
dockerfile = "Dockerfile"
context    = "."

[images.primary.security]
cap-add = ["NET_ADMIN"]

[images.tools]
image-name = "docker.io/library/alpine:latest"

[sidecars.tools]
image = "tools"

[images.primary.mcp]
leaky = { command = ["/nonexistent-mcp"], env = { TOKEN = "${OUTRIG_TEST_RUN_NEW_UNSET_SECRET}" } }
fs    = { command = ["mcp-fs"], sidecar = "tools" }
"#;

    /// The policy, asserted on what is launched rather than on what Python can
    /// read: the spec carries no server and no sidecar, ignores the image's
    /// label, and names what it left out. Were `leaky` started, resolving its
    /// unset `${..}` alone would fail the launch. The image-config, here a
    /// Dockerfile, is pinned to the tag already ensured and keeps its security.
    /// The spec carries the session id, so the container is named and labeled
    /// for it.
    #[tokio::test]
    async fn the_launch_starts_no_mcp_server_and_no_sidecar() {
        let cfg = Config::load_from_str(WITH_SERVERS).expect("config parses");
        cfg.validate(None).expect("config validates");
        let repo = tempfile::tempdir().expect("a repo dir");

        let tag = ImageTag::new("docker.io/library/alpine:latest");
        let sid = SessionId("20260925T000000-abcd".to_string());
        let (spec, skipped) = launch_spec(
            &cfg,
            "primary",
            &tag,
            repo.path(),
            repo.path().join("logs"),
            &sid,
        )
        .await
        .expect("lowers without touching podman");

        assert_eq!(spec.session_id.as_deref(), Some("20260925T000000-abcd"));
        assert!(spec.mcp.is_empty(), "{:?}", spec.mcp.keys());
        assert!(spec.sidecars.is_empty(), "a sidecar would start");
        assert_eq!(spec.embedded_mcp_policy, EmbeddedMcpPolicy::Ignore);
        assert_eq!(skipped, ["fs", "leaky"]);
        assert_eq!(
            format!("{:?}", spec.security),
            format!(
                "{:?}",
                outrig::SecuritySpec::from(cfg.images["primary"].security())
            ),
        );
        assert!(
            cfg.images["primary"].mcp().contains_key("leaky") && cfg.sidecars.contains_key("tools"),
            "the session's own config is left whole"
        );
    }

    /// A summary of a report that `verdict` reads as, and nothing else.
    fn summary(verdict: Verdict) -> Summary {
        Summary {
            verdict,
            closed: None,
            not_stopped: None,
            executions: Vec::new(),
            hosted: Vec::new(),
            last_sequence: 0,
            missed: 0,
            log: None,
        }
    }

    /// A summary of a report that `verdict` reads as, for a session the
    /// interpreter's exit closed.
    fn died(verdict: Verdict) -> Summary {
        Summary {
            closed: Some("the Python interpreter exited (exit status: 1)".to_string()),
            ..summary(verdict)
        }
    }

    /// What a stop found decides the status, and one not proven stopped
    /// outranks everything. A report that reads not proven stopped cannot be
    /// had from podman on demand, so this is where 3 is forced.
    #[test]
    fn the_report_decides_the_exit_status() {
        assert_eq!(exit_status(&summary(Verdict::Clean)), 0);
        assert_eq!(exit_status(&summary(Verdict::StoppedWithUnknown)), 2);
        assert_eq!(exit_status(&summary(Verdict::NotProvenStopped)), 3);
        assert_eq!(exit_status(&died(Verdict::Clean)), 1);
        assert_eq!(exit_status(&died(Verdict::StoppedWithUnknown)), 1);
        assert_eq!(exit_status(&died(Verdict::NotProvenStopped)), 3);
    }

    /// A session cut short by an error, as a write to a stdout whose reader
    /// went away does to the conversation.
    fn failed() -> Result<()> {
        Err(std::io::Error::other("stdout went away").into())
    }

    /// An error that cuts the session short keeps its text; only a stop not
    /// proven moves its status from 1 to 3.
    #[test]
    fn an_error_exits_3_only_when_the_stop_is_not_proven() {
        let text = failed().expect_err("it failed").to_string();

        let stuck = outcome_of(failed(), &summary(Verdict::NotProvenStopped), None)
            .expect_err("the conversation's error is returned");
        assert_eq!(stuck.exit_code(), 3);
        assert_eq!(stuck.to_string(), text, "the error is printed as it was");

        for other in [
            summary(Verdict::Clean),
            summary(Verdict::StoppedWithUnknown),
            died(Verdict::Clean),
        ] {
            let err = outcome_of(failed(), &other, None).expect_err("the error is returned");
            assert_eq!(err.exit_code(), 1, "{other:?}");
            assert_eq!(err.to_string(), text);
        }
    }

    /// A signal that ended the session decides the status, as it does for
    /// `run` and `mcp`: 128 plus its number, and the record keeps the same.
    /// It is not an error, so nothing is printed for it.
    #[test]
    fn a_signal_exits_128_plus_its_number() {
        for (sig, code) in [
            (EndSignal::Hangup, 129),
            (EndSignal::Interrupt, 130),
            (EndSignal::Terminate, 143),
        ] {
            let status = outcome_of(Ok(()), &summary(Verdict::Clean), Some(sig))
                .expect("a signal is not an error");
            assert_eq!(status, code, "{sig}");
        }
        // The session did stop cleanly, so what the report found is the
        // signal's status and not the report's 0.
        assert_eq!(exit_status(&summary(Verdict::Clean)), 0);
    }

    /// A write that failed as the signalled session closed is still its
    /// error, printed as it was, but the session leaves as it was asked to:
    /// the stop it proved is the same, so the process and the record keep the
    /// signal's status and not the error's 1.
    #[test]
    fn a_signal_keeps_its_status_when_a_write_failed() {
        let text = failed().expect_err("it failed").to_string();
        for (sig, code) in [(EndSignal::Hangup, 129), (EndSignal::Terminate, 143)] {
            for proven in [summary(Verdict::Clean), summary(Verdict::StoppedWithUnknown)] {
                let err = outcome_of(failed(), &proven, Some(sig))
                    .expect_err("the write's error is still returned");
                assert_eq!(err.exit_code(), code, "{sig}: {proven:?}");
                assert_eq!(err.to_string(), text, "the error is printed as it was");
            }
        }
    }

    /// A stop not proven outranks the signal: a script reading the status
    /// must still be told that something may be running.
    #[test]
    fn a_stop_not_proven_outranks_a_signal() {
        let status = outcome_of(
            Ok(()),
            &summary(Verdict::NotProvenStopped),
            Some(EndSignal::Terminate),
        )
        .expect("no error cut it short");
        assert_eq!(status, 3);

        let stuck = outcome_of(
            failed(),
            &summary(Verdict::NotProvenStopped),
            Some(EndSignal::Terminate),
        )
        .expect_err("the error is still returned");
        assert_eq!(stuck.exit_code(), 3);
    }

    /// What a person needs before typing: the model, and that Python is ready
    /// and which one.
    #[test]
    fn the_banner_names_the_model_and_the_python() {
        let banner = render_banner(&Banner {
            image_config_row: builtin_image::banner_image_config_row("outrig-default", true),
            image_tag: "docker.io/library/alpine:latest",
            model: "sonnet",
            python_version: "3.13.15",
            container_name: "outrig-20260925T000000-abcd",
            session_id: "20260925T000000-abcd",
        });
        assert_eq!(
            banner,
            "[outrig] image-config:  outrig-default (built-in default)\n\
             [outrig] image:         docker.io/library/alpine:latest\n\
             [outrig] model:         sonnet\n\
             [outrig] python 3.13.15 ready in outrig-20260925T000000-abcd\n\
             [outrig] session id: 20260925T000000-abcd   (Ctrl-D to exit, /help for slash commands)\n"
        );
    }

    /// The rule `run`'s `--session-dir` follows: a directory that is not there
    /// yet is created, with its parents, and one that holds anything is
    /// refused -- a record by what it is, anything else by name.
    #[test]
    fn a_session_dir_is_created_if_absent_and_must_be_empty() {
        let root = tempfile::tempdir().expect("a root");
        let store = SessionStore::new(root.path().to_path_buf());
        let sid = SessionId::from("sid".to_string());
        let missing = root.path().join("missing/nested");
        let (_reserved, log_dir) =
            reserve_session_dir(&store, &sid, Some(&missing)).expect("an absent dir is created");
        assert_eq!(log_dir, missing.join("logs"));
        assert!(log_dir.is_dir());

        let used = tempfile::tempdir().expect("a used dir");
        std::fs::write(used.path().join("session.json"), "{}").expect("a record");
        let err = reserve_session_dir(&store, &sid, Some(used.path()))
            .expect_err("a dir with a record is refused")
            .to_string();
        assert!(err.contains("holds an earlier session's record"), "{err}");

        let notes = tempfile::tempdir().expect("a dir of notes");
        std::fs::write(notes.path().join("notes.md"), "mine").expect("a note");
        let err = reserve_session_dir(&store, &sid, Some(notes.path()))
            .expect_err("a dir with anything in it is refused")
            .to_string();
        assert!(err.contains("is not empty (it holds notes.md)"), "{err}");
        assert!(
            !notes.path().join("logs").exists(),
            "a refused dir gets nothing"
        );

        let (_reserved, log_dir) =
            reserve_session_dir(&store, &sid, None).expect("the default layout");
        assert_eq!(log_dir, root.path().join("sid").join("logs"));
        assert!(log_dir.is_dir());
    }

    /// Two invocations naming one directory: the first holds it until it
    /// returns, and the second is refused before it creates or writes anything
    /// there. Once the first lets go, the directory can be taken again.
    #[test]
    fn a_session_dir_is_held_by_one_invocation_at_a_time() {
        let root = tempfile::tempdir().expect("a root");
        let store = SessionStore::new(root.path().to_path_buf());
        let dir = tempfile::tempdir().expect("a session dir");
        let first = SessionId::from("first".to_string());
        let second = SessionId::from("second".to_string());

        let held = reserve_session_dir(&store, &first, Some(dir.path())).expect("free");
        std::fs::remove_dir(dir.path().join("logs")).expect("the first made logs/");
        let err = reserve_session_dir(&store, &second, Some(dir.path()))
            .expect_err("held by the first")
            .to_string();
        assert!(err.contains("is in use by another `outrig run-new`"), "{err}");
        assert!(
            !dir.path().join("logs").exists(),
            "the refused one made nothing there"
        );

        drop(held);
        reserve_session_dir(&store, &second, Some(dir.path())).expect("free again");
    }
}
