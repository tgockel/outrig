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
use crate::session::{self, SessionStore};
use outrig::error::IoPathExt;

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
    global_override: Option<&Path>,
) -> Result<i32> {
    let logs_dir = resolve_logs_dir(args, session_root_flag, global_override)?;
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
            let pos = cat_file(stdout, &path).await?;
            if follow {
                follow_file(stdout, &path, pos, logs_dir.parent()).await?;
            }
            Ok(0)
        }
    }
}

fn resolve_logs_dir(
    args: &LogsArgs,
    session_root_flag: Option<&Path>,
    global_override: Option<&Path>,
) -> Result<PathBuf> {
    if let Some(dir) = args.session_dir.as_deref() {
        return Ok(dir.join(session::LOGS_DIR));
    }
    let Some(query) = args.session.as_deref() else {
        return Err(OutrigError::Configuration(
            "outrig logs requires either a session id or --session-dir".to_string(),
        )
        .into());
    };
    let root = session::resolve_session_root_for_cli(session_root_flag, global_override)?;
    let store = SessionStore::new(root);
    let (dir, _) = super::resolve_session_arg(&store, query)?;
    Ok(dir.join(session::LOGS_DIR))
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
        Err(e) => {
            return Err(e)
                .path_ctx("read directory", logs_dir)
                .map_err(Into::into);
        }
    };
    // A subagent launched by another subagent writes its transcript into a
    // `subagent-<parent>/` directory, so the listing descends and names each
    // file by its path under `logs_dir`.
    let mut subdirs = Vec::new();
    loop {
        while let Some(ent) = rd.next_entry().await? {
            let meta = ent.metadata().await?;
            let path = ent.path();
            if meta.is_dir() {
                subdirs.push(path);
            } else if meta.is_file() {
                let raw = path.strip_prefix(logs_dir).unwrap_or(&path).to_string_lossy();
                let display = raw.strip_suffix(LOG_SUFFIX).unwrap_or(&raw).to_string();
                entries.push((display, meta.len()));
            }
        }
        let Some(dir) = subdirs.pop() else {
            break;
        };
        rd = tokio::fs::read_dir(&dir)
            .await
            .path_ctx("read directory", &dir)?;
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
        Err(e) => return Err(e).path_ctx("open", path).map_err(Into::into),
    };
    let n = tokio::io::copy(&mut file, stdout).await?;
    stdout.flush().await?;
    Ok(n)
}

/// Polling tail. Caller has already cat'd the file once via [`cat_file`],
/// whose return value seeds `pos`: re-stating the file here instead, as this
/// used to, reads whatever landed between that dump finishing and this call
/// starting into `pos` itself, so the delta is never read as "new" and is
/// lost rather than caught up on (#383).
///
/// Loop: wait out the poll tick (or `ctrl_c`, which ends it), sample whether
/// `session_dir`'s record already shows the session finished, then stat --
/// if size grew read+write the delta, if size shrank reopen from byte 0
/// (file was truncated/rotated) -- and only *after* that read, return if the
/// sample said so. Sampling before the read rather than after matters:
/// teardown stops the MCP server and lets its log settle before it
/// finalizes the record (see `session_setup::teardown`), so a sample taken
/// before this tick's read is guaranteed to have every byte the writer will
/// ever produce already on disk for that same read to catch up on; sampling
/// after the read instead would let a write landing between the read and
/// the sample go unread forever, mistaking "stopped, as of a moment ago" for
/// "stopped, and this tick already read everything there is." When
/// `session_dir` is `None` or its record is unreadable or not yet finalized
/// -- a bare log directory with no `session.json`, as the tests here use,
/// included -- the loop only ends on `ctrl_c`, as it always did.
async fn follow_file<W: AsyncWrite + Unpin>(
    stdout: &mut W,
    path: &Path,
    mut pos: u64,
    session_dir: Option<&Path>,
) -> Result<()> {
    let mut buf = [0u8; 8192];
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => return Ok(()),
            _ = tokio::time::sleep(FOLLOW_POLL) => {}
        }
        // Sampled before this tick's stat+read, not after: teardown stops the
        // MCP server and settles its log *before* it finalizes the record, so
        // a `true` seen here means every byte the writer will ever produce is
        // already on disk, and the read below is guaranteed to catch up to
        // all of it. Sampling the other way -- after the read -- leaves a
        // window where a write lands between that read and this check, and
        // the check would then see `true` and return having never read it;
        // the record stays finalized once set, so a stale `false` here costs
        // at most one more poll tick before the next iteration notices.
        let ended = session_dir.is_some_and(session_ended);
        let len = match tokio::fs::metadata(path).await {
            Ok(m) => m.len(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e).path_ctx("stat", path).map_err(Into::into),
        };
        if len < pos {
            // Truncation/rotation: re-read from the start.
            pos = 0;
        }
        if len > pos {
            let mut file = tokio::fs::File::open(path).await.path_ctx("open", path)?;
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
        if ended {
            return Ok(());
        }
    }
}

/// Whether `dir`'s `session.json` records the session as over. `false` for
/// anything that isn't a readable, finalized record -- a directory with no
/// record at all, one mid-write, or one whose session is still running --
/// so a follow never stops on a record it cannot confirm is done.
fn session_ended(dir: &Path) -> bool {
    session::SessionStore::new(PathBuf::new())
        .get_by_path(dir)
        .is_ok_and(|s| s.ended_at.is_some())
}

#[cfg(test)]
mod tests {
    use super::{cat_file, format_size};
    use std::io::Write as _;

    #[test]
    fn size_formatting() {
        assert_eq!(format_size(0), "0 B");
        assert_eq!(format_size(512), "512 B");
        assert_eq!(format_size(1228), "1.2 KiB");
        assert_eq!(format_size(3 * 1024 * 1024 + 400 * 1024), "3.4 MiB");
    }

    /// `cat_file`'s return value is the byte count it actually wrote out, so
    /// a caller that seeds `follow_file`'s `pos` from it (rather than
    /// re-`stat`ing afterward) never misses whatever a writer appends between
    /// the dump finishing and the first poll -- the seam `follow_file` used
    /// to lose (#383): a re-`stat` taken after such a write landed would seed
    /// `pos` past it.
    #[tokio::test]
    async fn cat_file_returns_the_byte_count_it_wrote() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("fs.stderr");
        std::fs::write(&path, b"first\n").expect("write");

        let mut out = Vec::new();
        let n = cat_file(&mut out, &path).await.expect("cat_file");
        assert_eq!(n, 6, "byte count should match the file's contents");
        assert_eq!(out, b"first\n");

        // A write landing right after `cat_file` returns, before anything
        // re-`stat`s the file -- exactly the window `follow_file` used to
        // lose. Seeding `pos` from `n` above (not a fresh `stat`) is what a
        // caller needs to do to catch this on the next poll.
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .expect("open append");
        f.write_all(b"seam\n").expect("seam write");
        drop(f);

        let len = std::fs::metadata(&path).expect("stat").len();
        assert!(
            len > n,
            "the seam write must land after cat_file's count, or this test proves nothing"
        );
    }
}
