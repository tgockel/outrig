//! Shared test helpers across integration tests. Cargo treats files in
//! `tests/` as test binaries; subdirectories with `mod.rs` are
//! conventional shared modules (no phantom `common` test binary).

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream, duplex};

use outrig::config::ImageConfig;
use outrig::error::OutrigError;
use outrig_cli::error::Result;
use outrig_cli::hf::{HfFile, HfTreeFetcher};
use outrig_cli::init::prompt::TerminalPrompt;
use outrig_cli::session::{Session, SessionId};

/// Build-source `ImageConfig` for a fixture directory that holds a
/// `Dockerfile` -- the shape nearly every `ensure_image` test wants.
///
/// Shared so this crate's gated test binaries name the shape once. Since
/// `ImageConfig` is `#[non_exhaustive]`, `tests/` cannot spell the literal at
/// all; the constructor is the whole construction path from out here.
#[allow(dead_code)]
pub fn fixture_build_config() -> ImageConfig {
    ImageConfig::from_dockerfile("Dockerfile", ".")
}

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
        sidecar_container_names: Vec::new(),
        image_tag: "outrig/test:abc123".to_string(),
        image_config_name: Some("coding".to_string()),
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

// ---- scripted HTTP mock ---------------------------------------------------
//
// A local stand-in for an LLM provider's HTTP endpoint: bind an ephemeral
// loopback port, answer a scripted list of canned responses in order, and
// record what each request actually carried. Enough HTTP/1.1 to satisfy
// `reqwest`, and no more.
//
// Shared because asserting on the *request* is how a provider integration
// proves it speaks the right wire format without a paid account. The e2e
// smoke tests still carry their own older copies of this shape.

/// One request as the mock saw it, before any client library is asked to
/// interpret it.
#[allow(dead_code)]
#[derive(Debug)]
pub struct RecordedRequest {
    pub method: String,
    pub path: String,
    /// Header names lowercased. HTTP header names are case-insensitive, so
    /// pinning a particular casing would pin the wrong thing.
    pub headers: Vec<(String, String)>,
    pub body: serde_json::Value,
}

impl RecordedRequest {
    /// The value of `name` (lowercase), or `None` if the request had no such
    /// header -- which is itself worth asserting.
    #[allow(dead_code)]
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }
}

/// One canned response. Status is separate from body so a script can put a
/// transient failure ahead of a success, and `headers` carries the response
/// headers that drive client behavior -- `Retry-After`, most usefully.
#[allow(dead_code)]
#[derive(Clone)]
pub struct CannedResponse {
    pub status: u16,
    pub body: serde_json::Value,
    /// Extra response headers, emitted verbatim. `Content-Type`,
    /// `Content-Length`, and `Connection` are always sent and are not listed
    /// here.
    pub headers: Vec<(String, String)>,
}

impl CannedResponse {
    #[allow(dead_code)]
    pub fn ok(body: serde_json::Value) -> Self {
        Self::status(200, body)
    }

    #[allow(dead_code)]
    pub fn status(status: u16, body: serde_json::Value) -> Self {
        Self {
            status,
            body,
            headers: Vec::new(),
        }
    }

    #[allow(dead_code)]
    #[must_use]
    pub fn with_header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.to_string(), value.to_string()));
        self
    }
}

/// Start a mock HTTP server on an ephemeral loopback port. Returns its
/// address and the channel on which every request arrives.
///
/// The server answers `script` in order, one response per connection. Past
/// the end of the script it repeats the last entry rather than hanging, so an
/// unexpected extra request fails a count assertion instead of a timeout.
#[allow(dead_code)]
pub async fn start_mock_http(
    script: Vec<CannedResponse>,
) -> (
    std::net::SocketAddr,
    tokio::sync::mpsc::UnboundedReceiver<RecordedRequest>,
) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock listener");
    let addr = listener.local_addr().expect("mock local_addr");
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(serve_mock_http(listener, script, tx));
    (addr, rx)
}

async fn serve_mock_http(
    listener: tokio::net::TcpListener,
    script: Vec<CannedResponse>,
    tx: tokio::sync::mpsc::UnboundedSender<RecordedRequest>,
) {
    let mut served = 0usize;
    loop {
        let Ok((mut sock, _)) = listener.accept().await else {
            return;
        };
        let Some(recorded) = read_http_request(&mut sock).await else {
            continue;
        };
        if tx.send(recorded).is_err() {
            return;
        }
        let Some(canned) = script.get(served).or(script.last()).cloned() else {
            return;
        };
        served += 1;

        let body = serde_json::to_string(&canned.body).expect("canned body serializes");
        let extra: String = canned
            .headers
            .iter()
            .map(|(name, value)| format!("{name}: {value}\r\n"))
            .collect();
        let response = format!(
            "HTTP/1.1 {} MOCK\r\nContent-Type: application/json\r\n{}\
             Content-Length: {}\r\nConnection: close\r\n\r\n{}",
            canned.status,
            extra,
            body.len(),
            body,
        );
        let _ = sock.write_all(response.as_bytes()).await;
        let _ = sock.flush().await;
        let _ = sock.shutdown().await;
    }
}

/// Read one HTTP/1.1 request: the request line, its headers, and a
/// `Content-Length`-delimited JSON body.
async fn read_http_request(sock: &mut tokio::net::TcpStream) -> Option<RecordedRequest> {
    use tokio::io::AsyncReadExt;

    let mut buf = vec![0u8; 8192];
    let mut total = Vec::new();

    let header_end = loop {
        let n = sock.read(&mut buf).await.ok()?;
        if n == 0 {
            return None;
        }
        total.extend_from_slice(&buf[..n]);
        if let Some(idx) = total.windows(4).position(|w| w == b"\r\n\r\n") {
            break idx + 4;
        }
        if total.len() > 1 << 20 {
            return None;
        }
    };

    let head = String::from_utf8_lossy(&total[..header_end]).into_owned();
    let mut lines = head.lines();
    let mut request_line = lines.next()?.split_whitespace();
    let method = request_line.next()?.to_string();
    let path = request_line.next()?.to_string();

    let mut headers = Vec::new();
    let mut content_length = 0usize;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let name = name.trim().to_ascii_lowercase();
        let value = value.trim().to_string();
        if name == "content-length" {
            content_length = value.parse().unwrap_or(0);
        }
        headers.push((name, value));
    }

    while total.len() < header_end + content_length {
        let n = sock.read(&mut buf).await.ok()?;
        if n == 0 {
            break;
        }
        total.extend_from_slice(&buf[..n]);
    }
    let body = serde_json::from_slice(&total[header_end..]).unwrap_or(serde_json::Value::Null);

    Some(RecordedRequest {
        method,
        path,
        headers,
        body,
    })
}

/// Everything the mock has recorded so far. Call after the exchange under
/// test finishes; the count is itself an assertion worth making.
#[allow(dead_code)]
pub fn drain_recorded(
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<RecordedRequest>,
) -> Vec<RecordedRequest> {
    let mut out = Vec::new();
    while let Ok(recorded) = rx.try_recv() {
        out.push(recorded);
    }
    out
}

/// Set an environment variable for a test.
///
/// SAFETY: edition 2024 marks `env::set_var` unsafe because of multi-thread
/// races. Callers must use a variable name unique to the test, so no two
/// tests in a binary race on one key.
#[allow(dead_code)]
pub fn set_test_env(var: &str, value: &str) {
    unsafe { std::env::set_var(var, value) }
}

/// Clear a variable set by [`set_test_env`]. Same uniqueness requirement.
#[allow(dead_code)]
pub fn unset_test_env(var: &str) {
    unsafe { std::env::remove_var(var) }
}

/// Materialize `<root>/<sid>/session.json` from [`sample_session`], letting
/// `mutate` reshape the JSON first. Tests on-disk shapes the current code
/// wouldn't write itself -- legacy key names, dropped fields, corruption --
/// without hand-writing a whole record. Returns the session directory.
#[allow(dead_code)]
pub fn write_raw_session(
    root: &std::path::Path,
    sid: &SessionId,
    mutate: impl FnOnce(&mut serde_json::Value),
) -> PathBuf {
    let dir = root.join(sid.as_str());
    std::fs::create_dir_all(&dir).expect("mkdir");
    let mut session = sample_session(sid);
    session.session_dir = dir.clone();
    let mut value = serde_json::to_value(&session).expect("to_value");
    mutate(&mut value);
    std::fs::write(dir.join("session.json"), value.to_string()).expect("write");
    dir
}

/// Mutator for [`write_raw_session`]: rewrite the record the way versions
/// before the 2026-06-01 `container` -> `image` rename did.
#[allow(dead_code)]
pub fn as_legacy_image_key(value: &mut serde_json::Value) {
    let obj = value.as_object_mut().expect("object");
    let name = obj.remove("image_config_name").expect("image_config_name");
    obj.insert("container_config_name".into(), name);
}

/// Mutator for [`write_raw_session`]: drop a field that is still required,
/// standing in for whatever the next unaliased rename breaks.
#[allow(dead_code)]
pub fn drop_image_tag(value: &mut serde_json::Value) {
    value.as_object_mut().expect("object").remove("image_tag");
}
