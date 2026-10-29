//! `outrig run-new <script.py> [args...]`: a trusted Python program on the host (preview).
//!
//! The configuration is loaded once, from the invocation's directory, before the program starts;
//! the program gets a read-only view of it as `outrig.config()`, and each session it enters is
//! started from it, in the image `default-image` names or outrig's built-in default, with its
//! directory under the session root. Its streams, working directory, environment and exit status
//! are its own, and nothing here reads what it prints.
//!
//! `SIGINT` and `SIGTERM` this process receives are forwarded to the program's process group,
//! and so is a `SIGHUP`, as the `SIGTERM` a closed terminal asks for. The first starts the
//! program's grace (`--grace`, `[host] grace-secs`); past it, or at a second signal, the
//! program's group is stopped, and a further signal sends `SIGKILL` at once. Whatever the program
//! left open, the engine stops, and each such session is told of here, on stderr.
//!
//! None of the three is left at its default disposition once the program exists: a `SIGHUP` that
//! was would end this process where it stands, and with it the engine that stops the sessions the
//! program left open -- the containers would run on under conmon, and their records would stay
//! live (#644).

use std::ffi::OsString;
use std::path::Path;
use std::time::Duration;

use outrig::harness::HostProgram;
use tokio::signal::unix::{SignalKind, signal};

use crate::builtin_image;
use crate::cli::eprint_best_effort;
use crate::cli::report::render_left_open;
use crate::error::{OutrigError, Result};
use crate::paths::{RepoConfig, default_session_root};
use crate::session::resolve_session_root;

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
    // A closed terminal asks for the program to stop, which is what a SIGTERM asks for, so it is
    // forwarded as one: the program has one way to be asked to end, and the grace and the stop
    // that follow are the same. Left at its default, it would kill this process with the
    // program's sessions still running.
    let mut hangups = signal(SignalKind::hangup()).map_err(OutrigError::Io)?;
    let exit = loop {
        tokio::select! {
            exit = running.wait() => break exit?,
            Some(()) = interrupts.recv() => running.interrupt(),
            Some(()) = terminations.recv() => running.terminate(),
            Some(()) = hangups.recv() => running.terminate(),
        }
    };
    for left in exit.left_open() {
        eprint_best_effort(&render_left_open(left));
    }
    Ok(exit.code())
}
