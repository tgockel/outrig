//! Shared test helpers across integration tests. Cargo treats files in
//! `tests/` as test binaries; subdirectories with `mod.rs` are
//! conventional shared modules (no phantom `common` test binary).

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::Stdio;
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

/// What a session writes into its directory beside `session.json`: a log
/// under `logs/`, and the `outrig-enter` launcher a `view = "primary"` sidecar
/// runs.
#[allow(dead_code)]
pub fn write_outrig_entries(dir: &Path) {
    std::fs::create_dir_all(dir.join("logs")).expect("mkdir logs");
    std::fs::write(dir.join("logs/fs.stderr"), b"server stderr\n").expect("write log");
    std::fs::write(dir.join("outrig-enter"), b"\x7fELF").expect("write launcher");
}

/// Files of the user's own in a directory that also holds a session record --
/// what a `--session-dir` given an existing directory before 0.2.2 left.
#[allow(dead_code)]
pub fn write_user_files(dir: &Path) {
    std::fs::write(dir.join("KEEP_ME.txt"), b"my notes\n").expect("write note");
    std::fs::create_dir_all(dir.join("photos")).expect("mkdir photos");
    std::fs::write(dir.join("photos/holiday.jpg"), b"jpeg bytes\n").expect("write photo");
}

/// After removing a record from a directory [`write_user_files`] wrote into:
/// the user's files are intact, and nothing outrig wrote is left.
#[allow(dead_code)]
pub fn assert_only_user_files_left(dir: &Path) {
    let note = std::fs::read(dir.join("KEEP_ME.txt")).expect("note kept");
    assert_eq!(note, b"my notes\n");
    let photo = std::fs::read(dir.join("photos/holiday.jpg")).expect("photo kept");
    assert_eq!(photo, b"jpeg bytes\n");
    for name in ["session.json", "logs", "outrig-enter"] {
        assert!(
            std::fs::symlink_metadata(dir.join(name)).is_err(),
            "{name} should be removed from {dir:?}"
        );
    }
}

/// The mcp-fs fixture: a `Dockerfile` whose image carries
/// `mcp-server-filesystem`, the server the e2e tests drive.
#[allow(dead_code)]
pub fn fixture_mcp_fs_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("outrig-cli is under crates/")
        .join("outrig/tests/fixtures/mcp-fs")
}

/// Ceiling for an e2e wait on a live `outrig` process or on podman.
#[allow(dead_code)]
pub const E2E_TIMEOUT: Duration = Duration::from_secs(120);

/// Wait for a stderr line starting with `prefix`, and return the rest of it,
/// trimmed.
#[allow(dead_code)]
pub async fn wait_for_stderr_value(stderr: Arc<Mutex<String>>, prefix: &str) -> String {
    tokio::time::timeout(E2E_TIMEOUT, async {
        loop {
            {
                let snapshot = stderr.lock().unwrap().clone();
                if let Some(value) = snapshot
                    .lines()
                    .find_map(|line| line.strip_prefix(prefix).map(str::trim))
                {
                    return value.to_string();
                }
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("stderr lacked {prefix:?}: {}", stderr.lock().unwrap()))
}

/// The names of every container, running or not, that `filter` selects.
#[allow(dead_code)]
pub async fn podman_names(filter: &str) -> Vec<String> {
    let out = tokio::process::Command::new("podman")
        .args(["ps", "-a", "--filter", filter, "--format", "{{.Names}}"])
        .output()
        .await
        .expect("podman ps");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::to_string)
        .filter(|l| !l.is_empty())
        .collect()
}

/// Wait until no container by any of `names` exists.
#[allow(dead_code)]
pub async fn wait_until_gone(names: &[String]) {
    tokio::time::timeout(E2E_TIMEOUT, async {
        loop {
            let mut alive = Vec::new();
            for name in names {
                let found = podman_names(&format!("name={name}")).await;
                alive.extend(found);
            }
            if alive.is_empty() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("containers {names:?} were not reaped within {E2E_TIMEOUT:?}"));
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

    /// The conversation the request carried. OpenAI's chat completions and
    /// Anthropic's messages both send it as a top-level `messages` array.
    #[allow(dead_code)]
    pub fn messages(&self) -> &[serde_json::Value] {
        self.body["messages"].as_array().expect("a messages array")
    }

    /// The role of each of [`Self::messages`], in order: what an endpoint that
    /// requires alternating roles checks.
    #[allow(dead_code)]
    pub fn roles(&self) -> Vec<&str> {
        self.messages()
            .iter()
            .map(|message| message["role"].as_str().unwrap_or_default())
            .collect()
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
    /// Answer nothing at all; see [`Self::held`].
    pub held: bool,
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
            held: false,
        }
    }

    /// A request the mock records and never answers, holding the connection
    /// open until the client gives up on it: a model call still in flight,
    /// which is where a test drops a turn to do what Ctrl-C does.
    #[allow(dead_code)]
    pub fn held() -> Self {
        Self {
            held: true,
            ..Self::status(0, serde_json::Value::Null)
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

        if canned.held {
            // Off the accept loop, so later requests are still served. Reading
            // to EOF is what notices the client has dropped the request.
            tokio::spawn(async move {
                use tokio::io::AsyncReadExt;
                let _ = sock.read_to_end(&mut Vec::new()).await;
            });
            continue;
        }

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

/// Build `cfg`'s `[agents.coding]` through the real `resolve_agent` ->
/// `build_agent` path, for a test whose providers point at
/// [`start_mock_http`] mocks.
///
/// `vars` are the test's own env-var names for its fake keys, each set to `key`
/// -- unique per test, which is what makes the `set_test_env` calls safe. A
/// slice rather than one name because a failover chain has a key per
/// candidate, and candidate selection drops any row whose key is unset -- so a
/// chain that set only the head's var would resolve to a chain of one.
#[allow(dead_code)]
pub async fn build_mock_agent(
    cfg: &outrig::config::Config,
    key: &str,
    vars: &[&str],
    tools: Vec<outrig_cli::session_tool::SessionTool>,
) -> outrig_cli::llm::RigAgent {
    for var in vars {
        set_test_env(var, key);
    }
    // A mock config names no `[models.<name>].model-path`, so the repo root
    // this resolves relative paths against never comes up.
    let resolved = outrig_cli::llm::resolve_agent(cfg, std::path::Path::new("/"), Some("coding"))
        .expect("resolves");
    for var in vars {
        unset_test_env(var);
    }

    #[cfg(feature = "local-llm")]
    let registry = Arc::new(outrig_cli::llm::LlmRegistry::new());
    outrig_cli::llm::build_agent(
        &resolved,
        tools,
        std::path::Path::new("."),
        #[cfg(feature = "local-llm")]
        &registry,
    )
    .await
    .expect("agent builds")
}

/// The name [`FixedTool`] registers under.
#[allow(dead_code)]
pub const FIXED_TOOL: &str = "outrig_test_fixed";

/// A tool that returns `output` whatever it is called with, so a test can put
/// exact text in a tool result and look for it in the request that follows.
#[allow(dead_code)]
pub struct FixedTool {
    pub output: &'static str,
}

impl rig::tool::ToolDyn for FixedTool {
    fn name(&self) -> String {
        FIXED_TOOL.to_string()
    }

    fn description(&self) -> String {
        "Return a fixed text.".to_string()
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({ "type": "object", "properties": {} })
    }

    fn call<'a>(
        &'a self,
        _args: String,
    ) -> rig::wasm_compat::WasmBoxedFuture<'a, std::result::Result<String, rig::tool::ToolError>>
    {
        Box::pin(async move { Ok(self.output.to_string()) })
    }
}

/// Tool results rig 0.40 would not send as written: each is JSON in a shape
/// its `ToolResultContent::from_tool_output` reads as structured (#253).
///
/// * A WireMock stub mapping, as a file read returns it. It has a top-level
///   `response` key, so it would reach the model as that value alone,
///   re-serialized, and without the `request` it matches.
/// * A `response` beside image `parts`, which would reach it as the quoted
///   `response` and an image.
/// * An object shaped as an image, which would reach it as that image.
#[allow(dead_code)]
pub const RESULTS_RIG_RESHAPES: [&str; 3] = [
    r#"{
  "request": { "method": "GET", "url": "/api/health" },
  "response": { "status": 200, "body": "ok" }
}
"#,
    r#"{"response": "Rendered the chart.", "parts": [{"type": "image", "data": "iVBORw0KGgo=", "mimeType": "image/png"}]}"#,
    r#"{"type": "image", "data": "https://example.com/x.png", "mimeType": "image/png"}"#,
];

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

#[allow(dead_code)]
const RUN_OUTRIG_TIMEOUT: Duration = Duration::from_secs(60);

/// A global config that resolves a model, so a run gets past model wiring and
/// into the image cascade. The provider is never contacted.
#[allow(dead_code)]
pub const GLOBAL_WITH_MODEL: &str = r#"
default-model = "fast"

[providers.openai]
style    = "openai"
base-url = "http://127.0.0.1:1/v1"
api-key  = "${OUTRIG_TEST_KEY}"

[models.fast]
provider   = "openai"
identifier = "test-model"
"#;

/// A `PATH` whose `podman` and `buildah` exit non-zero immediately, so image
/// probes and pulls fail instantly instead of hitting the network.
#[allow(dead_code)]
fn stub_runtime_path(dir: &Path) -> std::ffi::OsString {
    for name in ["podman", "buildah"] {
        let stub = dir.join(name);
        std::fs::write(&stub, "#!/bin/sh\nexit 1\n").expect("write runtime stub");
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755))
            .expect("chmod runtime stub");
    }
    let mut path = std::ffi::OsString::from(dir);
    path.push(":");
    path.push(std::env::var_os("PATH").unwrap_or_default());
    path
}

/// Run `outrig` in `cwd` and return whether it succeeded, plus its stderr.
/// `podman` and `buildah` are stubs that fail at once (see
/// `builtin_default.rs`), and `XDG_CACHE_HOME` is a tempdir, so nothing
/// reaches the network or the developer's real cache.
#[allow(dead_code)]
pub async fn run_outrig(cwd: &Path, args: &[&str]) -> (bool, String) {
    run_outrig_with_env(cwd, args, &[]).await
}

/// [`run_outrig`] with extra environment variables, set last so they win.
#[allow(dead_code)]
pub async fn run_outrig_with_env(
    cwd: &Path,
    args: &[&str],
    env: &[(&str, &Path)],
) -> (bool, String) {
    let stubs = tempfile::tempdir().expect("tempdir stubs");
    let cache = tempfile::tempdir().expect("tempdir cache");

    let output = tokio::time::timeout(
        RUN_OUTRIG_TIMEOUT,
        tokio::process::Command::new(env!("CARGO_BIN_EXE_outrig"))
            .args(args)
            .current_dir(cwd)
            .env("OUTRIG_TEST_KEY", "test-key")
            .env("PATH", stub_runtime_path(stubs.path()))
            .env("XDG_CACHE_HOME", cache.path())
            .envs(env.iter().copied())
            .stdin(Stdio::null())
            .output(),
    )
    .await
    .expect("outrig timed out")
    .expect("spawn outrig");

    (
        output.status.success(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}
