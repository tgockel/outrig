//! CLI subcommand entry points. Each subcommand owns its arg struct and its
//! `execute` function; `bin/outrig.rs` stays a thin dispatch table.

pub mod build;
pub mod design_prompt;
pub mod discard;
pub mod env_arg;
pub mod logs;
pub mod ls;
pub mod mcp;
pub mod mcp_self;
pub mod run;
pub mod session_setup;

use std::path::PathBuf;

use crate::error::{OutrigError, Result};
use crate::session::{Session, SessionId, SessionStore};

/// Resolve the user's `<session>` argument (which may be a substring) to a
/// concrete session. CLI policy, not store policy: substring matching is an
/// ergonomic affordance for the user typing `outrig logs 1419 fs`, but the
/// store's contract stays "exact id or path."
///
/// Tries an exact `get_by_id` first (so the common case is one stat). On miss,
/// falls back to a `list()` substring match. Zero matches -> not-found error;
/// multiple -> [`OutrigError::Configuration`] with the candidate ids one per
/// line.
pub fn resolve_session_arg(store: &SessionStore, query: &str) -> Result<(PathBuf, Session)> {
    let exact = SessionId(query.to_string());
    if let Ok(out) = store.get_by_id(&exact) {
        return Ok(out);
    }
    let mut matches: Vec<Session> = store
        .list()?
        .into_iter()
        .filter(|s| s.id.as_str().contains(query))
        .collect();
    match matches.len() {
        0 => Err(OutrigError::Configuration(format!(
            "no session matching {query:?}"
        ))),
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
            Err(OutrigError::Configuration(msg))
        }
    }
}
