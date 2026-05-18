//! `outrig logs` -- print or `tail -F` an MCP server's captured stderr.
//!
//! Three modes:
//! - `outrig logs <sid>` (no server) -- list available log files with sizes.
//! - `outrig logs <sid> <server>` -- cat that server's `.stderr` file.
//! - `outrig logs <sid> <server> --follow` -- polling tail.
//!
//! `--session-dir <path>` substitutes for `<sid>` when the user already knows
//! the on-disk path (avoids the id lookup). The two are mutually exclusive
//! per the [`ArgGroup`].

use std::fmt::Write as FmtWrite;
use std::path::{Path, PathBuf};
use std::time::Duration;

use clap::{ArgGroup, Parser};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWrite, AsyncWriteExt};

use crate::error::{OutrigError, Result};
use outrig::session::{self, SessionStore};

const FOLLOW_POLL: Duration = Duration::from_millis(200);
/// MCP server stderr captures land at `<session>/logs/<server>.stderr` (see
/// `src/mcp.rs`). Used both to build the file name and to strip the suffix
/// for display.
const LOG_SUFFIX: &str = ".stderr";

#[derive(Debug, Parser)]
#[command(group(
    ArgGroup::new("logs_target")
        .args(["session", "session_dir"])
        .multiple(false)
        .required(false)
))]
pub struct LogsArgs {
    /// Session id (substring match if unambiguous).
    pub session: Option<String>,
    /// MCP server name (omit to list available logs).
    pub server: Option<String>,
    /// Tail the file; continue reading as new lines arrive.
    #[arg(short = 'f', long = "follow")]
    pub follow: bool,
    /// Read directly from this session dir, skipping the id lookup.
    #[arg(long = "session-dir", value_name = "PATH")]
    pub session_dir: Option<PathBuf>,
}

pub async fn execute(
    args: &LogsArgs,
    session_root_flag: Option<&Path>,
    repo_cfg_override: Option<&Path>,
    global_cfg_path: &Path,
    cwd: &Path,
) -> Result<i32> {
    let logs_dir = resolve_logs_dir(
        args,
        session_root_flag,
        repo_cfg_override,
        global_cfg_path,
        cwd,
    )?;
    let mut stdout = tokio::io::stdout();
    let mut stderr = tokio::io::stderr();
    execute_with(
        &mut stdout,
        &mut stderr,
        &logs_dir,
        args.server.as_deref(),
        args.follow,
    )
    .await
}

pub async fn execute_with<W, E>(
    stdout: &mut W,
    stderr: &mut E,
    logs_dir: &Path,
    server: Option<&str>,
    follow: bool,
) -> Result<i32>
where
    W: AsyncWrite + Unpin,
    E: AsyncWrite + Unpin,
{
    match server {
        None => {
            list_logs(stdout, stderr, logs_dir).await?;
            Ok(0)
        }
        Some(s) => {
            let path = logs_dir.join(format!("{s}{LOG_SUFFIX}"));
            cat_file(stdout, &path).await?;
            if follow {
                follow_file(stdout, &path).await?;
            }
            Ok(0)
        }
    }
}

fn resolve_logs_dir(
    args: &LogsArgs,
    session_root_flag: Option<&Path>,
    repo_cfg_override: Option<&Path>,
    global_cfg_path: &Path,
    cwd: &Path,
) -> Result<PathBuf> {
    if let Some(dir) = args.session_dir.as_deref() {
        return Ok(dir.join("logs"));
    }
    let Some(query) = args.session.as_deref() else {
        return Err(OutrigError::Configuration(
            "outrig logs requires either a session id or --session-dir".to_string(),
        )
        .into());
    };
    let root = session::resolve_session_root_for_cli(
        session_root_flag,
        repo_cfg_override,
        global_cfg_path,
        cwd,
    )?;
    let store = SessionStore::new(root);
    let (dir, _) = super::resolve_session_arg(&store, query)?;
    Ok(dir.join("logs"))
}

async fn list_logs<W, E>(stdout: &mut W, stderr: &mut E, logs_dir: &Path) -> Result<()>
where
    W: AsyncWrite + Unpin,
    E: AsyncWrite + Unpin,
{
    let mut entries: Vec<(String, u64)> = Vec::new();
    let mut rd = match tokio::fs::read_dir(logs_dir).await {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            stderr
                .write_all(b"[outrig] no logs directory for this session\n")
                .await?;
            return Ok(());
        }
        Err(e) => return Err(e.into()),
    };
    while let Some(ent) = rd.next_entry().await? {
        let meta = ent.metadata().await?;
        if !meta.is_file() {
            continue;
        }
        let raw = ent.file_name().to_string_lossy().into_owned();
        let display = raw.strip_suffix(LOG_SUFFIX).unwrap_or(&raw).to_string();
        entries.push((display, meta.len()));
    }
    entries.sort();

    let header = format!("[outrig] logs in {}:\n", logs_dir.display());
    stderr.write_all(header.as_bytes()).await?;

    if entries.is_empty() {
        stderr.write_all(b"  (none)\n").await?;
        return Ok(());
    }

    let pad = entries.iter().map(|(n, _)| n.len()).max().unwrap_or(0);
    let mut out = String::new();
    for (name, size) in &entries {
        let _ = writeln!(out, "  {:<pad$}  ({})", name, format_size(*size), pad = pad);
    }
    stdout.write_all(out.as_bytes()).await?;
    Ok(())
}

/// Sizes formatted like `1.2 KiB`, `3.4 MiB` -- one decimal, IEC prefixes.
fn format_size(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = KIB * 1024.0;
    const GIB: f64 = MIB * 1024.0;
    let b = bytes as f64;
    if b < KIB {
        format!("{bytes} B")
    } else if b < MIB {
        format!("{:.1} KiB", b / KIB)
    } else if b < GIB {
        format!("{:.1} MiB", b / MIB)
    } else {
        format!("{:.1} GiB", b / GIB)
    }
}

async fn cat_file<W: AsyncWrite + Unpin>(stdout: &mut W, path: &Path) -> Result<u64> {
    let mut file = match tokio::fs::File::open(path).await {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(OutrigError::Configuration(format!(
                "log file {} does not exist",
                path.display()
            ))
            .into());
        }
        Err(e) => return Err(e.into()),
    };
    let n = tokio::io::copy(&mut file, stdout).await?;
    stdout.flush().await?;
    Ok(n)
}

/// Polling tail. Caller has already cat'd the file once via [`cat_file`].
/// Loop: stat, if size grew read+write delta, if size shrank reopen from
/// byte 0 (file was truncated/rotated). Terminates on `ctrl_c`.
async fn follow_file<W: AsyncWrite + Unpin>(stdout: &mut W, path: &Path) -> Result<()> {
    let mut pos: u64 = tokio::fs::metadata(path).await?.len();
    let mut buf = [0u8; 8192];
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => return Ok(()),
            _ = tokio::time::sleep(FOLLOW_POLL) => {}
        }
        let len = match tokio::fs::metadata(path).await {
            Ok(m) => m.len(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e.into()),
        };
        if len < pos {
            // Truncation/rotation: re-read from the start.
            pos = 0;
        }
        if len > pos {
            let mut file = tokio::fs::File::open(path).await?;
            file.seek(std::io::SeekFrom::Start(pos)).await?;
            loop {
                let n = file.read(&mut buf).await?;
                if n == 0 {
                    break;
                }
                stdout.write_all(&buf[..n]).await?;
                pos += n as u64;
            }
            stdout.flush().await?;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::format_size;

    #[test]
    fn size_formatting() {
        assert_eq!(format_size(0), "0 B");
        assert_eq!(format_size(512), "512 B");
        assert_eq!(format_size(1228), "1.2 KiB");
        assert_eq!(format_size(3 * 1024 * 1024 + 400 * 1024), "3.4 MiB");
    }
}
