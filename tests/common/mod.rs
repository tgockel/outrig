//! Shared test helpers across integration tests. Cargo treats files in
//! `tests/` as test binaries; subdirectories with `mod.rs` are
//! conventional shared modules (no phantom `common` test binary).

use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use tokio::io::{AsyncWriteExt, BufReader, DuplexStream, duplex};

use outrig::init::prompt::TerminalPrompt;
use outrig::session::{Session, SessionId};

/// `TerminalPrompt` wired to in-memory `tokio::io::duplex` streams so
/// tests can replay scripted stdin and inspect stderr.
#[allow(dead_code)]
pub type ScriptedPrompt = TerminalPrompt<BufReader<DuplexStream>, DuplexStream>;

/// Build a `TerminalPrompt` whose stdin replays `script` (closed
/// immediately afterwards so EOF surfaces correctly) and whose stderr
/// is captured into the returned `DuplexStream`.
#[allow(dead_code)]
pub async fn scripted_prompt(script: &[u8]) -> (ScriptedPrompt, DuplexStream) {
    const BUF: usize = 4096;
    let (mut stdin_w, stdin_r) = duplex(BUF);
    let (stderr_w, stderr_r) = duplex(BUF);
    stdin_w.write_all(script).await.unwrap();
    drop(stdin_w);
    (
        TerminalPrompt::new(BufReader::new(stdin_r), stderr_w),
        stderr_r,
    )
}

/// Build a `Session` with sane defaults suitable for both the in-flight
/// (`ended_at: None`) and finished (`ended_at: Some`) cases. Callers
/// override fields as needed.
#[allow(dead_code)]
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
