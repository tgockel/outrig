//! CLI subcommand entry points. Each subcommand owns its arg struct and its
//! `execute` function; `bin/outrig.rs` stays a thin dispatch table.

pub mod app;
pub mod build;
pub mod build_containers;
pub mod clean;
pub mod design_prompt;
pub mod discard;
pub mod engine;
pub mod env_arg;
pub mod logs;
pub mod ls;
pub mod mcp;
pub mod mcp_self;
pub mod run;
pub mod session_setup;
pub mod signals;
pub mod volume_arg;
pub mod watcher;

use std::path::{Path, PathBuf};

use crate::error::{OutrigError, Result};
use crate::session::{DanglingLink, Session, SessionId, SessionStore, SkippedSession};

/// What a `<session>` argument names: a readable session, a `--session-dir`
/// link whose directory is gone, or an entry whose record cannot be read.
pub enum Found {
    Session(PathBuf, Box<Session>),
    Dangling(DanglingLink),
    Unreadable(Box<SkippedSession>),
}

/// Resolve the user's `<session>` argument (which may be a substring) to a
/// concrete session. CLI policy, not store policy: substring matching is an
/// ergonomic affordance for the user typing `outrig logs 1419 fs`, but the
/// store's contract stays "exact id or path."
///
/// Tries an exact `get_by_id` first (so the common case is one stat). On miss,
/// falls back to a `list()` substring match; an entry named exactly by the
/// query wins over longer names containing it, as a readable one does.
/// Zero matches -> not-found error; multiple -> [`OutrigError::Configuration`]
/// with the candidate ids one per line. A match that is a dangling link or an
/// unreadable record comes back as [`Found::Dangling`] or [`Found::Unreadable`],
/// for the commands that can act on one.
pub fn resolve_found(store: &SessionStore, query: &str) -> Result<Found> {
    let exact = SessionId(query.to_string());
    if let Ok((dir, session)) = store.get_by_id(&exact) {
        return Ok(Found::Session(dir, Box::new(session)));
    }
    let mut listing = store.list()?;
    if let Some(at) = listing.skipped.iter().position(|s| s.entry == query) {
        return Ok(Found::Unreadable(Box::new(listing.skipped.swap_remove(at))));
    }
    let mut matches: Vec<Found> = listing
        .sessions
        .into_iter()
        .filter(|s| s.id.as_str().contains(query))
        .map(|s| Found::Session(s.session_dir.clone(), Box::new(s)))
        .chain(
            listing
                .dangling
                .into_iter()
                .filter(|d| d.entry.contains(query))
                .map(Found::Dangling),
        )
        .chain(
            listing
                .skipped
                .into_iter()
                .filter(|s| s.entry.contains(query))
                .map(|s| Found::Unreadable(Box::new(s))),
        )
        .collect();
    match matches.len() {
        0 => Err(OutrigError::Configuration(format!("no session matching {query:?}")).into()),
        1 => Ok(matches.pop().expect("len == 1")),
        _ => {
            let mut msg = format!("ambiguous session {query:?}; candidates:");
            for m in &matches {
                msg.push_str("\n  ");
                msg.push_str(match m {
                    Found::Session(_, s) => s.id.as_str(),
                    Found::Dangling(d) => &d.entry,
                    Found::Unreadable(s) => &s.entry,
                });
            }
            Err(OutrigError::Configuration(msg).into())
        }
    }
}

/// [`resolve_found`] for the commands that need a session's record. A dangling
/// link or an unreadable record has none, so it is an error that says what is
/// left and what removes it.
pub fn resolve_session_arg(store: &SessionStore, query: &str) -> Result<(PathBuf, Session)> {
    match resolve_found(store, query)? {
        Found::Session(dir, session) => Ok((dir, *session)),
        Found::Dangling(d) => Err(OutrigError::Configuration(format!(
            "session {} has no record: its directory {} no longer exists; \
             `outrig discard {}` removes the link",
            d.entry,
            d.target.display(),
            d.entry
        ))
        .into()),
        Found::Unreadable(s) => Err(s.error),
    }
}

/// Unlink a dangling link and say so: whether it went, and the line for
/// `discard` and `clean` to print. A link whose directory came back, or
/// that is no longer the same link, stays.
pub fn unlink_dangling_report(store: &SessionStore, link: &DanglingLink) -> Result<(bool, String)> {
    let path = store.symlink_path(&link.id());
    Ok(if store.unlink_dangling(link)? {
        let msg = format!(
            "[outrig] removed {} (symlink to missing {})\n",
            path.display(),
            link.target.display()
        );
        (true, msg)
    } else {
        let msg = format!(
            "[outrig] kept {}: it changed after it was selected\n",
            path.display()
        );
        (false, msg)
    })
}

/// What `discard` and `clean` print for one removed session. `left` is what
/// its directory still holds, as `SessionStore::remove_by_id` returns it:
/// empty when the directory went with the record, otherwise the files outrig
/// did not write, which stay -- and so does the directory.
pub fn removal_report(dir: &Path, left: &[String]) -> String {
    if left.is_empty() {
        return format!("[outrig] removed {}\n", dir.display());
    }
    let (shown, rest) = left.split_at(left.len().min(5));
    let mut names = shown.join(", ");
    if !rest.is_empty() {
        names += &format!(", and {} more", rest.len());
    }
    format!(
        "[outrig] removed the session record from {dir}\n\
         [outrig] kept {dir}, which also holds {names}\n",
        dir = dir.display()
    )
}
