//! Shared test helpers across integration tests. Cargo treats files in
//! `tests/` as test binaries; subdirectories with `mod.rs` are
//! conventional shared modules (no phantom `common` test binary).

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream, duplex};

use outrig::error::OutrigError;
use outrig_cli::error::Result;
use outrig_cli::hf::{HfFile, HfTreeFetcher};
use outrig_cli::init::prompt::TerminalPrompt;
use outrig_cli::session::{Session, SessionId};

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

/// `HfTreeFetcher` stub for scripted prompt tests. Returns
/// `Ok(files.clone())` on every call unless `error_message` is set, in
/// which case it returns a configuration error so the prompt flow takes
/// the free-form fallback path.
#[allow(dead_code)]
pub struct StubHfTreeFetcher {
    pub files: Vec<HfFile>,
    pub error_message: Option<String>,
}

impl StubHfTreeFetcher {
    /// Stub that always returns the given filenames (no size info). Use
    /// `[] ` for tests that shouldn't reach the HF path; use
    /// `["only.gguf"]` for the auto-pick path; use multiple entries for
    /// the picker path.
    #[allow(dead_code)]
    pub fn with_files<I: IntoIterator<Item = S>, S: Into<String>>(files: I) -> Self {
        Self {
            files: files
                .into_iter()
                .map(|s| HfFile {
                    path: s.into(),
                    size: None,
                })
                .collect(),
            error_message: None,
        }
    }

    /// Stub that always returns the given files including sizes -- exercises
    /// the picker rendering path with human-readable sizes.
    #[allow(dead_code)]
    pub fn with_sized_files<I: IntoIterator<Item = (S, u64)>, S: Into<String>>(files: I) -> Self {
        Self {
            files: files
                .into_iter()
                .map(|(s, sz)| HfFile {
                    path: s.into(),
                    size: Some(sz),
                })
                .collect(),
            error_message: None,
        }
    }

    /// Stub that always errors with `msg`. Use for tests that exercise
    /// the network-fallback path through `MODEL_FILE_FIELD`.
    #[allow(dead_code)]
    pub fn errors_with(msg: &str) -> Self {
        Self {
            files: Vec::new(),
            error_message: Some(msg.to_string()),
        }
    }
}

impl HfTreeFetcher for StubHfTreeFetcher {
    async fn list_files(
        &mut self,
        _model_id: &str,
        _revision: Option<&str>,
    ) -> Result<Vec<HfFile>> {
        if let Some(msg) = &self.error_message {
            return Err(OutrigError::Configuration(msg.clone()).into());
        }
        Ok(self.files.clone())
    }
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
        image_config_name: "coding".to_string(),
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
