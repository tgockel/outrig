//! `outrig clean` -- bulk-remove old session records.
//!
//! This is intentionally a session-store command, not a container lifecycle
//! command: it removes old metadata/log directories and refuses to touch
//! sessions whose container is still running.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use clap::Parser;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};

use crate::error::Result;
use crate::session::{self, Session, SessionStore};
use outrig::container::Container;

const DAY: u64 = 24 * 60 * 60;
pub const DEFAULT_OLDER_THAN: Duration = Duration::from_secs(30 * DAY);

#[derive(Debug, Parser)]
pub struct CleanArgs {
    /// Remove sessions older than this duration. Supports s, m, h, and d.
    #[arg(
        long = "older-than",
        value_name = "DURATION",
        default_value = "30d",
        value_parser = parse_duration
    )]
    pub older_than: Duration,
    /// Skip the interactive `[y/N]` confirmation.
    #[arg(short = 'y', long = "yes")]
    pub yes: bool,
}

pub async fn execute(
    args: &CleanArgs,
    session_root_flag: Option<&Path>,
    repo_cfg_override: Option<&Path>,
    global_cfg_path: &Path,
    cwd: &Path,
) -> Result<i32> {
    let root = session::resolve_session_root_for_cli(
        session_root_flag,
        repo_cfg_override,
        global_cfg_path,
        cwd,
    )?;
    let store = SessionStore::new(root);
    let stdin = tokio::io::BufReader::new(tokio::io::stdin());
    let mut stderr = tokio::io::stderr();
    execute_with(
        &mut stderr,
        stdin,
        &store,
        args,
        SystemTime::now(),
        podman_is_running,
    )
    .await
}

pub async fn execute_with<E, R, F, Fut>(
    stderr: &mut E,
    stdin: R,
    store: &SessionStore,
    args: &CleanArgs,
    now: SystemTime,
    mut is_running: F,
) -> Result<i32>
where
    E: AsyncWrite + Unpin,
    R: AsyncBufRead + Unpin,
    F: FnMut(String) -> Fut,
    Fut: Future<Output = Result<bool>>,
{
    let mut targets = Vec::new();
    let mut skipped_running = Vec::new();

    for session in store.list()? {
        if !older_than(&session, args.older_than, now) {
            continue;
        }
        if is_running(session.container_name.clone()).await? {
            skipped_running.push(session);
            continue;
        }
        targets.push(CleanTarget {
            dir: session.session_dir.clone(),
            session,
        });
    }

    write_skipped_running(stderr, &skipped_running).await?;

    let retention = format_retention(args.older_than);
    if targets.is_empty() {
        let msg = format!("[outrig] no stopped sessions older than {retention}\n");
        stderr.write_all(msg.as_bytes()).await?;
        return Ok(0);
    }

    write_preview(stderr, &targets, &retention).await?;

    if !args.yes && !confirm(stderr, stdin, targets.len()).await? {
        stderr.write_all(b"[outrig] aborted\n").await?;
        return Ok(0);
    }

    let mut removed = 0usize;
    for target in &targets {
        store.remove_by_id(&target.session.id)?;
        removed += 1;

        let dir_msg = format!("[outrig] removed {}\n", target.dir.display());
        stderr.write_all(dir_msg.as_bytes()).await?;
        if target.session.link_target.is_some() {
            let link_path = store.symlink_path(&target.session.id);
            let link_msg = format!("[outrig] removed {} (symlink)\n", link_path.display());
            stderr.write_all(link_msg.as_bytes()).await?;
        }
    }

    let summary = format!("[outrig] cleaned {}\n", session_count(removed));
    stderr.write_all(summary.as_bytes()).await?;
    Ok(0)
}

pub fn parse_duration(value: &str) -> std::result::Result<Duration, String> {
    let raw = value.trim();
    if raw.len() < 2 {
        return Err("expected a duration like 30d, 12h, 45m, or 10s".to_string());
    }

    let (amount, unit) = raw.split_at(raw.len() - 1);
    if amount.is_empty() || !amount.bytes().all(|b| b.is_ascii_digit()) {
        return Err("duration amount must be a positive integer".to_string());
    }

    let amount: u64 = amount
        .parse()
        .map_err(|_| "duration amount is too large".to_string())?;
    if amount == 0 {
        return Err("duration amount must be greater than zero".to_string());
    }

    let unit_secs = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 60 * 60,
        "d" => DAY,
        _ => return Err("duration unit must be one of s, m, h, or d".to_string()),
    };

    let secs = amount
        .checked_mul(unit_secs)
        .ok_or_else(|| "duration is too large".to_string())?;
    Ok(Duration::from_secs(secs))
}

struct CleanTarget {
    session: Session,
    dir: PathBuf,
}

fn older_than(session: &Session, cutoff: Duration, now: SystemTime) -> bool {
    let age_basis = session.ended_at.unwrap_or(session.started_at);
    now.duration_since(age_basis)
        .map(|age| age >= cutoff)
        .unwrap_or(false)
}

async fn write_skipped_running<E>(stderr: &mut E, skipped: &[Session]) -> Result<()>
where
    E: AsyncWrite + Unpin,
{
    if skipped.is_empty() {
        return Ok(());
    }

    stderr
        .write_all(b"[outrig] skipped running sessions:\n")
        .await?;
    for session in skipped {
        let line = format!("  {}  {}\n", session.id, session.container_name);
        stderr.write_all(line.as_bytes()).await?;
    }
    Ok(())
}

async fn write_preview<E>(stderr: &mut E, targets: &[CleanTarget], retention: &str) -> Result<()>
where
    E: AsyncWrite + Unpin,
{
    let header = format!(
        "[outrig] will remove {} older than {retention}:\n",
        session_count(targets.len())
    );
    stderr.write_all(header.as_bytes()).await?;
    for target in targets {
        let (label, timestamp) = match target.session.ended_at {
            Some(t) => ("ended", t),
            None => ("started", target.session.started_at),
        };
        let line = format!(
            "  {}  {} {}  {}\n",
            target.session.id,
            label,
            session::format_started_at(timestamp),
            target.dir.display()
        );
        stderr.write_all(line.as_bytes()).await?;
    }
    Ok(())
}

async fn confirm<E, R>(stderr: &mut E, mut stdin: R, count: usize) -> Result<bool>
where
    E: AsyncWrite + Unpin,
    R: AsyncBufRead + Unpin,
{
    let prompt = format!("Clean {}? [y/N]: ", session_count(count));
    stderr.write_all(prompt.as_bytes()).await?;
    stderr.flush().await?;
    let mut line = String::new();
    stdin.read_line(&mut line).await?;
    let answer = line.trim().to_ascii_lowercase();
    Ok(answer == "y" || answer == "yes")
}

fn session_count(count: usize) -> String {
    if count == 1 {
        "1 session".to_string()
    } else {
        format!("{count} sessions")
    }
}

fn format_retention(duration: Duration) -> String {
    let secs = duration.as_secs();
    if secs.is_multiple_of(DAY) {
        format!("{}d", secs / DAY)
    } else if secs.is_multiple_of(60 * 60) {
        format!("{}h", secs / (60 * 60))
    } else if secs.is_multiple_of(60) {
        format!("{}m", secs / 60)
    } else {
        format!("{secs}s")
    }
}

async fn podman_is_running(name: String) -> Result<bool> {
    Ok(Container::is_running(&name).await?)
}
