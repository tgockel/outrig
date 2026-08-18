//! CLI subcommand entry points. Each subcommand owns its arg struct and its
//! `execute` function; `bin/outrig.rs` stays a thin dispatch table.

pub mod app;
pub mod build;
pub mod clean;
pub mod design_prompt;
pub mod discard;
pub mod env_arg;
pub mod logs;
pub mod ls;
pub mod mcp;
pub mod mcp_self;
pub mod run;
pub mod session_setup;
pub mod volume_arg;
pub mod watcher;

use std::path::PathBuf;

use crate::error::{OutrigError, Result};
use crate::session::{Session, SessionId, SessionStore, SkippedSession};

/// Resolve the user's `<session>` argument (which may be a substring) to a
/// concrete session. CLI policy, not store policy: substring matching is an
/// ergonomic affordance for the user typing `outrig logs 1419 fs`, but the
/// store's contract stays "exact id or path."
///
/// Tries an exact `get_by_id` first (so the common case is one stat). On miss,
/// falls back to a `list()` substring match. Zero matches -> not-found error,
/// unless the query matches an entry that exists but couldn't be parsed, in
/// which case say *that* rather than claiming nothing matched; multiple ->
/// [`OutrigError::Configuration`] with the candidate ids one per line.
pub fn resolve_session_arg(store: &SessionStore, query: &str) -> Result<(PathBuf, Session)> {
    let exact = SessionId(query.to_string());
    if let Ok(out) = store.get_by_id(&exact) {
        return Ok(out);
    }
    let listing = store.list()?;
    let mut matches: Vec<Session> = listing
        .sessions
        .into_iter()
        .filter(|s| s.id.as_str().contains(query))
        .collect();
    match matches.len() {
        0 => {
            let unreadable: Vec<&SkippedSession> = listing
                .skipped
                .iter()
                .filter(|s| s.entry.contains(query))
                .collect();
            if unreadable.is_empty() {
                return Err(
                    OutrigError::Configuration(format!("no session matching {query:?}")).into(),
                );
            }
            let mut msg = format!("session {query:?} exists but could not be read:");
            for s in &unreadable {
                msg.push_str("\n  ");
                msg.push_str(&s.entry);
                msg.push_str(": ");
                msg.push_str(&s.reason);
            }
            Err(OutrigError::Configuration(msg).into())
        }
        1 => {
            let s = matches.pop().expect("len == 1");
            Ok((s.session_dir.clone(), s))
        }
        _ => {
            let mut msg = format!("ambiguous session {query:?}; candidates:");
            for s in &matches {
                msg.push_str("\n  ");
                msg.push_str(s.id.as_str());
            }
            Err(OutrigError::Configuration(msg).into())
        }
    }
}
