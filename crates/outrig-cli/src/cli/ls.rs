//! `outrig ls` -- list sessions newest-first under the session root.
//!
//! Output discipline: the table goes to stdout (it's the scriptable thing); a
//! "no sessions" notice goes to stderr, as does one `skipping <sid>: <reason>`
//! line per unreadable record -- listing the rest still beats failing outright.
//! Symlinked entries (created via `outrig run --session-dir`) get a trailing
//! `-> <target>` so the user can see where the real bytes live.
//!
//! A `--session-dir` link whose directory is gone is reported the same way.
//!
//! A `session.json` that can't be read or parsed is skipped and reported rather
//! than failed on, including when that leaves nothing to list -- so an
//! all-skipped root still exits 0. Faults reaching an entry in the first place
//! (reading the root, stat'ing or resolving an entry under it) are still errors.

use std::fmt::Write as _;
use std::path::Path;
use std::time::SystemTime;

use clap::Parser;
use tokio::io::{AsyncWrite, AsyncWriteExt};
use unicode_width::UnicodeWidthStr;

use crate::error::Result;
use crate::session::{self, Session, SessionStore};

#[derive(Debug, Parser)]
pub struct LsArgs {}

pub async fn execute(
    _args: &LsArgs,
    session_root_flag: Option<&Path>,
    global_override: Option<&Path>,
) -> Result<i32> {
    let root = session::resolve_session_root_for_cli(session_root_flag, global_override)?;
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
    let listing = store.list()?;
    for skipped in &listing.skipped {
        let line = format!("[outrig] skipping {}: {}\n", skipped.entry, skipped.reason());
        stderr.write_all(line.as_bytes()).await?;
    }
    for link in &listing.dangling {
        let line = format!(
            "[outrig] skipping {}: session directory {} no longer exists\n",
            link.entry,
            link.target.display()
        );
        stderr.write_all(line.as_bytes()).await?;
    }
    if listing.sessions.is_empty() {
        // An empty root and a root where nothing parsed are different facts;
        // saying "no sessions" for the latter hides the records entirely.
        let unlisted = listing.skipped.len() + listing.dangling.len();
        let notice = if unlisted == 0 {
            "[outrig] no sessions\n".to_string()
        } else {
            format!(
                "[outrig] no readable sessions ({unlisted} skipped)\n"
            )
        };
        stderr.write_all(notice.as_bytes()).await?;
        return Ok(0);
    }
    let now = SystemTime::now();
    let table = render_table(&listing.sessions, now);
    stdout.write_all(table.as_bytes()).await?;
    Ok(0)
}

/// Hand-rolled column padding (per task notes: don't pull in a table crate).
/// Columns: `ID`, `STARTED`, `DURATION`, `IMAGE`, `EXIT`. The `EXIT`
/// column trails with `-> <target>` for symlinked entries.
///
/// Every width here is a count of terminal display columns, which is what the
/// reader's alignment is in. See [`padded`] for why the format specifier's own
/// padding can't be used for it.
fn render_table(sessions: &[Session], now: SystemTime) -> String {
    let rows: Vec<Row> = sessions.iter().map(|s| Row::from_session(s, now)).collect();

    let id_w = max_width("ID", rows.iter().map(|r| r.id.as_str()));
    let started_w = max_width("STARTED", rows.iter().map(|r| r.started.as_str()));
    let duration_w = max_width("DURATION", rows.iter().map(|r| r.duration.as_str()));
    let image_w = max_width("IMAGE", rows.iter().map(|r| r.image.as_str()));

    let mut out = String::new();
    let _ = writeln!(
        out,
        "{}  {}  {}  {}  EXIT",
        padded("ID", id_w),
        padded("STARTED", started_w),
        padded("DURATION", duration_w),
        padded("IMAGE", image_w),
    );
    for r in &rows {
        let exit_cell = match &r.link_target {
            Some(t) => format!("{}  -> {}", padded(&r.exit, EXIT_W), t),
            None => r.exit.clone(),
        };
        let _ = writeln!(
            out,
            "{}  {}  {}  {}  {}",
            padded(&r.id, id_w),
            padded(&r.started, started_w),
            padded(&r.duration, duration_w),
            padded(&r.image, image_w),
            exit_cell,
        );
    }
    out
}

/// What the `EXIT` cell of a symlinked row is padded to before its
/// `-> <target>` suffix, so the targets line up under each other. Exit codes
/// are at most three digits (`130`, `143`) and `-` for a session still
/// running.
const EXIT_W: usize = 3;

struct Row {
    id: String,
    started: String,
    duration: String,
    image: String,
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
        let image = s.image_config_name.clone().unwrap_or_else(|| "-".to_string());
        let exit = match s.exit_code {
            Some(c) => c.to_string(),
            None => "-".to_string(),
        };
        let link_target = s.link_target.as_ref().map(|p| p.display().to_string());
        Self {
            id: s.id.as_str().to_string(),
            started,
            duration,
            image,
            exit,
            link_target,
        }
    }
}

/// The widest of `header` and `values`, in terminal display columns.
///
/// Not bytes: an `IMAGE` cell is an image-config name, and the key of an
/// `image-name` block is only a label -- it is held to no grammar, so it can
/// be any UTF-8 the user wrote. Counting its bytes padded every row of the
/// table by that name's continuation bytes (#371).
fn max_width<'a, I: IntoIterator<Item = &'a str>>(header: &str, values: I) -> usize {
    values
        .into_iter()
        .map(UnicodeWidthStr::width)
        .fold(header.width(), usize::max)
}

/// `cell`, then the spaces that carry the next column to `w` display columns.
///
/// `{:<w$}` is not this and cannot be made into it: it pads to `w` *`char`s*,
/// so a cell holding a double-width character -- any CJK name, an emoji -- is
/// left one space too wide for each one however the width was counted. Doing
/// the padding here keeps one answer for "how wide is this text" across the
/// measuring and the writing, which is what [`UnicodeWidthStr::width`] gives.
///
/// A cell wider than `w` is written whole and pushes its row out of line,
/// which is the honest rendering: the alternative is to cut a name in half.
fn padded(cell: &str, w: usize) -> String {
    let pad = w.saturating_sub(cell.width());
    let mut out = String::with_capacity(cell.len() + pad);
    out.push_str(cell);
    out.extend(std::iter::repeat_n(' ', pad));
    out
}
