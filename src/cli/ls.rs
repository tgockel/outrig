//! `outrig ls` -- list sessions newest-first under the session root.
//!
//! Output discipline: the table goes to stdout (it's the scriptable thing); a
//! "no sessions" notice goes to stderr. Symlinked entries (created via
//! `outrig run --session-dir`) get a trailing `-> <target>` so the user can
//! see where the real bytes live.

use std::fmt::Write as _;
use std::path::Path;
use std::time::SystemTime;

use clap::Parser;
use tokio::io::{AsyncWrite, AsyncWriteExt};

use crate::error::Result;
use crate::session::{self, Session, SessionStore};

#[derive(Debug, Parser)]
pub struct LsArgs {}

pub async fn execute(
    _args: &LsArgs,
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
    let mut stdout = tokio::io::stdout();
    let mut stderr = tokio::io::stderr();
    execute_with(&mut stdout, &mut stderr, &store).await
}

pub async fn execute_with<W, E>(stdout: &mut W, stderr: &mut E, store: &SessionStore) -> Result<i32>
where
    W: AsyncWrite + Unpin,
    E: AsyncWrite + Unpin,
{
    let sessions = store.list()?;
    if sessions.is_empty() {
        stderr.write_all(b"[outrig] no sessions\n").await?;
        return Ok(0);
    }
    let now = SystemTime::now();
    let table = render_table(&sessions, now);
    stdout.write_all(table.as_bytes()).await?;
    Ok(0)
}

/// Hand-rolled column padding (per task notes: don't pull in a table crate).
/// Columns: `ID`, `STARTED`, `DURATION`, `CONTAINER`, `EXIT`. The `EXIT`
/// column trails with `-> <target>` for symlinked entries.
fn render_table(sessions: &[Session], now: SystemTime) -> String {
    let rows: Vec<Row> = sessions.iter().map(|s| Row::from_session(s, now)).collect();

    let id_w = max_width("ID", rows.iter().map(|r| r.id.as_str()));
    let started_w = max_width("STARTED", rows.iter().map(|r| r.started.as_str()));
    let duration_w = max_width("DURATION", rows.iter().map(|r| r.duration.as_str()));
    let container_w = max_width("CONTAINER", rows.iter().map(|r| r.container.as_str()));

    let mut out = String::new();
    let _ = writeln!(
        out,
        "{:<id_w$}  {:<started_w$}  {:<duration_w$}  {:<container_w$}  EXIT",
        "ID",
        "STARTED",
        "DURATION",
        "CONTAINER",
        id_w = id_w,
        started_w = started_w,
        duration_w = duration_w,
        container_w = container_w,
    );
    for r in &rows {
        let exit_cell = match &r.link_target {
            Some(t) => format!("{:<3}  -> {}", r.exit, t),
            None => r.exit.clone(),
        };
        let _ = writeln!(
            out,
            "{:<id_w$}  {:<started_w$}  {:<duration_w$}  {:<container_w$}  {}",
            r.id,
            r.started,
            r.duration,
            r.container,
            exit_cell,
            id_w = id_w,
            started_w = started_w,
            duration_w = duration_w,
            container_w = container_w,
        );
    }
    out
}

struct Row {
    id: String,
    started: String,
    duration: String,
    container: String,
    exit: String,
    link_target: Option<String>,
}

impl Row {
    fn from_session(s: &Session, now: SystemTime) -> Self {
        let started = session::format_started_at(s.started_at);
        let end = s.ended_at.unwrap_or(now);
        let duration = end
            .duration_since(s.started_at)
            .map(session::format_duration)
            .unwrap_or_else(|_| "?".to_string());
        let container = s.container_config_name.clone();
        let exit = match s.exit_code {
            Some(c) => c.to_string(),
            None => "-".to_string(),
        };
        let link_target = s.link_target.as_ref().map(|p| p.display().to_string());
        Self {
            id: s.id.as_str().to_string(),
            started,
            duration,
            container,
            exit,
            link_target,
        }
    }
}

fn max_width<'a, I: IntoIterator<Item = &'a str>>(header: &str, values: I) -> usize {
    let mut w = header.len();
    for v in values {
        if v.len() > w {
            w = v.len();
        }
    }
    w
}
