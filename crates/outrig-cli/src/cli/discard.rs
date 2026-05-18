//! `outrig discard` -- delete a session's on-disk record.
//!
//! Refuses if the session's container is still running (would race the
//! `outrig run` writer). The running-check is closure-injected so tests can
//! drive both branches without a podman dependency.

use std::future::Future;
use std::path::{Path, PathBuf};

use clap::{ArgGroup, Parser};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};

use crate::error::{OutrigError, Result};
use crate::session::{self, Session, SessionStore};
use outrig::container::Container;

#[derive(Debug, Parser)]
#[command(group(
    ArgGroup::new("discard_target")
        .args(["session", "session_dir"])
        .multiple(false)
        .required(false)
))]
pub struct DiscardArgs {
    /// Session id (substring match if unambiguous).
    pub session: Option<String>,
    /// Skip the interactive `[y/N]` confirmation.
    #[arg(short = 'y', long = "yes")]
    pub yes: bool,
    /// Discard exactly this session dir, skipping the id lookup.
    #[arg(long = "session-dir", value_name = "PATH")]
    pub session_dir: Option<PathBuf>,
}

pub async fn execute(
    args: &DiscardArgs,
    session_root_flag: Option<&Path>,
    repo_cfg_override: Option<&Path>,
    global_cfg_path: &Path,
    cwd: &Path,
) -> Result<i32> {
    // `--session-dir` skips the id lookup entirely, so the root is unused.
    // Resolving it anyway would surface a config-parse error on a path
    // that doesn't need the config at all.
    let root = if args.session_dir.is_some() {
        PathBuf::new()
    } else {
        session::resolve_session_root_for_cli(
            session_root_flag,
            repo_cfg_override,
            global_cfg_path,
            cwd,
        )?
    };
    let store = SessionStore::new(root);
    let stdin = tokio::io::BufReader::new(tokio::io::stdin());
    let mut stderr = tokio::io::stderr();
    execute_with(&mut stderr, stdin, &store, args, podman_is_running).await
}

/// Inner form: every effect is injected so the test suite can drive both
/// the "container running" and "user typed n" branches deterministically.
/// `is_running` takes `String` (not `&str`) so its returned future is
/// `'static` -- avoids the HRTB friction with `FnOnce(&str) -> impl Future`.
pub async fn execute_with<E, R, F, Fut>(
    stderr: &mut E,
    stdin: R,
    store: &SessionStore,
    args: &DiscardArgs,
    is_running: F,
) -> Result<i32>
where
    E: AsyncWrite + Unpin,
    R: AsyncBufRead + Unpin,
    F: FnOnce(String) -> Fut,
    Fut: Future<Output = Result<bool>>,
{
    let target = resolve_target(args, store)?;

    if is_running(target.session.container_name.clone()).await? {
        return Err(OutrigError::Configuration(format!(
            "session {} is still running (container {}); stop it before discarding",
            target.session.id, target.session.container_name
        ))
        .into());
    }

    if !args.yes && !confirm(stderr, stdin, &target.dir).await? {
        stderr.write_all(b"[outrig] aborted\n").await?;
        return Ok(0);
    }

    if args.session_dir.is_some() {
        store.remove_by_path(&target.dir)?;
    } else {
        store.remove_by_id(&target.session.id)?;
    }

    let dir_msg = format!("[outrig] removed {}\n", target.dir.display());
    stderr.write_all(dir_msg.as_bytes()).await?;
    if args.session_dir.is_none() && target.session.link_target.is_some() {
        let link_path = store.symlink_path(&target.session.id);
        let link_msg = format!("[outrig] removed {} (symlink)\n", link_path.display());
        stderr.write_all(link_msg.as_bytes()).await?;
    }
    Ok(0)
}

async fn confirm<E, R>(stderr: &mut E, mut stdin: R, dir: &Path) -> Result<bool>
where
    E: AsyncWrite + Unpin,
    R: AsyncBufRead + Unpin,
{
    let prompt = format!("Discard {}? [y/N]: ", dir.display());
    stderr.write_all(prompt.as_bytes()).await?;
    stderr.flush().await?;
    let mut line = String::new();
    stdin.read_line(&mut line).await?;
    let answer = line.trim().to_ascii_lowercase();
    Ok(answer == "y" || answer == "yes")
}

struct Target {
    dir: PathBuf,
    session: Session,
}

fn resolve_target(args: &DiscardArgs, store: &SessionStore) -> Result<Target> {
    if let Some(dir) = args.session_dir.as_deref() {
        let session = store.get_by_path(dir)?;
        return Ok(Target {
            dir: dir.to_path_buf(),
            session,
        });
    }
    let Some(query) = args.session.as_deref() else {
        return Err(OutrigError::Configuration(
            "outrig discard requires either a session id or --session-dir".to_string(),
        )
        .into());
    };
    let (dir, session) = super::resolve_session_arg(store, query)?;
    Ok(Target { dir, session })
}

async fn podman_is_running(name: String) -> Result<bool> {
    Ok(Container::is_running(&name).await?)
}
