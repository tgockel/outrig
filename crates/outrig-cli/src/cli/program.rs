//! `outrig run <script.py> [args...]`: a trusted Python program on the host (preview).
//!
//! The configuration is loaded once, from the invocation's directory, before the program starts;
//! the program gets a read-only view of it as `outrig.config()`, and each session it enters is
//! started from it, in the image `default-image` names or outrig's built-in default, with its
//! directory under the session root. Its streams, working directory, environment and exit status
//! are its own, and nothing here reads what it prints.
//!
//! `SIGINT` and `SIGTERM` this process receives are forwarded to the program's process group. The
//! first starts the program's grace (`--grace`, `[host] grace-secs`); past it, or at a second
//! signal, the program's group is stopped, and a further signal sends `SIGKILL` at once. Whatever
//! the program left open, the engine stops, and each such session is told of here, on stderr.

use std::ffi::OsString;
use std::path::Path;
use std::time::Duration;

use clap::Args;
use outrig::config::HOST_GRACE_SECS_CEILING;
use outrig::harness::HostProgram;
use tokio::signal::unix::{SignalKind, signal};

use crate::builtin_image;
use crate::cli::eprint_best_effort;
use crate::cli::report::render_left_open;
use crate::cli::run::RunArgs;
use crate::error::{OutrigError, Result};
use crate::paths::{RepoConfig, default_session_root};
use crate::session::resolve_session_root;

/// The ids of [`RunArgs`]' flags: the interactive session's, which a program, choosing its own
/// session's inputs, refuses. Listed rather than named as [`RunArgs`]' group, so that clap's
/// refusal names the flag given rather than every flag in the group.
const SESSION_FLAGS: [&str; 9] = [
    "agent",
    "model",
    "image",
    "session_dir",
    "max_tool_calls",
    "max_tool_result_bytes",
    "env",
    "network",
    "volume",
];

/// `outrig run`'s arguments: the interactive session's flags, or a program and its arguments.
#[derive(Debug, Args)]
pub struct RunCommandArgs {
    #[command(flatten)]
    pub session: RunArgs,

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

/// Run the program `program` names -- its script, then its arguments -- to its end, stop what it
/// left open, and return the status to exit with: the program's, or 128 plus the signal that ended
/// it, or 3 when a session it left open is not proven stopped.
pub async fn execute(
    repo: &RepoConfig,
    global_cfg_path: Option<&Path>,
    session_root: Option<&Path>,
    grace: Option<u64>,
    program: &[OsString],
) -> Result<i32> {
    let (script, args) = program
        .split_first()
        .expect("a program is its script and then its arguments");
    let mut cfg = repo.load(global_cfg_path)?;
    cfg.session_root = Some(resolve_session_root(
        session_root,
        &cfg,
        &default_session_root(),
    ));
    // As `run-new` falls back to it: a program's sessions run in outrig's built-in default when
    // `default-image` names nothing.
    let chosen = cfg.default_image.clone();
    if let Some(injection) = builtin_image::primary_for(&mut cfg, chosen.as_deref())
        && chosen.is_none()
        && let Ok(name) = injection.resolved
    {
        cfg.default_image = Some(name.to_string());
    }
    let mut host = HostProgram::new(cfg, script).repo_root(&repo.root).args(args);
    if let Some(secs) = grace {
        host = host.grace(Duration::from_secs(secs));
    }
    let mut running = host.start().await?;
    // Only now: before the program exists, a signal ends this process by its default action,
    // with nothing left to clean up.
    let mut interrupts = signal(SignalKind::interrupt()).map_err(OutrigError::Io)?;
    let mut terminations = signal(SignalKind::terminate()).map_err(OutrigError::Io)?;
    let exit = loop {
        tokio::select! {
            exit = running.wait() => break exit?,
            Some(()) = interrupts.recv() => running.interrupt(),
            Some(()) = terminations.recv() => running.terminate(),
        }
    };
    for left in exit.left_open() {
        eprint_best_effort(&render_left_open(left));
    }
    Ok(exit.code())
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;

    use super::{RunArgs, SESSION_FLAGS};

    /// Every flag of the interactive session is refused before a script: a flag added to
    /// [`RunArgs`] and not to [`SESSION_FLAGS`] would be taken and then ignored.
    #[test]
    fn every_session_flag_is_refused_before_a_script() {
        let mut ids: Vec<String> = RunArgs::command()
            .get_arguments()
            .map(|arg| arg.get_id().to_string())
            .filter(|id| id != "help")
            .collect();
        ids.sort();
        let mut listed = SESSION_FLAGS.map(str::to_string).to_vec();
        listed.sort();
        assert_eq!(ids, listed);
    }
}
