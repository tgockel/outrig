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
pub(crate) mod log_divert;
pub mod logs;
pub mod ls;
pub mod mcp;
pub mod mcp_self;
pub mod program;
pub(crate) mod report;
pub mod run;
pub mod run_new;
pub mod run_new_only_keys;
pub mod session_setup;
pub mod signals;
pub mod volume_arg;
pub mod watcher;

use std::io::Write;
use std::path::PathBuf;

use crate::error::{OutrigError, Result};
use crate::session::{DanglingLink, Session, SessionId, SessionStore, SkippedSession};

/// What a `<session>` argument names: a readable session, a `--session-dir`
/// link whose directory is gone, or an entry whose record cannot be read.
pub enum Found {
    Session(PathBuf, Box<Session>),
    Dangling(DanglingLink),
    Unreadable(Box<SkippedSession>),
}

/// Write `text` to stderr, and let it go if the write fails.
///
/// For what a command says as it ends, where `eprint!` is wrong: it panics when
/// stderr's reader has gone, so what comes after it never runs and the process
/// exits with a panic's 101 instead of the status the command decided. A
/// diagnostic nobody can read is no reason for either.
pub(crate) fn eprint_best_effort(text: &str) {
    let _ = std::io::stderr().write_all(text.as_bytes());
}

/// How long [`eprint_bounded`] waits for the write to `stderr`.
const FINAL_ERROR_GRACE: std::time::Duration = std::time::Duration::from_secs(2);

/// [`eprint_best_effort`] for the last thing a process says, which must not
/// keep it from exiting: a stderr whose reader has stopped, with its pipe
/// full, holds a write for good. The write is made on a thread of its own and
/// waited for only so long; the process ends with the write left in flight.
pub(crate) fn eprint_bounded(text: &str) {
    write_bounded(std::io::stderr(), text, FINAL_ERROR_GRACE);
}

fn write_bounded(mut out: impl Write + Send + 'static, text: &str, within: std::time::Duration) {
    let text = text.to_string();
    let (done, written) = std::sync::mpsc::channel();
    let spawned = std::thread::Builder::new()
        .name("final-error".to_string())
        .spawn(move || {
            let _ = out.write_all(text.as_bytes());
            let _ = done.send(());
        });
    if spawned.is_ok() {
        let _ = written.recv_timeout(within);
    }
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


#[cfg(test)]
mod bounded_tests {
    use std::io::Write;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    use super::write_bounded;

    /// A writer whose write waits until it is let go, as a write to a full
    /// pipe nobody reads does.
    struct Stuck(mpsc::Receiver<()>);

    impl Write for Stuck {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            let _ = self.0.recv();
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// The last thing a process says does not keep it from exiting when the
    /// write never finishes (#644).
    #[test]
    fn a_write_that_never_finishes_is_given_up_on() {
        let (_hold, stuck) = mpsc::channel();
        let started = Instant::now();
        write_bounded(Stuck(stuck), "error: x\n", Duration::from_millis(100));
        assert!(started.elapsed() < Duration::from_secs(10));
    }
}
