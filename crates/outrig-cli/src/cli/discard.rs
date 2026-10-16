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
use crate::session::{self, DanglingLink, Session, SessionStore};
use outrig::container::Container;
use outrig::error::IoPathExt;

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
    global_override: Option<&Path>,
) -> Result<i32> {
    // `--session-dir` needs the root too: the links under it that name the
    // directory go with it.
    let root = session::resolve_session_root_for_cli(session_root_flag, global_override)?;
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

    if is_running(target.container_name.clone()).await? {
        return Err(OutrigError::Configuration(format!(
            "session {} is still running (container {}); stop it before discarding",
            target.id, target.container_name
        ))
        .into());
    }

    if !args.yes && !confirm(stderr, stdin, &target.dir).await? {
        stderr.write_all(b"[outrig] aborted\n").await?;
        return Ok(0);
    }

    if !target.dangling.is_empty() {
        for link in &target.dangling {
            let (_, msg) = super::unlink_dangling_report(store, link)?;
            stderr.write_all(msg.as_bytes()).await?;
        }
        return Ok(0);
    }

    let left = if args.session_dir.is_some() {
        store.remove_by_path(&target.dir)?
    } else {
        store.remove_by_id(&target.id)?
    };

    let dir_msg = super::removal_report(&target.dir, &left);
    stderr.write_all(dir_msg.as_bytes()).await?;
    if args.session_dir.is_none() && target.linked {
        let link_path = store.symlink_path(&target.id);
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
    /// The session id; the first of them when `--session-dir` names a
    /// directory that several dangling links point at.
    id: crate::session::SessionId,
    /// Whether `<root>/<id>` is a symlink to `dir`.
    linked: bool,
    /// The links to unlink, and nothing more, when `dir` is gone: the ones
    /// that were dangling when selected, so a directory recreated before
    /// the removal is never reached through them.
    dangling: Vec<DanglingLink>,
    /// The container the discard would pull the record out from under.
    container_name: String,
}

fn resolve_target(args: &DiscardArgs, store: &SessionStore) -> Result<Target> {
    if let Some(dir) = args.session_dir.as_deref() {
        if !dir.exists() {
            // No record to read, so the container comes from the links that
            // name the directory. Nothing naming it is a mistyped path.
            let dir = normalize_missing(dir)?;
            let links: Vec<_> = store
                .list()?
                .dangling
                .into_iter()
                .filter(|d| d.target == dir)
                .collect();
            let Some(first) = links.first() else {
                return Err(OutrigError::Configuration(format!(
                    "--session-dir {} does not exist and no session links to it",
                    dir.display()
                ))
                .into());
            };
            return Ok(Target {
                dir,
                id: first.id(),
                linked: true,
                container_name: first.container_name(),
                dangling: links,
            });
        }
        let session = store.get_by_path(dir)?;
        // Given a symlink, the record is in the directory it names, so that is
        // the one the prompt and the report have to name.
        let dir = std::fs::canonicalize(dir).path_ctx("resolve", dir)?;
        return Ok(Target::from_session(dir, session));
    }
    let Some(query) = args.session.as_deref() else {
        return Err(OutrigError::Configuration(
            "outrig discard requires either a session id or --session-dir".to_string(),
        )
        .into());
    };
    Ok(match super::resolve_found(store, query)? {
        super::Found::Session(dir, session) => Target::from_session(dir, *session),
        super::Found::Dangling(link) => Target {
            dir: link.target.clone(),
            id: link.id(),
            linked: true,
            container_name: link.container_name(),
            dangling: vec![link],
        },
    })
}

/// `dir` as `SessionStore::create` would have recorded it had it existed:
/// absolute against the cwd, symlinks resolved as far as the path exists.
/// A symlink whose destination is gone is followed too, so a path through
/// one lands where the session's own link points; what is missing past that
/// is appended as written.
fn normalize_missing(dir: &Path) -> Result<PathBuf> {
    use std::ffi::OsString;

    const MAX_LINKS: usize = 40;
    let abs = std::path::absolute(dir).path_ctx("resolve", dir)?;
    let mut pending: std::collections::VecDeque<OsString> =
        abs.iter().map(OsString::from).collect();
    let mut resolved = PathBuf::new();
    let mut links = 0;
    while let Some(part) = pending.pop_front() {
        if part == "/" {
            resolved = PathBuf::from("/");
        } else if part == "." {
        } else if part == ".." {
            resolved.pop();
        } else {
            let next = resolved.join(&part);
            if std::fs::symlink_metadata(&next).is_ok_and(|m| m.is_symlink()) {
                links += 1;
                if links > MAX_LINKS {
                    return Err(OutrigError::Configuration(format!(
                        "--session-dir {} passes through too many symlinks",
                        dir.display()
                    ))
                    .into());
                }
                let target = std::fs::read_link(&next).path_ctx("read symlink", &next)?;
                for piece in target.iter().rev() {
                    pending.push_front(piece.to_os_string());
                }
            } else {
                resolved = next;
            }
        }
    }
    Ok(resolved)
}

impl Target {
    fn from_session(dir: PathBuf, session: Session) -> Self {
        Self {
            dir,
            linked: session.link_target.is_some(),
            dangling: Vec::new(),
            container_name: session.container_name,
            id: session.id,
        }
    }
}

async fn podman_is_running(name: String) -> Result<bool> {
    Ok(Container::is_running(&name).await?)
}
