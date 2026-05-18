//! Shared test helpers across library integration tests. Cargo treats
//! files in `tests/` as test binaries; subdirectories with `mod.rs` are
//! conventional shared modules (no phantom `common` test binary).

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use tokio::io::{AsyncBufReadExt, BufReader};

use outrig::session::{Session, SessionId};

/// Install a best-effort tracing subscriber for integration tests that
/// surface process output under `--nocapture`.
#[allow(dead_code)]
pub fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .try_init();
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
        agent_name: Some("default".to_string()),
        working_dir: PathBuf::from("/some/repo"),
        session_dir: PathBuf::new(),
        exit_code: None,
        link_target: None,
    }
}

/// Drain `reader` line-by-line, mirroring each line to the test runner's
/// stderr (so a hang dumps everything-so-far) and into the shared `sink`
/// buffer for later assertions. `label` distinguishes which stream a line
/// came from in the mirrored output.
#[allow(dead_code)]
pub async fn stream_lines<R>(reader: R, sink: Arc<Mutex<String>>, label: &'static str)
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    let mut reader = BufReader::new(reader);
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line).await {
            Ok(0) => break,
            Ok(_) => {
                eprintln!("[child {label}] {}", line.trim_end_matches('\n'));
                sink.lock().unwrap().push_str(&line);
            }
            Err(_) => break,
        }
    }
}
