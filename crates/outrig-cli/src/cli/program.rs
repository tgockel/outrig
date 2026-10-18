//! `outrig run <script.py> [args...]`: a trusted Python program on the host (preview).
//!
//! The configuration is loaded once, from the invocation's directory, before the program starts;
//! the program gets a read-only view of it as `outrig.config()`. Its streams, working directory,
//! environment and exit status are its own, and nothing here reads what it prints. `SIGINT` and
//! `SIGTERM` this process receives are forwarded to the program's process group, which is all
//! that is done with them: the program has no session yet to clean up after.

use std::ffi::OsString;
use std::path::Path;

use clap::Args;
use outrig::harness::HostProgram;
use tokio::signal::unix::{SignalKind, signal};

use crate::cli::run::RunArgs;
use crate::error::{OutrigError, Result};
use crate::paths::RepoConfig;

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

    /// A Python program to run on the host, then its own arguments (preview). Everything after
    /// it is the program's, `--help` and OutRig's own flags included.
    #[arg(
        value_name = "SCRIPT",
        trailing_var_arg = true,
        conflicts_with_all = SESSION_FLAGS
    )]
    pub program: Vec<OsString>,
}

/// Run the program `program` names -- its script, then its arguments -- to its end, and return
/// the status to exit with: the program's, or 128 plus the signal that ended it.
pub async fn execute(
    repo: &RepoConfig,
    global_cfg_path: &Path,
    program: &[OsString],
) -> Result<i32> {
    let (script, args) = program
        .split_first()
        .expect("a program is its script and then its arguments");
    let cfg = repo.load(global_cfg_path)?;
    let mut running = HostProgram::new(cfg, script)
        .repo_root(&repo.root)
        .args(args)
        .start()
        .await?;
    // Only now: before the program exists, a signal ends this process by its default action,
    // with nothing left to clean up.
    let mut interrupts = signal(SignalKind::interrupt()).map_err(OutrigError::Io)?;
    let mut terminations = signal(SignalKind::terminate()).map_err(OutrigError::Io)?;
    loop {
        tokio::select! {
            exit = running.wait() => return Ok(exit?.code()),
            Some(()) = interrupts.recv() => running.interrupt(),
            Some(()) = terminations.recv() => running.terminate(),
        }
    }
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
