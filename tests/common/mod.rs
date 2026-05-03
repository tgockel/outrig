//! Shared test helpers across integration tests. Cargo treats files in
//! `tests/` as test binaries; subdirectories with `mod.rs` are
//! conventional shared modules (no phantom `common` test binary).

use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use outrig::session::{Session, SessionId};

/// Build a `Session` with sane defaults suitable for both the in-flight
/// (`ended_at: None`) and finished (`ended_at: Some`) cases. Callers
/// override fields as needed.
pub fn sample_session(id: &SessionId) -> Session {
    Session {
        id: id.clone(),
        started_at: SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000),
        ended_at: None,
        container_name: format!("outrig-{}", id.as_str()),
        image_tag: "outrig/test:abc123".to_string(),
        container_config_name: "coding".to_string(),
        agent_name: "default".to_string(),
        working_dir: PathBuf::from("/some/repo"),
        session_dir: PathBuf::new(),
        exit_code: None,
        link_target: None,
    }
}
