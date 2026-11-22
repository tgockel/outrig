//! rmcp client over `podman exec` stdio.
//!
//! Spawns the MCP server as a child of `podman exec -i` against a running
//! [`Container`], hands the resulting `(stdout, stdin)` pair to
//! [`rmcp::service::serve_client`], and exposes our own thin facade so
//! callers don't depend on rmcp directly. Per-server stderr is redirected to
//! `<log_dir>/<name>.stderr` -- written to the file even if the server
//! crashes during the `initialize` handshake.
//!
//! The child handle is owned by [`McpClient`] (not by rmcp's transport
//! wrapper) so [`McpClient::shutdown`] can implement the close-stdin -> wait
//! grace -> kill sequence the MCP spec calls for.
//!
//! Every request has a deadline. rmcp's own helpers send with none, so a
//! server that accepts stdin and never answers would otherwise hold startup
//! or a turn for as long as it liked: `initialize` and `tools/list` each get
//! [`STARTUP_TIMEOUT`], and `tools/call` gets the client's configurable
//! deadline (see [`McpClient::with_call_timeout`]).

use std::collections::{BTreeMap, HashSet};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use rmcp::model::{
    CallToolRequest, CallToolRequestParams, CancelledNotificationParam, ClientRequest,
    CustomResult, ListToolsRequest, PaginatedRequestParams, RequestId, ServerResult,
};
use rmcp::service::{
    NotificationContext, Peer, PeerRequestOptions, RoleClient, RunningService, ServiceError,
    serve_client,
};
use serde_json::{Map, Value};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use crate::config::{EnvValue, McpServerSpec, ResolvedEnvValue, effective_mcp_call_timeout};
use crate::container::{Container, ExecOptions, ExecTarget, embedded::McpDeclarationSource};
use crate::error::{
    IoPathExt, McpDispatchState, McpFailureKind, McpSessionError, OutrigError, Result,
};
use crate::mcp_content::{
    McpTool, McpToolResult, McpUndecodedTool, result_from_rmcp, tool_from_rmcp,
};
use crate::process::{Cmd, Owned, StdioSpec, Transcript};

mod http;

pub(crate) use http::{HttpFailed, client as http_client, header as http_header, reserved_header};

const SHUTDOWN_GRACE: Duration = Duration::from_secs(2);

/// How long a server has to answer `initialize`, and then again to finish
/// answering `tools/list`, every page included. The `initialize` clock starts
/// at the spawn of the transport, so it covers `podman exec` or `podman start
/// --attach` as well as the server's own start, and a server that fetches
/// itself on first launch (`npx -y`, `uvx`) spends its download inside it too
/// -- which is why it is minutes rather than seconds. Past it the server is
/// presumed hung, and startup fails the way it does for a server that crashed.
pub(crate) const STARTUP_TIMEOUT: Duration = Duration::from_secs(120);

/// How many `tools/list` pages a server may send before its listing is
/// refused. No server a model could use comes near it -- most send every tool
/// on one page -- so one still paging here is minting cursors, not listing
/// tools, and would otherwise grow the listing until [`STARTUP_TIMEOUT`].
const TOOLS_LIST_PAGE_CEILING: usize = 1000;

/// The stderr tail of a server OutRig runs no process of, a URL's.
const NO_STDERR: &str = "(none: OutRig runs no process of this server)";

/// The `reason` a stopped or dropped call's `notifications/cancelled` carries.
const ABANDONED_REASON: &str = "outrig stopped waiting for this call";

/// How far off a call's deadline is when its bound is past what the clock
/// can hold: thirty years.
const NO_DEADLINE: Duration = Duration::from_secs(30 * 365 * 24 * 60 * 60);

/// On `Drop` without an explicit [`McpClient::shutdown`], the child is owned by
/// outrig's process layer: it is SIGKILLed synchronously and reaped by the
/// runtime -- not graceful, but no leaked process and no zombie.
///
/// The child is the **host-side transport**, not the server. Both shapes are
/// clients of a container: `podman exec -i` for a server started per session,
/// `podman start --attach` for one that is the container's entrypoint. Killing
/// either closes the transport and tells outrig nothing about the server,
/// which conmon supervises inside the container's own namespaces and which may
/// keep running. Stopping the container is what ends it, and session teardown
/// is what does that -- so a caller that drops an `McpClient` without also
/// stopping the container has closed a pipe, not shut down a server.
///
/// The transport leads a process group of its own, so a terminal's Ctrl-C
/// reaches the process holding this client and not the transport. It belongs
/// to the session rather than to whatever request a Ctrl-C abandons, and
/// [`McpClient::shutdown`] or a drop is what ends it.
///
/// A call that is dropped before its answer lands -- a turn a Ctrl-C
/// abandoned, a proxied call its own client cancelled -- sends the server
/// `notifications/cancelled`, as one that outlives its deadline does.
/// Cancellation is advisory: a server may finish the work anyway, and its
/// answer is discarded.
#[derive(Debug)]
pub struct McpClient {
    name: String,
    service: RunningService<RoleClient, Notices>,
    /// The transport's host-side process; `None` for a transport with no
    /// process of its own, as a test's in-memory one.
    child: Option<Owned>,
    /// Kept so a failure after the handshake can show what the server said,
    /// the way a failed handshake already does.
    stderr_path: PathBuf,
    call_timeout: Duration,
}

impl McpClient {
    /// Spawn `server_cfg`'s command via `podman exec -i` against `container`,
    /// redirect its stderr to `<log_dir>/<name>.stderr`, and drive the MCP
    /// `initialize` handshake. Returns the live client on success.
    ///
    /// `extra_env` carries `--env` CLI overlay entries already merged for this
    /// specific server (global + per-server). They are layered on top of the
    /// config-file env (overriding on key conflict) before resolution.
    ///
    /// `log_dir` is created (and any missing parents) if it doesn't exist.
    /// The `name` is used both for the stderr filename and for diagnostic
    /// messages; callers should pass the server's local config name (e.g.
    /// `"fs"`).
    ///
    /// The `tools/call` deadline is `server_cfg`'s own `call-timeout-secs`, else
    /// the default; a session's `mcp-call-timeout-secs` is the caller's to
    /// resolve and apply with [`with_call_timeout`](Self::with_call_timeout).
    pub async fn connect_via_podman_exec(
        container: &Container,
        server_cfg: &McpServerSpec,
        name: &str,
        log_dir: &Path,
        extra_env: &BTreeMap<String, EnvValue>,
    ) -> Result<Self> {
        Self::connect_via_podman_exec_inner(container, server_cfg, name, None, log_dir, extra_env)
            .await
    }

    pub async fn connect_via_podman_exec_with_source(
        container: &Container,
        server_cfg: &McpServerSpec,
        name: &str,
        declaration_source: McpDeclarationSource,
        log_dir: &Path,
        extra_env: &BTreeMap<String, EnvValue>,
    ) -> Result<Self> {
        Self::connect_via_podman_exec_inner(
            container,
            server_cfg,
            name,
            Some(declaration_source.description()),
            log_dir,
            extra_env,
        )
        .await
    }

    async fn connect_via_podman_exec_inner(
        container: &Container,
        server_cfg: &McpServerSpec,
        name: &str,
        declaration_source: Option<&'static str>,
        log_dir: &Path,
        extra_env: &BTreeMap<String, EnvValue>,
    ) -> Result<Self> {
        let (command, env_spec) = server_cfg.normalize();
        let env = resolve_mcp_env(name, env_spec, extra_env)?;
        let client = Self::connect_exec(
            &container.exec_target(),
            &command,
            env,
            name,
            declaration_source,
            log_dir,
            STARTUP_TIMEOUT,
        )
        .await?;
        Ok(client.with_call_timeout(effective_mcp_call_timeout(
            server_cfg.call_timeout_secs(),
            None,
        )))
    }

    /// Connect an entrypoint-stdio server: spawn
    /// `podman start --attach --interactive <container>` as the owned child,
    /// so the image's ENTRYPOINT becomes the server and container lifetime
    /// equals server lifetime. The container must be created+initialized
    /// (`Container::create_initialized`) with the server's env baked in --
    /// `podman start` carries no `--env`.
    ///
    /// [`McpClient::shutdown`]'s close-stdin -> grace -> kill sequence works
    /// unchanged: EOF on the attached stdin reaches the entrypoint, which
    /// exits and takes the container with it (`--rm`). A server that ignores
    /// EOF gets the *attach child* SIGKILLed, which does not stop the
    /// container itself -- session teardown's `Container::stop` covers that.
    ///
    /// There is no spec here to read a `call-timeout-secs` from: the
    /// `tools/call` deadline starts at the default, for the caller to replace
    /// with [`with_call_timeout`](Self::with_call_timeout).
    pub async fn connect_via_podman_start(
        container: &Container,
        name: &str,
        declaration_source: McpDeclarationSource,
        log_dir: &Path,
    ) -> Result<Self> {
        Self::connect_via_podman_start_within(
            container,
            name,
            Some(declaration_source.description()),
            log_dir,
            STARTUP_TIMEOUT,
        )
        .await
    }

    /// Connect `command`, exec'd in the container `target` names with `env`,
    /// giving its `initialize` handshake `initialize_within`.
    pub(crate) async fn connect_exec(
        target: &ExecTarget,
        command: &[String],
        env: BTreeMap<String, ResolvedEnvValue>,
        name: &str,
        declaration_source: Option<&'static str>,
        log_dir: &Path,
        initialize_within: Duration,
    ) -> Result<Self> {
        // Sidecar servers run wherever their image puts them; no exec-side
        // working directory has come up for one yet.
        let cmd = target.exec_argv(command, &ExecOptions::new().with_resolved_env(env));
        Self::connect_stdio_cmd(
            cmd,
            name,
            declaration_source,
            log_dir,
            target.transcript(),
            // An exec'd server's output is not in the container's log.
            None,
            command,
            initialize_within,
        )
        .await
    }

    /// [`McpClient::connect_via_podman_start`], giving its `initialize`
    /// handshake `initialize_within`.
    pub(crate) async fn connect_via_podman_start_within(
        container: &Container,
        name: &str,
        declaration_source: Option<&'static str>,
        log_dir: &Path,
        initialize_within: Duration,
    ) -> Result<Self> {
        let cmd = Cmd::new("podman")
            .args(["start", "--attach", "--interactive"])
            .arg(container.selector());
        // Derived from the one Cmd so failure diagnostics can't drift from
        // what actually ran.
        let argv: Vec<String> = std::iter::once(cmd.program.to_string())
            .chain(
                cmd.shown_args()
                    .iter()
                    .map(|a| a.to_string_lossy().into_owned()),
            )
            .collect();
        Self::connect_stdio_cmd(
            cmd,
            name,
            declaration_source,
            log_dir,
            container.transcript(),
            container.ctr_log_path(),
            &argv,
            initialize_within,
        )
        .await
    }

    /// Transport-agnostic connection tail: spawn `cmd` with piped stdio and
    /// stderr redirected to `<log_dir>/<name>.stderr`, then drive the MCP
    /// `initialize` handshake, giving the server `initialize_within` to
    /// answer. `display_command` is what a startup failure reports as the
    /// attempted command, and `ctr_log` is where its stderr can still be read
    /// if the attach stream that fills that file comes back empty.
    #[allow(clippy::too_many_arguments)]
    async fn connect_stdio_cmd(
        cmd: Cmd,
        name: &str,
        declaration_source: Option<&'static str>,
        log_dir: &Path,
        transcript: Option<Transcript>,
        ctr_log: Option<&Path>,
        display_command: &[String],
        initialize_within: Duration,
    ) -> Result<Self> {
        let by = tokio::time::Instant::now() + initialize_within;
        tokio::fs::create_dir_all(log_dir)
            .await
            .path_ctx("create directory", log_dir)?;
        let stderr_path = log_dir.join(format!("{name}.stderr"));
        let stderr_file = tokio::fs::File::create(&stderr_path)
            .await
            .path_ctx("create", &stderr_path)?;
        let stderr_std = stderr_file.into_std().await;

        if let Some(transcript) = transcript
            && let Err(e) = transcript
                .line("podman", &format!("$ {}", cmd.render()))
                .await
        {
            crate::process::warn_transcript_failed("podman", &e);
        }

        // Through the shared spawn chokepoint rather than a hand-rolled
        // `Command`, so this child gets the same ownership guarantee as every
        // other: see `crate::process`. The stderr override is the only reason
        // this site needs a spec of its own. Out of the terminal's group, so
        // the transport outlives a turn a Ctrl-C abandons (#335).
        let mut child = cmd.in_own_process_group().spawn_owned(
            StdioSpec::bidirectional().with_stderr(Stdio::from(stderr_std)),
            crate::process::Termination::Kill,
        )?;
        let stdin = child.take_stdin();
        let stdout = child.take_stdout();

        // The unwrapped pipe halves go straight to rmcp via the blanket
        // `IntoTransport for (R, W)` impl. Going through
        // `rmcp::transport::TokioChildProcess::new` would spawn its own child
        // internally, leaving us no `Child` handle for graceful shutdown.
        let handshake =
            tokio::time::timeout_at(by, serve_client(Notices::default(), (stdout, stdin))).await;
        // A handshake that failed is classified already: what diagnosing it
        // waits for is bounded by what its own bound left, so the caller's
        // bound does not end it as out of time. One that ran out of time is
        // transient whatever the diagnosis finds, and keeps the ceilings.
        let (failed, until) = match handshake {
            Ok(Ok(service)) => {
                return Ok(Self {
                    name: name.to_string(),
                    service,
                    child: Some(child),
                    stderr_path,
                    call_timeout: effective_mcp_call_timeout(None, None),
                });
            }
            Ok(Err(source)) => (Handshake::failed(source), Some(by)),
            // rmcp drives the handshake inline, so the elapsed future took the
            // transport with it: the server's stdin is closed by the time
            // `enrich_startup_error` waits on it.
            Err(_) => (Handshake::timed_out(initialize_within), None),
        };
        Err(enrich_startup_error(
            name,
            declaration_source,
            display_command,
            &stderr_path,
            ctr_log,
            &mut child,
            failed,
            until,
        )
        .await)
    }

    /// The first line of what the server said of itself in its `initialize`
    /// answer, if it said anything.
    pub(crate) fn instructions(&self) -> Option<String> {
        let info = self.service.peer_info()?;
        let line = info.instructions.as_deref()?.lines().next()?.trim();
        (!line.is_empty()).then(|| line.to_string())
    }

    /// A client of the server `script` runs under `sh`, its stderr kept in
    /// `log_dir`: a test's server that is a real process.
    #[cfg(test)]
    pub(crate) async fn connect_script(
        name: &str,
        script: &str,
        log_dir: &Path,
        initialize_within: Duration,
    ) -> Result<Self> {
        let cmd = Cmd::new("sh").args(["-c", script]);
        Self::connect_stdio_cmd(cmd, name, None, log_dir, None, None, &[], initialize_within).await
    }

    /// A client of `server`, served in memory with no process behind it, for a
    /// test: the handshake, its bound and its classification are the ones a
    /// podman transport gets.
    #[cfg(test)]
    pub(crate) async fn handshake_in_memory<S>(
        name: &str,
        server: S,
        initialize_within: Duration,
    ) -> Result<Self>
    where
        S: rmcp::Service<rmcp::RoleServer>,
    {
        let never = CancellationToken::new();
        Self::handshake_in_memory_until(name, server, initialize_within, never).await
    }

    /// [`Self::handshake_in_memory`], its server closing its end of the pipe
    /// once `hang_up` is cancelled, as one whose process died does.
    #[cfg(test)]
    pub(crate) async fn handshake_in_memory_until<S>(
        name: &str,
        server: S,
        initialize_within: Duration,
        hang_up: CancellationToken,
    ) -> Result<Self>
    where
        S: rmcp::Service<rmcp::RoleServer>,
    {
        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        tokio::spawn(async move {
            if let Ok(running) = rmcp::service::serve_server(server, server_io).await {
                // Dropping the running service ends its loop, and the pipe with it.
                tokio::select! {
                    _ = running.waiting() => {}
                    () = hang_up.cancelled() => {}
                }
            }
        });
        let failed = match tokio::time::timeout(
            initialize_within,
            serve_client(Notices::default(), client_io),
        )
        .await
        {
            Ok(Ok(service)) => {
                return Ok(Self {
                    name: name.to_string(),
                    service,
                    child: None,
                    stderr_path: PathBuf::new(),
                    call_timeout: effective_mcp_call_timeout(None, None),
                });
            }
            Ok(Err(source)) => Handshake::failed(source),
            Err(_) => Handshake::timed_out(initialize_within),
        };
        Err(startup_failure(
            name,
            None,
            &[],
            None,
            Path::new(""),
            "(empty)".into(),
            failed,
        ))
    }

    /// A start whose server exited with `code` before it answered, having
    /// written `stderr_tail`: what a missing binary looks like, for a test.
    #[cfg(test)]
    pub(crate) fn exited_before_initialize(
        name: &str,
        command: &[String],
        code: i32,
        stderr_tail: &str,
    ) -> OutrigError {
        use std::os::unix::process::ExitStatusExt;
        let failed = Handshake::failed(rmcp::service::ClientInitializeError::ConnectionClosed(
            "initialize response".into(),
        ));
        startup_failure(
            name,
            None,
            command,
            Some(std::process::ExitStatus::from_raw(code << 8)),
            Path::new(""),
            stderr_tail.to_string(),
            failed,
        )
    }

    /// Replace the `tools/call` deadline. The cascade -- a server's own
    /// `call-timeout-secs`, then the session's `mcp-call-timeout-secs`, then
    /// [`DEFAULT_MCP_CALL_TIMEOUT_SECS`](crate::config::DEFAULT_MCP_CALL_TIMEOUT_SECS)
    /// -- is resolved by the caller, which is
    /// the one that has the session's config.
    pub fn with_call_timeout(mut self, timeout: Duration) -> Self {
        self.call_timeout = timeout;
        self
    }

    /// The local name this client was constructed with (e.g. `"fs"`).
    pub fn name(&self) -> &str {
        &self.name
    }

    /// What changes with each `notifications/tools/list_changed` its server
    /// sends, for as long as it is connected, from now: one sent before this
    /// is not heard. Nothing relists on its own: the change is for whoever
    /// decides to.
    pub(crate) fn list_changes(&self) -> watch::Receiver<()> {
        self.service.service().changes.subscribe()
    }

    /// Cancelled once its connection broke where no call was there to say so:
    /// a URL server's stream it gave up resuming, after which no listing
    /// change is heard. It is then to be connected again.
    pub(crate) fn broken(&self) -> CancellationToken {
        self.service.service().broken.clone()
    }

    /// Issue an MCP `tools/list`, following its pages until the server stops
    /// sending a `nextCursor`, and translate the results into our own
    /// [`McpTool`] type. Every page together gets the same fixed bound
    /// `initialize` does, 120 seconds; a listing that outlives it fails with
    /// [`McpFailureKind::Timeout`]. One whose pages would never end -- a
    /// `nextCursor` an earlier page already sent, or a server still paging
    /// after 1000 pages -- fails at once with [`McpFailureKind::Protocol`], as
    /// does one holding a tool this client cannot decode, such as one whose
    /// `inputSchema` is not an object, which the failure names. A failure
    /// carries the server's stderr tail.
    pub async fn list_tools(&self) -> Result<Vec<McpTool>> {
        self.list_tools_within(STARTUP_TIMEOUT).await
    }

    /// [`Self::list_tools`] with the bound named, so a test can reach the
    /// timeout without waiting out the real one. No cancel goes upstream on
    /// expiry: every caller abandons a server whose listing failed.
    pub(crate) async fn list_tools_within(&self, bound: Duration) -> Result<Vec<McpTool>> {
        let Listing { tools, undecoded } = self.list_tools_tolerant_within(bound).await?;
        if undecoded.is_empty() {
            return Ok(tools);
        }
        let named: Vec<String> = undecoded.iter().map(McpUndecodedTool::to_string).collect();
        Err(self
            .list_failed(McpSessionError::new(
                McpFailureKind::Protocol,
                format!(
                    "tools/list holds tools this client cannot decode: {}",
                    named.join("; ")
                ),
            ))
            .await)
    }

    /// A `tools/list` as [`Self::list_tools_within`] bounds and pages it, that
    /// keeps every tool that decodes and names each that does not, rather
    /// than failing for one: a server's malformed tool costs that tool alone.
    pub(crate) async fn list_tools_tolerant_within(&self, bound: Duration) -> Result<Listing> {
        let source = match tokio::time::timeout(bound, self.list_every_page()).await {
            Ok(Ok(listing)) => return Ok(listing),
            Ok(Err(source)) => source,
            Err(_) => McpSessionError::new(
                McpFailureKind::Timeout,
                format!("no tools/list response within {bound:?}"),
            ),
        };
        Err(self.list_failed(source).await)
    }

    /// A listing that failed for `source`, with what the server wrote -- for
    /// a server with no stderr file, a URL's, that it has none.
    async fn list_failed(&self, source: McpSessionError) -> OutrigError {
        let stderr_tail = match self.stderr_path.as_os_str().is_empty() {
            true => NO_STDERR.to_string(),
            // Read once: the server may still be running, and the settle wait
            // exists for a writer that has just exited.
            false => read_stderr_tail_settling_for(&self.stderr_path, Duration::ZERO).await,
        };
        OutrigError::McpToolsListFailed {
            name: self.name.clone(),
            source,
            stderr_path: self.stderr_path.clone(),
            stderr_tail,
        }
    }

    /// Every page of `tools/list`, in order. rmcp's `list_all_tools` pages
    /// until the server omits `nextCursor`, so a server that echoes the cursor
    /// it was sent would be paged forever. Here a cursor any earlier page
    /// already sent -- the echo, or a longer cycle -- is refused before it is
    /// requested again, and so is a listing past [`TOOLS_LIST_PAGE_CEILING`].
    /// Neither message quotes the cursor: the server chose it.
    ///
    /// Each page is asked for through `send_request_with_option` rather than
    /// rmcp's `list_tools`, which caches and fails a page whole for one tool
    /// its typed decode refuses. Such a page arrives as `CustomResult`, the
    /// last of rmcp's untagged `ServerResult` variants, and its tools are
    /// decoded one by one. A page that also carries `_meta` decodes as a
    /// `CallToolResult` before it gets there, its tools lost, and fails as a
    /// response of the wrong kind.
    async fn list_every_page(&self) -> std::result::Result<Listing, McpSessionError> {
        let mut listing = Listing::default();
        let mut seen = HashSet::new();
        let mut cursor = None;
        for page in 1..=TOOLS_LIST_PAGE_CEILING {
            let request = ClientRequest::ListToolsRequest(ListToolsRequest::with_param(
                PaginatedRequestParams::default().with_cursor(cursor),
            ));
            let answered = self
                .service
                .send_request_with_option(request, PeerRequestOptions::no_options())
                .await
                .map_err(|source| listing_error(&source))?
                .await_response()
                .await
                .map_err(|source| listing_error(&source))?;
            let next = match answered {
                ServerResult::ListToolsResult(listed) => {
                    let tools = listed.tools.into_iter().map(tool_from_rmcp);
                    listing.tools.extend(tools);
                    listed.next_cursor
                }
                ServerResult::CustomResult(CustomResult(listed)) => {
                    listing.read_page(page, listed)?
                }
                _ => return Err(session_error_from_rmcp(&ServiceError::UnexpectedResponse)),
            };
            let Some(next) = next else {
                return Ok(listing);
            };
            if !seen.insert(next.clone()) {
                return Err(McpSessionError::new(
                    McpFailureKind::Protocol,
                    format!(
                        "tools/list page {page} handed back a nextCursor an earlier page \
                         already had, so its pages would never end"
                    ),
                ));
            }
            cursor = Some(next);
        }
        Err(McpSessionError::new(
            McpFailureKind::Protocol,
            format!("tools/list was still paging after {TOOLS_LIST_PAGE_CEILING} pages"),
        ))
    }

    /// Issue an MCP `tools/call`. `args` must be a JSON object (forwarded as
    /// the call's `arguments`) or `Value::Null` (no arguments). Other shapes
    /// are a programmer error and return [`OutrigError::McpArgsNotObject`].
    ///
    /// The response's content blocks, structured content, and `_meta` are
    /// carried through intact; `is_error` mirrors the server's flag. See
    /// [`McpToolResult::render_text`] for the single-string view.
    ///
    /// A call that outlives its deadline (see
    /// [`with_call_timeout`](Self::with_call_timeout)) is cancelled at the
    /// server and fails with [`McpFailureKind::Timeout`]; one whose future is
    /// dropped first is cancelled the same way. A response other than a tool result --
    /// an `input_required` or a task, neither of which the protocol revision
    /// this client negotiates can produce -- is [`McpFailureKind::Protocol`].
    pub async fn call_tool(&self, name: &str, args: Value) -> Result<McpToolResult> {
        let arguments = match args {
            Value::Object(map) => Some(map),
            Value::Null => None,
            other => {
                return Err(OutrigError::McpArgsNotObject {
                    kind: kind_of(&other),
                });
            }
        };
        let bound = self.call_timeout;
        let now = tokio::time::Instant::now();
        // A deadline past what the clock can hold is none at all.
        let deadline = now.checked_add(bound).unwrap_or(now + NO_DEADLINE);
        self.call_tool_within(name, arguments, deadline, std::future::pending())
            .await
            .map_err(|failed| {
                let mut source = failed.source;
                if source.kind == McpFailureKind::Timeout {
                    // Read by the model as often as by a person, so it says what
                    // happened to the work and where a longer deadline is set.
                    source.message = format!(
                        "no tools/call response within {bound:?}, so the call was cancelled; \
                         the server's `call-timeout-secs` sets how long its calls may run"
                    );
                }
                OutrigError::McpCallToolFailed {
                    name: self.name.clone(),
                    source,
                }
            })
    }

    /// Issue an MCP `tools/call` of `name` with `arguments`, until `deadline`
    /// or until `cancelled` resolves, whichever is first; on failure, where in
    /// the call's flow it ended, which is what says whether the server can
    /// have seen it (see [`McpCallFailure`]).
    ///
    /// A call stopped by its deadline or by `cancelled` once its request was
    /// handed over sends the server `notifications/cancelled` before it
    /// returns, and is may have dispatched: the notice asks, and proves
    /// nothing. An answer that comes after is discarded. One stopped while its
    /// request was still being handed over has no request id to name, and is
    /// may have dispatched too: a write cut off part-way proves nothing either.
    pub(crate) async fn call_tool_within(
        &self,
        name: &str,
        arguments: Option<Map<String, Value>>,
        deadline: tokio::time::Instant,
        cancelled: impl Future<Output = ()>,
    ) -> std::result::Result<McpToolResult, McpCallFailure> {
        let mut params = CallToolRequestParams::new(name.to_string());
        if let Some(arguments) = arguments {
            params = params.with_arguments(arguments);
        }
        let request = ClientRequest::CallToolRequest(CallToolRequest::new(params));
        // What stops the call, and as which failure: polled by the send and the wait alike.
        let mut stop = std::pin::pin!(async {
            tokio::select! {
                biased;
                () = tokio::time::sleep_until(deadline) => McpFailureKind::Timeout,
                () = cancelled => McpFailureKind::Canceled,
            }
        });
        let sent = tokio::select! {
            biased;
            sent = self
                .service
                .send_request_with_option(request, PeerRequestOptions::no_options()) => sent,
            kind = &mut stop => return Err(McpCallFailure::stopped(kind, false)),
        };
        let handle = sent.map_err(|source| McpCallFailure {
            state: McpDispatchState::MayHaveDispatched,
            source: session_error_from_rmcp(&source),
            code: None,
        })?;
        // The deadline is ours rather than rmcp's, and so is the cancel: a
        // stopped wait sends the notice through the guard, as a caller's own
        // drop does, so there is one sender and one reason.
        let guard = CancelOnDrop::new(handle.peer.clone(), handle.id.clone());
        let stopped = tokio::select! {
            biased;
            answered = handle.await_response() => {
                guard.disarm();
                return answered_call(answered);
            }
            kind = &mut stop => kind,
        };
        guard.send_now().await;
        Err(McpCallFailure::stopped(stopped, true))
    }

    /// Cancel the rmcp service (which closes the child's stdin -- the MCP
    /// spec's normal shutdown signal), wait up to `SHUTDOWN_GRACE` for the
    /// **transport** to exit on its own, then SIGKILL it if it does not.
    ///
    /// On that last path `Ok` means the kill was issued and the reap is owed,
    /// not that it has happened: the bounded `terminate` can itself time out
    /// against a client that cannot be collected, and what covers that is
    /// `Owned`'s `Drop`, which is where the obligation belongs. A transport
    /// that exits within the grace *is* reaped before this returns.
    ///
    /// The transport is the host-side podman client, and it is all this owns.
    /// The server runs in the container, outlives the client that spoke to
    /// it, and is terminated by container teardown -- so a returned `Ok` says
    /// the pipe is closed and the client is gone, not that the server has
    /// stopped.
    pub async fn shutdown(self) -> Result<()> {
        let Self { service, child, .. } = self;

        // A `JoinError` here just means the rmcp task panicked, which doesn't
        // affect our ability to clean up the child below. Bound cancellation
        // too: if the owning container disappeared, the transport task can be
        // wedged behind already-broken podman exec pipes.
        let _ = tokio::time::timeout(SHUTDOWN_GRACE, service.cancel()).await;
        let Some(mut child) = child else {
            return Ok(());
        };

        match tokio::time::timeout(SHUTDOWN_GRACE, child.wait()).await {
            Ok(Ok(_)) => Ok(()),
            Ok(Err(e)) => Err(e.into()),
            // `terminate` kills and awaits the reap, so on the ordinary
            // outcome this returns with the *transport* confirmed gone --
            // the host-side podman client, and only it. The server itself
            // runs in the container, outlives the client that spoke to it,
            // and goes when the container does. The timeout keeps the old
            // bound for a client that cannot be reaped at all; dropping
            // `terminate` there leaves the obligation with `Owned`, which is
            // where it belongs.
            Err(_) => {
                let _ = tokio::time::timeout(SHUTDOWN_GRACE, child.terminate()).await;
                Ok(())
            }
        }
    }
}

/// Owes the server a `notifications/cancelled` for one request until
/// disarmed. rmcp's `RequestHandle` has no `Drop`, so a request future dropped
/// mid-flight would otherwise leave the server working on an answer nobody
/// will read.
struct CancelOnDrop {
    owed: Option<(Peer<RoleClient>, RequestId)>,
}

impl CancelOnDrop {
    fn new(peer: Peer<RoleClient>, id: RequestId) -> Self {
        Self {
            owed: Some((peer, id)),
        }
    }

    fn disarm(mut self) {
        self.owed = None;
    }

    /// Send the notice now, and owe nothing after: what a call stopped on
    /// purpose does, so that it has been sent when the call returns.
    async fn send_now(mut self) {
        if let Some((peer, id)) = self.owed.take() {
            notify_cancelled(peer, id).await;
        }
    }
}

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        let Some((peer, id)) = self.owed.take() else {
            return;
        };
        // `Drop` cannot await, so the notice goes on a task of its own -- and
        // spawning outside a runtime panics, so a drop during runtime teardown,
        // with no one left to deliver it, owes nothing.
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        runtime.spawn(notify_cancelled(peer, id));
    }
}

/// Send the server `notifications/cancelled` for the request `id`. Bounded
/// like the rest of the transport's goodbyes: a wedged pipe must not keep it
/// alive.
async fn notify_cancelled(peer: Peer<RoleClient>, id: RequestId) {
    let notice = CancelledNotificationParam::new(Some(id), Some(ABANDONED_REASON.to_string()));
    let _ = tokio::time::timeout(SHUTDOWN_GRACE, peer.notify_cancelled(notice)).await;
}

/// How a `tools/call` made with [`McpClient::call_tool_within`] failed:
/// where in its flow it ended -- which is what says whether its server can
/// have seen it -- and what failed there.
#[derive(Debug)]
pub(crate) struct McpCallFailure {
    /// Where it ended: decided by where the failure happened, never read from
    /// what failed.
    pub(crate) state: McpDispatchState,
    /// What failed.
    pub(crate) source: McpSessionError,
    /// The JSON-RPC error code, when the server answered with an error.
    pub(crate) code: Option<i32>,
}

impl McpCallFailure {
    /// Stopped by its deadline (`Timeout`) or by its caller (`Canceled`),
    /// the server sent the notice when `noticed`.
    fn stopped(kind: McpFailureKind, noticed: bool) -> Self {
        let why = match kind {
            McpFailureKind::Timeout => "its deadline passed",
            _ => "it was told to stop",
        };
        let message = match noticed {
            true => format!(
                "{why} before the server answered, so the server was sent \
                 notifications/cancelled, which asks it to stop and does not prove it did"
            ),
            false => format!(
                "{why} while its request was being sent, so the server may have it; no \
                 request id had been given yet for a cancel to name"
            ),
        };
        Self {
            state: McpDispatchState::MayHaveDispatched,
            source: McpSessionError::new(kind, message),
            code: None,
        }
    }
}

/// What a `tools/call` whose wait ended with `answered` came to.
fn answered_call(
    answered: std::result::Result<ServerResult, ServiceError>,
) -> std::result::Result<McpToolResult, McpCallFailure> {
    let source = match answered {
        Ok(ServerResult::CallToolResult(result)) => return Ok(result_from_rmcp(result)),
        Ok(_) => ServiceError::UnexpectedResponse,
        Err(source) => source,
    };
    let code = match &source {
        ServiceError::McpError(error) => Some(error.code.0),
        _ => None,
    };
    Err(McpCallFailure {
        state: wait_state(&source),
        source: session_error_from_rmcp(&source),
        code,
    })
}

/// Where a `tools/call` whose wait failed with `source` ended. The server
/// answered when it sent an error or a message the protocol does not allow
/// there; anything else -- the transport closed, a failure this build does
/// not classify -- came after the request was handed over and before any
/// answer was read, so the server may have it. Decided by position, so a
/// variant an SDK upgrade adds is may have dispatched, never guessed at.
fn wait_state(source: &ServiceError) -> McpDispatchState {
    match source {
        ServiceError::McpError(_)
        | ServiceError::UnexpectedResponse
        | ServiceError::InputRequiredRoundsExceeded { .. } => McpDispatchState::ResponseReceived,
        _ => McpDispatchState::MayHaveDispatched,
    }
}

/// A `tools/list`, as far as it decoded: the tools that did, in the server's
/// order, and each that did not.
#[derive(Debug, Default)]
pub(crate) struct Listing {
    pub(crate) tools: Vec<McpTool>,
    pub(crate) undecoded: Vec<McpUndecodedTool>,
}

impl Listing {
    /// Take in page `page`, which rmcp's typed decode refused and handed over
    /// as the JSON it was: each of its `tools` that decodes is kept, and each
    /// that does not is named with why. Its `nextCursor`, if it has one.
    fn read_page(
        &mut self,
        page: usize,
        listed: Value,
    ) -> std::result::Result<Option<String>, McpSessionError> {
        let unreadable = |what: &str| {
            McpSessionError::new(
                McpFailureKind::Protocol,
                format!("tools/list page {page} is not a listing: {what}"),
            )
        };
        let Value::Object(mut listed) = listed else {
            return Err(unreadable("it is not an object"));
        };
        let next = match listed.remove("nextCursor") {
            None | Some(Value::Null) => None,
            Some(Value::String(next)) => Some(next),
            Some(_) => return Err(unreadable("its nextCursor is not a string")),
        };
        let Some(Value::Array(tools)) = listed.remove("tools") else {
            return Err(unreadable("its tools are not a list"));
        };
        for tool in tools {
            let name = tool.get("name").and_then(Value::as_str).map(str::to_string);
            match serde_json::from_value::<rmcp::model::Tool>(tool) {
                Ok(tool) => self.tools.push(tool_from_rmcp(tool)),
                Err(e) => self
                    .undecoded
                    .push(McpUndecodedTool::new(name, e.to_string())),
            }
        }
        Ok(next)
    }
}

/// What a client hears from its server unasked: here only that its tool
/// listing changed, for whoever watches [`McpClient::list_changes`]; and
/// whether it can still hear it. Its `get_info` is the trait's own, as `()`'s
/// is, so the handshake is the one a client with no handler makes.
#[derive(Debug, Default)]
pub(crate) struct Notices {
    changes: watch::Sender<()>,
    /// Cancelled once a stream the server speaks on broke past resuming, the
    /// connection gone though its transport runs on: a URL's. A process's
    /// pipes break with the transport, and this never is.
    broken: CancellationToken,
}

impl rmcp::ClientHandler for Notices {
    async fn on_tool_list_changed(&self, _context: NotificationContext<RoleClient>) {
        self.changes.send_replace(());
    }
}

/// Layer the CLI `--env` overlay onto the config-file env (overlay wins on
/// key conflict) and resolve each value -- literals verbatim, `${VAR}` refs
/// from the host environment -- keeping what each was resolved from. `name`
/// labels resolution failures with the owning server.
///
/// Shared by both stdio transports: exec-stdio resolves at connect time
/// (`podman exec --env`), entrypoint-stdio at container-create time (`podman
/// create --env`), and each hands the result to `with_resolved_env`, so a
/// `${VAR}` value reaches podman by name and is shown as the reference.
pub fn resolve_mcp_env(
    name: &str,
    config_env: BTreeMap<String, EnvValue>,
    extra_env: &BTreeMap<String, EnvValue>,
) -> Result<BTreeMap<String, ResolvedEnvValue>> {
    let mut env_spec = config_env;
    for (key, value) in extra_env {
        env_spec.insert(key.clone(), value.clone());
    }
    let mut env = BTreeMap::new();
    for (key, value) in env_spec {
        let resolved = ResolvedEnvValue::resolve(value).map_err(|source| {
            OutrigError::McpEnvResolveFailed {
                name: name.to_string(),
                key: key.clone(),
                source,
            }
        })?;
        env.insert(key, resolved);
    }
    Ok(env)
}

/// Convert rmcp's bare transport error from `serve_client` into a richer
/// `McpStartupFailed` carrying the server name, the command we tried to run,
/// the child's exit status (so the user sees *why* the pipe closed), and a
/// tail of whatever the child wrote to stderr. The brief `child.wait()`
/// timeout lets a child that crashed mid-write flush its last few bytes
/// before we read them. `until`, when given, bounds both waits.
///
/// For `podman start --attach` the child is a client relaying the container's
/// stderr, and it can relay none of it for a container that exits within
/// milliseconds. `ctr_log` is where that text still is: see
/// [`restore_lost_stderr`].
#[allow(clippy::too_many_arguments)]
async fn enrich_startup_error(
    name: &str,
    declaration_source: Option<&str>,
    command: &[String],
    stderr_path: &Path,
    ctr_log: Option<&Path>,
    child: &mut Owned,
    failed: Handshake,
    until: Option<tokio::time::Instant>,
) -> OutrigError {
    let within = |ceiling: Duration| match until {
        Some(by) => ceiling.min(by.saturating_duration_since(tokio::time::Instant::now())),
        None => ceiling,
    };
    let exit_status = match tokio::time::timeout(within(CHILD_EXIT_CEILING), child.wait()).await {
        Ok(Ok(status)) => Some(status),
        _ => None,
    };
    // Not before the client has exited: until then it may still be writing the
    // file, and what is in it is not yet what it will hold.
    if let (Some(ctr_log), Some(_)) = (ctr_log, exit_status) {
        restore_lost_stderr(name, stderr_path, ctr_log).await;
    }
    // A client that has exited wrote all it ever will: read once.
    let settle = match exit_status {
        Some(_) => Duration::ZERO,
        None => within(STDERR_SETTLE_CEILING),
    };
    let stderr_tail = read_stderr_tail_settling_for(stderr_path, settle).await;

    let what = match exit_status {
        Some(status) if status.success() => None,
        Some(status) => Some(format!(
            "terminated before initialize ({})",
            format_exit(Some(status))
        )),
        // A handshake that timed out against a server ignoring its closed
        // stdin: it did not terminate, and saying so would send the reader
        // looking for a crash.
        None => Some("never completed initialize and is still running".to_string()),
    };
    if let Some(what) = what {
        tracing::error!(
            target: "outrig::mcp",
            server = name,
            "mcp server {name:?} {what}; see {} for details",
            stderr_path.display()
        );
    }

    startup_failure(
        name,
        declaration_source,
        command,
        exit_status,
        stderr_path,
        stderr_tail,
        failed,
    )
}

/// A handshake that failed: the SDK's error, or the bound that ran out, with
/// its one classification.
struct Handshake {
    source: Box<dyn std::error::Error + Send + Sync>,
    classified: McpSessionError,
    class: StartupClass,
}

impl Handshake {
    fn failed(source: rmcp::service::ClientInitializeError) -> Self {
        let (kind, class) = init_error_class(&source);
        Self {
            classified: McpSessionError::new(kind, source.to_string()),
            source: Box::new(source),
            class,
        }
    }

    fn timed_out(bound: Duration) -> Self {
        let message = format!("no `initialize` response within {bound:?}");
        Self {
            source: Box::new(McpSessionError::new(
                McpFailureKind::Timeout,
                message.clone(),
            )),
            classified: McpSessionError::new(McpFailureKind::Timeout, message),
            class: StartupClass::Transient,
        }
    }
}

/// The startup failure of server `name`, which ran `command` and ended with
/// `exit_status` -- `None` when it was still running -- having written
/// `stderr_tail` to `stderr_path`, its handshake having `failed`.
fn startup_failure(
    name: &str,
    declaration_source: Option<&str>,
    command: &[String],
    exit_status: Option<std::process::ExitStatus>,
    stderr_path: &Path,
    stderr_tail: String,
    failed: Handshake,
) -> OutrigError {
    let Handshake {
        source,
        classified,
        class,
    } = failed;
    OutrigError::McpStartupFailed(Box::new(crate::error::McpStartupFailure {
        name: name.to_string(),
        declaration_source: declaration_source.map(str::to_string),
        command: render_command(command),
        exit: format_exit(exit_status),
        stderr_path: stderr_path.to_path_buf(),
        stderr_tail,
        kind: classified.kind,
        class: by_exit(class, classified.kind, exit_status),
        classified,
        source,
    }))
}

/// What a failed start means for trying again: the classes of
/// `mcp-presenter.md`'s "Startup" table. Only a transient one is retried.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StartupClass {
    /// The transport failed or closed, or the handshake timed out: it may
    /// work if tried again.
    Transient,
    /// The server answered `initialize` with an error.
    Refused,
    /// The declaration cannot work as written: a binary that is not there, a
    /// protocol revision that cannot be agreed, a value that does not resolve.
    Misconfigured,
    /// Anything else, named so rather than guessed at.
    Unclassified,
}

/// `class`, for a transport failure of a stdio server that ended with
/// `exit_status`: one that exited 126 or 127 -- the shell's "cannot run" and
/// "not found", which `podman exec` passes on -- is misconfigured, one that
/// exited otherwise unclassified, since a server that exits before it answers
/// would only exit again; one still running stays as it was.
fn by_exit(
    class: StartupClass,
    kind: McpFailureKind,
    exit_status: Option<std::process::ExitStatus>,
) -> StartupClass {
    match (kind, exit_status) {
        (McpFailureKind::Transport, Some(status)) => match status.code() {
            Some(126 | 127) => StartupClass::Misconfigured,
            _ => StartupClass::Unclassified,
        },
        _ => class,
    }
}

/// Classify a failed `initialize` handshake: the kind a mid-session failure of
/// the same shape has, and what it means for trying again.
fn init_error_class(
    source: &rmcp::service::ClientInitializeError,
) -> (McpFailureKind, StartupClass) {
    use rmcp::service::ClientInitializeError as E;

    use McpFailureKind as K;
    use StartupClass as C;

    // `ClientInitializeError` is `#[non_exhaustive]`, so the wildcard is
    // mandatory; `REVIEWED_RMCP`, in `tests/reviewed_rmcp.rs`, is what keeps
    // it from going unreviewed.
    match source {
        E::ConnectionClosed(_) | E::TransportError { .. } => (K::Transport, C::Transient),
        E::JsonRpcError(_) => (K::Protocol, C::Refused),
        E::NoCompatibleProtocolVersion { .. } | E::NoPreferredProtocolVersion => {
            (K::Protocol, C::Misconfigured)
        }
        E::ExpectedInitResponse(_)
        | E::ExpectedInitResult(_)
        | E::ConflictInitResponseId(..)
        | E::UncorrelatedErrorResponse { .. } => (K::Protocol, C::Unclassified),
        E::Cancelled => (K::Canceled, C::Unclassified),
        // The legacy handshake's failure is the one this client acts on.
        E::LegacyFallbackFailed { fallback, .. } => init_error_class(fallback),
        _ => (K::Other, C::Unclassified),
    }
}

/// Put the container's own stderr into `stderr_path` when the attach stream
/// left it empty. That stream can relay none of a container that exits within
/// milliseconds (#422, and [`crate::container::ctr_log`] for why), and the
/// failure then arrives as `exit: code 1` beside `(empty)`.
///
/// conmon had logged the stderr by then, so `ctr_log` has it. It goes into the
/// file, not only into the error, so `<server>.stderr`, `outrig logs` and the
/// error agree. The client has exited, so nothing else writes the file and the
/// log is complete: one look is enough, which is why this is not part of the
/// settle that follows. A file that holds anything is left as it is.
async fn restore_lost_stderr(name: &str, stderr_path: &Path, ctr_log: &Path) {
    if !matches!(tokio::fs::metadata(stderr_path).await, Ok(metadata) if metadata.len() == 0) {
        return;
    }
    let text = match crate::container::ctr_log::stderr_text(ctr_log).await {
        Ok(text) if !text.is_empty() => text,
        Ok(_) => {
            tracing::info!(
                target: "outrig::mcp",
                server = name,
                "mcp server {name:?}: neither podman's attach stream nor the container's log \
                 holds any stderr"
            );
            return;
        }
        Err(e) => {
            tracing::debug!(
                target: "outrig::mcp",
                server = name,
                "mcp server {name:?}: could not read the container's log {}: {e}",
                ctr_log.display()
            );
            return;
        }
    };
    match tokio::fs::write(stderr_path, &text).await {
        Ok(()) => tracing::warn!(
            target: "outrig::mcp",
            server = name,
            "mcp server {name:?}: podman's attach stream carried none of its stderr; restored \
             {} bytes of it from the container's log into {}",
            text.len(),
            stderr_path.display()
        ),
        Err(e) => tracing::warn!(
            target: "outrig::mcp",
            server = name,
            "mcp server {name:?}: could not restore its stderr into {}: {e}",
            stderr_path.display()
        ),
    }
}

/// How long [`enrich_startup_error`] gives the child to exit.
///
/// The child is a container engine, not a process -- `podman start --attach`
/// has an image store and an OCI runtime behind it. The 250 ms this replaces
/// bounded a flush rather than a hang, and a host under load got "still
/// running (wait timed out)" in place of an exit status. It stays bounded
/// because `serve_client` can fail against a server that is still running,
/// which is the only reason there is a timeout here at all.
const CHILD_EXIT_CEILING: Duration = Duration::from_secs(2);

/// How long [`read_stderr_tail_settling_for`] keeps looking for stderr that
/// has not landed yet.
///
/// A client that is still running can still write the file; one that has
/// exited cannot, so for it an empty file is empty for good, and its file is
/// read once. (#422's `(empty)` was output podman dropped, not output that
/// was late: see [`restore_lost_stderr`].)
///
/// Paid only on a failure path, and only while the file is *still* empty: a
/// server that genuinely said nothing waits this out once, on its way to an
/// error it was going to return regardless.
const STDERR_SETTLE_CEILING: Duration = Duration::from_secs(5);

/// How often [`read_stderr_tail_settling_for`] re-reads while it waits.
const STDERR_SETTLE_POLL: Duration = Duration::from_millis(25);

fn format_exit(status: Option<std::process::ExitStatus>) -> String {
    let Some(status) = status else {
        return "still running (wait timed out)".to_string();
    };
    if let Some(code) = status.code() {
        return format!("code {code}");
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(sig) = status.signal() {
            return format!("signal {sig}");
        }
    }
    "terminated".to_string()
}

async fn read_stderr_bytes(path: &Path) -> std::io::Result<Vec<u8>> {
    use tokio::io::{AsyncReadExt, AsyncSeekExt, SeekFrom};
    const MAX: u64 = 2048;

    let mut file = tokio::fs::File::open(path).await?;
    let len = file.metadata().await?.len();
    if len > MAX {
        file.seek(SeekFrom::End(-(MAX as i64))).await?;
    }
    let mut buf = Vec::with_capacity(len.min(MAX) as usize);
    // Capped on the read as well as the seek: a server still running -- the
    // `tools/list` failure path reads its file live -- can append between
    // the two, and an uncapped read would follow it.
    file.take(MAX).read_to_end(&mut buf).await?;
    Ok(buf)
}

fn render_stderr_tail(read: std::io::Result<Vec<u8>>) -> String {
    match read {
        Ok(bytes) if bytes.is_empty() => "(empty)".to_string(),
        Ok(bytes) => String::from_utf8_lossy(&bytes).trim_end().to_string(),
        Err(_) => "(could not read stderr file)".to_string(),
    }
}

/// The tail of `path`, giving a writer that has not caught up `ceiling` to
/// land: [`STDERR_SETTLE_CEILING`] at most, and nothing for a writer that has
/// exited. See that constant for why one read is not always enough.
async fn read_stderr_tail_settling_for(path: &Path, ceiling: Duration) -> String {
    let deadline = tokio::time::Instant::now() + ceiling;
    loop {
        let read = read_stderr_bytes(path).await;
        // Empty is the only outcome a later write changes. A file that cannot
        // be opened will not open by being asked again, and a non-empty one is
        // already the answer -- so neither spins.
        let still_empty = matches!(&read, Ok(bytes) if bytes.is_empty());
        if !still_empty || tokio::time::Instant::now() >= deadline {
            return render_stderr_tail(read);
        }
        tokio::time::sleep(STDERR_SETTLE_POLL).await;
    }
}

/// `argv` as one line a shell would read back as it: a word holding a space
/// or a quote, quoted.
pub(crate) fn render_command(argv: &[String]) -> String {
    argv.iter()
        .map(|a| {
            if a.is_empty()
                || a.chars()
                    .any(|c| c.is_whitespace() || matches!(c, '\'' | '"' | '$' | '`' | '\\'))
            {
                format!("'{}'", a.replace('\'', "'\\''"))
            } else {
                a.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

pub(crate) fn kind_of(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// Classify an rmcp session failure as outrig's own [`McpSessionError`].
///
/// A fifth conversion across the boundary [`crate::mcp_content`] documents,
/// and a free function for the same reason its four are: a public
/// `From<rmcp type>` would put the SDK back on the surface.
fn session_error_from_rmcp(source: &rmcp::service::ServiceError) -> McpSessionError {
    use rmcp::service::ServiceError as E;

    // `ServiceError` is `#[non_exhaustive]`, so the wildcard is mandatory and
    // a variant an SDK upgrade adds becomes `Other` without a compile error.
    // Review this mapping on every rmcp bump, with `init_error_class` and
    // `SUPPORTED_PROTOCOL_VERSIONS` -- `plan/next/rmcp-list-result-spec-gaps.md`
    // carries the obligations, and `REVIEWED_RMCP` in `tests/reviewed_rmcp.rs`
    // makes the bump say so.
    let kind = match source {
        E::TransportSend(_) | E::TransportClosed => McpFailureKind::Transport,
        E::McpError(_) | E::UnexpectedResponse | E::InputRequiredRoundsExceeded { .. } => {
            McpFailureKind::Protocol
        }
        E::Timeout { .. } => McpFailureKind::Timeout,
        E::Cancelled { .. } => McpFailureKind::Canceled,
        // Everything else, `SubscriptionLagged` included -- a consumer that
        // fell behind its own notification buffer is neither the peer's
        // failure nor the transport's.
        _ => McpFailureKind::Other,
    };
    McpSessionError::new(kind, source.to_string())
}

/// A `tools/list` that failed for `source`, as [`session_error_from_rmcp`]
/// says, but for an HTTP failure under it: classified as one at `initialize`
/// is, so that a server that refuses the listing is refused, not tried again.
fn listing_error(source: &rmcp::service::ServiceError) -> McpSessionError {
    let mut error = session_error_from_rmcp(source);
    if let Some((kind, class)) = http::request_class(source) {
        error.kind = kind;
        error.class = Some(class);
    }
    error
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::container::ctr_log::testing::{entry, log_holding};

    fn transport_error() -> rmcp::transport::DynamicTransportError {
        rmcp::transport::DynamicTransportError::from_parts(
            "test",
            std::any::TypeId::of::<()>(),
            Box::new(std::io::Error::other("boom")),
        )
    }

    /// All eight `ServiceError` variants rmcp 3.4.1 declares, named one by
    /// one, because the classifier's wildcard means the list is the only
    /// record of what was classified on purpose. Also pins that the SDK's
    /// wording crosses the boundary verbatim: `kind` is the contract,
    /// `message` is what a human reads.
    #[rustfmt::skip]
    #[test]
    fn every_rmcp_service_error_is_classified() {
        use rmcp::model::ErrorData;
        use rmcp::service::ServiceError as E;

        use McpFailureKind as K;

        for (source, expected) in [
            (E::TransportSend(transport_error()), K::Transport),
            (E::TransportClosed, K::Transport),
            (E::McpError(ErrorData::internal_error("upstream said no", None)), K::Protocol),
            (E::UnexpectedResponse, K::Protocol),
            (E::InputRequiredRoundsExceeded { max_rounds: 3 }, K::Protocol),
            (E::Timeout { timeout: Duration::from_secs(5) }, K::Timeout),
            (E::Cancelled { reason: Some("the caller stopped".to_string()) }, K::Canceled),
            (E::SubscriptionLagged { capacity: 16 }, K::Other),
        ] {
            let rendered = source.to_string();
            let classified = session_error_from_rmcp(&source);
            assert_eq!(classified.kind, expected, "classifying {rendered:?}");
            assert_eq!(
                classified.message, rendered,
                "the SDK's own wording is what a human reads"
            );
        }
    }

    /// Where a `tools/call` whose wait failed ended, for all eight `ServiceError` variants rmcp
    /// 3.4.1 declares: an error the server answered, or a message it may not send there, is a
    /// response; everything else came after the request was handed over and before any answer,
    /// so the server may have it -- a variant this build files as `Other` included, which is how
    /// one an SDK upgrade adds is read.
    #[rustfmt::skip]
    #[test]
    fn a_failed_wait_is_placed_by_position() {
        use rmcp::model::ErrorData;
        use rmcp::service::ServiceError as E;

        use McpDispatchState as S;

        for (source, expected) in [
            (E::TransportSend(transport_error()), S::MayHaveDispatched),
            (E::TransportClosed, S::MayHaveDispatched),
            (E::McpError(ErrorData::internal_error("upstream said no", None)), S::ResponseReceived),
            (E::UnexpectedResponse, S::ResponseReceived),
            (E::InputRequiredRoundsExceeded { max_rounds: 3 }, S::ResponseReceived),
            (E::Timeout { timeout: Duration::from_secs(5) }, S::MayHaveDispatched),
            (E::Cancelled { reason: None }, S::MayHaveDispatched),
            (E::SubscriptionLagged { capacity: 16 }, S::MayHaveDispatched),
        ] {
            assert_eq!(wait_state(&source), expected, "placing {source}");
        }
        let Err(failed) = answered_call(Err(E::SubscriptionLagged { capacity: 16 })) else {
            panic!("a lagged wait is a failure")
        };
        assert_eq!(
            (failed.state, failed.source.kind),
            (S::MayHaveDispatched, McpFailureKind::Other)
        );
        let Err(failed) = answered_call(Err(E::McpError(ErrorData::invalid_params("no", None))))
        else {
            panic!("an error answered is a failure")
        };
        assert_eq!(failed.code, Some(rmcp::model::ErrorCode::INVALID_PARAMS.0));
    }

    /// A call made once its server has closed the transport may have reached it -- a failed
    /// write proves nothing -- and is the transport's failure.
    #[tokio::test]
    async fn a_call_on_a_closed_transport_may_have_dispatched() {
        let hang_up = CancellationToken::new();
        let (cancels, _heard) = tokio::sync::mpsc::unbounded_channel();
        let stalling = Stalling {
            calls: Default::default(),
            cancels,
        };
        let client = McpClient::handshake_in_memory_until(
            "svc",
            stalling,
            Duration::from_secs(5),
            hang_up.clone(),
        )
        .await
        .expect("connects");
        hang_up.cancel();
        // Once the client has heard the pipe close, so the call is made after it.
        crate::python::testing::until("the transport's close", || {
            client.service.is_transport_closed()
        })
        .await;
        let far = tokio::time::Instant::now() + Duration::from_secs(5);
        let failed = client
            .call_tool_within("quick", None, far, std::future::pending())
            .await
            .expect_err("nothing answers");
        assert_eq!(
            (failed.state, failed.source.kind),
            (
                McpDispatchState::MayHaveDispatched,
                McpFailureKind::Transport
            )
        );
    }

    /// A call told to stop is sent its cancel before it returns, and may have dispatched, the
    /// failure `Canceled`; one past its deadline is the same, `Timeout`.
    #[tokio::test]
    async fn a_stopped_call_sends_its_cancel_before_it_returns() {
        let (client, mut heard, _dir) = client_against_stalling_server().await;
        let far = tokio::time::Instant::now() + Duration::from_secs(30);
        let failed = client
            .call_tool_within(
                "slow",
                None,
                far,
                tokio::time::sleep(Duration::from_millis(100)),
            )
            .await
            .expect_err("told to stop");
        assert_eq!(
            (failed.state, failed.source.kind),
            (
                McpDispatchState::MayHaveDispatched,
                McpFailureKind::Canceled
            )
        );
        assert!(
            failed.source.message.contains("notifications/cancelled"),
            "{}",
            failed.source.message
        );
        assert_eq!(next_cancel(&mut heard).await, "slow");

        let soon = tokio::time::Instant::now() + Duration::from_millis(100);
        let failed = client
            .call_tool_within("slow", None, soon, std::future::pending())
            .await
            .expect_err("past its deadline");
        assert_eq!(failed.source.kind, McpFailureKind::Timeout);
        assert_eq!(next_cancel(&mut heard).await, "slow");
    }

    /// All eleven `ClientInitializeError` variants rmcp 3.4.1 declares, named
    /// one by one with the kind and the startup class each gets: the
    /// classifier's wildcard means this list is the only record of what was
    /// classified on purpose. The SDK's wording reaches `classified` verbatim.
    #[rustfmt::skip]
    #[test]
    fn every_rmcp_init_error_is_classified() {
        use rmcp::model::{ErrorData, NumberOrString};
        use rmcp::service::ClientInitializeError as E;

        use McpFailureKind as K;
        use StartupClass as C;

        let id = |n| NumberOrString::Number(n);
        let closed = || E::ConnectionClosed("initialize response".into());
        for (source, kind, class) in [
            (E::ExpectedInitResponse(None), K::Protocol, C::Unclassified),
            (E::ExpectedInitResult(None), K::Protocol, C::Unclassified),
            (E::ConflictInitResponseId(id(1), id(2)), K::Protocol, C::Unclassified),
            (E::UncorrelatedErrorResponse { expected: id(1), received: id(2) }, K::Protocol,
             C::Unclassified),
            (closed(), K::Transport, C::Transient),
            (E::TransportError { error: transport_error(), context: "send initialize".into() },
             K::Transport, C::Transient),
            (E::JsonRpcError(ErrorData::invalid_request("not you", None)), K::Protocol,
             C::Refused),
            (E::NoCompatibleProtocolVersion { client_supported: vec![], server_supported: vec![] },
             K::Protocol, C::Misconfigured),
            (E::NoPreferredProtocolVersion, K::Protocol, C::Misconfigured),
            (E::Cancelled, K::Canceled, C::Unclassified),
            (E::LegacyFallbackFailed {
                discover: Box::new(E::NoPreferredProtocolVersion),
                fallback: Box::new(closed()),
            }, K::Transport, C::Transient),
        ] {
            let rendered = source.to_string();
            let failed = Handshake::failed(source);
            assert_eq!((failed.classified.kind, failed.class), (kind, class), "{rendered:?}");
            assert_eq!(failed.classified.message, rendered);
            assert_eq!(failed.source.to_string(), rendered, "the SDK's error is kept as raised");
        }
    }

    /// Fork 6: a stdio server whose transport closed is judged by how it
    /// ended -- not found or not runnable is the declaration's fault, any
    /// other exit unclassified, still running transient -- and a failure that
    /// is not the transport's keeps its class whatever the exit.
    #[test]
    fn a_server_that_exited_is_classified_by_its_exit() {
        use std::os::unix::process::ExitStatusExt;
        let exited = |code: i32| Some(std::process::ExitStatus::from_raw(code << 8));
        let transient = StartupClass::Transient;

        for (exit, expected) in [
            (exited(127), StartupClass::Misconfigured),
            (exited(126), StartupClass::Misconfigured),
            (exited(1), StartupClass::Unclassified),
            (exited(0), StartupClass::Unclassified),
            (None, StartupClass::Transient),
        ] {
            assert_eq!(
                by_exit(transient, McpFailureKind::Transport, exit),
                expected
            );
        }
        assert_eq!(
            by_exit(transient, McpFailureKind::Timeout, exited(0)),
            transient,
            "a handshake that timed out is not judged by the exit its closed stdin caused"
        );
        assert_eq!(
            by_exit(StartupClass::Refused, McpFailureKind::Protocol, exited(1)),
            StartupClass::Refused
        );

        let failure = McpClient::exited_before_initialize("fs", &["fs-server".into()], 127, "nope");
        let OutrigError::McpStartupFailed(failure) = failure else {
            panic!("a startup failure")
        };
        assert_eq!(failure.class, StartupClass::Misconfigured);
        assert_eq!(failure.kind, McpFailureKind::Transport);
        assert_eq!(failure.exit, "code 127");
    }

    #[test]
    fn resolve_mcp_env_overlay_wins_and_resolves_refs() {
        // SAFETY: test-local var; std::env::set_var is unsafe in edition 2024
        // because of thread-unsafety, acceptable in this single-purpose test.
        unsafe { std::env::set_var("OUTRIG_TEST_MCP_ENV", "from-host") };
        let config_env = BTreeMap::from([
            (
                "KEEP".to_string(),
                EnvValue::from_raw("literal".to_string()),
            ),
            ("BOTH".to_string(), EnvValue::from_raw("config".to_string())),
        ]);
        let extra_env = BTreeMap::from([
            (
                "BOTH".to_string(),
                EnvValue::from_raw("overlay".to_string()),
            ),
            (
                "REF".to_string(),
                EnvValue::from_raw("${OUTRIG_TEST_MCP_ENV}".to_string()),
            ),
        ]);

        let env = resolve_mcp_env("svc", config_env, &extra_env).expect("resolves");
        assert_eq!(env["KEEP"].value(), "literal");
        assert_eq!(env["BOTH"].value(), "overlay");
        assert_eq!(env["REF"].value(), "from-host");
        assert_eq!(
            env["REF"].source(),
            &EnvValue::EnvRef("OUTRIG_TEST_MCP_ENV".to_string())
        );
        assert_eq!(
            env["BOTH"].source(),
            &EnvValue::Literal("overlay".to_string())
        );
        let debug = format!("{env:?}");
        assert!(!debug.contains("from-host"), "{debug}");
    }

    /// The `-v` transcript line is written before the spawn, so it is what a
    /// server that never comes up leaves behind -- in `container.log`, and on
    /// the terminal.
    #[tokio::test(flavor = "current_thread")]
    async fn the_connect_transcript_shows_a_reference_not_its_value() {
        let dir = tempfile::tempdir().expect("tempdir");
        let log = dir.path().join("container.log");
        let transcript = Transcript::create(&log, false)
            .await
            .expect("create transcript");
        let cmd = Cmd::new("/bin/sh")
            // Some stderr, so the failure does not wait out the settle
            // ceiling for a tail that is never coming.
            .args(["-c", "echo gone >&2", "sh", "--env"])
            .arg_shown_as("TOKEN", "TOKEN=${FETCH_TOKEN}")
            .env_hidden("TOKEN", "s3cret");

        let connected = McpClient::connect_stdio_cmd(
            cmd,
            "svc",
            None,
            dir.path(),
            Some(transcript),
            None,
            &[],
            STARTUP_TIMEOUT,
        )
        .await;
        assert!(
            connected.is_err(),
            "a server that exits at once fails the handshake"
        );

        let text = std::fs::read_to_string(&log).expect("read transcript");
        assert!(text.contains("--env 'TOKEN=${FETCH_TOKEN}'"), "{text}");
        assert!(!text.contains("s3cret"), "{text}");
    }

    /// A server that refuses `initialize`, saying nothing on stderr, is
    /// refused within the bound it was given: the diagnosis waits no longer
    /// than the bound left -- and not at all for stderr from a server that
    /// has exited -- so a caller that bounds the start as tightly reads the
    /// refusal, not a start that ran out of time.
    #[tokio::test]
    async fn a_refusal_is_reported_within_its_bound() {
        let dir = tempfile::tempdir().expect("tempdir");
        let refusing = |then: &str| {
            let script = r#"read -r line
                id=$(printf '%s' "$line" | grep -o '"id":[0-9]*' | head -n 1 | cut -d: -f2)
                printf '{"jsonrpc":"2.0","id":%s,"error":{"code":-32600,"message":"no"}}\n' "$id"
                THEN"#;
            Cmd::new("sh").args(["-c", &script.replace("THEN", then)])
        };
        let bound = Duration::from_secs(1);
        // One that has exited is read at once; one still running is waited
        // on only as long as the bound left.
        for (then, within) in [
            ("exit 0", bound / 2),
            ("exec sleep 30", bound + Duration::from_millis(500)),
        ] {
            let started = std::time::Instant::now();
            let err = McpClient::connect_stdio_cmd(
                refusing(then),
                "svc",
                None,
                dir.path(),
                None,
                None,
                &[],
                bound,
            )
            .await
            .expect_err("a refused handshake does not connect");
            let elapsed = started.elapsed();
            let OutrigError::McpStartupFailed(failure) = &err else {
                panic!("expected McpStartupFailed, got {err:?}");
            };
            assert_eq!(failure.class, StartupClass::Refused, "{then}: {err}");
            assert!(elapsed < within, "{then}: diagnosed for {elapsed:?}");
        }
    }

    /// #338's repro without podman: `cat` echoes `initialize` back as if it
    /// were the server's own request and never answers it. The handshake
    /// must give up at its bound and fail the way a crashed server does --
    /// named, with what the server wrote to stderr -- rather than wait
    /// forever. The stderr line also keeps the failure from waiting out the
    /// settle ceiling for a tail that is never coming.
    #[tokio::test]
    async fn an_initialize_that_never_comes_fails_within_its_bound() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cmd = Cmd::new("sh").args(["-c", "echo listening on stdin >&2; exec cat"]);

        let started = std::time::Instant::now();
        let err = McpClient::connect_stdio_cmd(
            cmd,
            "svc",
            None,
            dir.path(),
            None,
            None,
            &["cat".to_string()],
            Duration::from_millis(200),
        )
        .await
        .expect_err("a server that never answers initialize must not connect");
        let elapsed = started.elapsed();

        let OutrigError::McpStartupFailed(failure) = &err else {
            panic!("expected McpStartupFailed, got {err:?}");
        };
        assert_eq!(failure.name, "svc");
        let cause = failure
            .source
            .downcast_ref::<McpSessionError>()
            .unwrap_or_else(|| panic!("a timeout is classified: {:?}", failure.source));
        assert_eq!(cause.kind, McpFailureKind::Timeout);
        assert!(
            failure.stderr_tail.contains("listening on stdin"),
            "the tail shows what the server said: {:?}",
            failure.stderr_tail
        );
        let rendered = err.to_string();
        assert!(
            rendered.contains(r#"mcp server "svc""#)
                && rendered.contains("no `initialize` response within 200ms"),
            "{rendered}"
        );
        assert!(
            elapsed < Duration::from_secs(10),
            "gave up after {elapsed:?}, not at its bound"
        );
    }

    /// `server` on one end of an in-memory pipe and a bare rmcp client on the
    /// other, handshake done; the task serves until the client goes away.
    pub(crate) async fn serve_in_memory<S: rmcp::Service<rmcp::RoleServer>>(
        server: S,
    ) -> (
        RunningService<RoleClient, Notices>,
        tokio::task::JoinHandle<()>,
    ) {
        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        let serving = tokio::spawn(async move {
            let running = rmcp::service::serve_server(server, server_io)
                .await
                .expect("serve in memory");
            let _ = running.waiting().await;
        });
        let client = serve_client(Notices::default(), client_io)
            .await
            .expect("connect in memory");
        (client, serving)
    }

    /// The other end of an in-memory pipe. It answers `initialize` and a
    /// `tools/call` to `quick` at once, holds every other request until the
    /// client cancels it, and reports each `notifications/cancelled` it is sent
    /// on `cancels`, by the name of the call it names.
    #[derive(Clone)]
    struct Stalling {
        calls: std::sync::Arc<std::sync::Mutex<std::collections::HashMap<RequestId, String>>>,
        cancels: tokio::sync::mpsc::UnboundedSender<String>,
    }

    impl rmcp::ServerHandler for Stalling {
        fn get_info(&self) -> rmcp::model::ServerConfig {
            rmcp::model::ServerConfig::new(
                rmcp::model::ServerCapabilities::builder()
                    .enable_tools()
                    .build(),
            )
        }

        async fn list_tools(
            &self,
            _request: Option<rmcp::model::PaginatedRequestParams>,
            ctx: rmcp::service::RequestContext<rmcp::service::RoleServer>,
        ) -> std::result::Result<rmcp::model::ListToolsResult, rmcp::ErrorData> {
            ctx.ct.cancelled().await;
            Err(rmcp::ErrorData::internal_error("cancelled", None))
        }

        async fn call_tool(
            &self,
            request: CallToolRequestParams,
            ctx: rmcp::service::RequestContext<rmcp::service::RoleServer>,
        ) -> std::result::Result<rmcp::model::CallToolResponse, rmcp::ErrorData> {
            self.calls
                .lock()
                .unwrap()
                .insert(ctx.id.clone(), request.name.to_string());
            if request.name == "quick" {
                return Ok(rmcp::model::CallToolResult::success(vec![
                    rmcp::model::ContentBlock::text("done"),
                ])
                .into());
            }
            ctx.ct.cancelled().await;
            Err(rmcp::ErrorData::internal_error("cancelled", None))
        }

        async fn on_cancelled(
            &self,
            notification: CancelledNotificationParam,
            _ctx: rmcp::service::NotificationContext<rmcp::service::RoleServer>,
        ) {
            let call = notification
                .request_id
                .and_then(|id| self.calls.lock().unwrap().get(&id).cloned())
                .unwrap_or_else(|| "an unknown request".to_string());
            let _ = self.cancels.send(call);
        }
    }

    /// An [`McpClient`] whose transport is an in-memory pipe to `server`
    /// rather than a podman child, with the directory its stderr file lives
    /// in. The client still owes a child, so it gets one that does nothing.
    async fn client_against<S: rmcp::Service<rmcp::RoleServer>>(
        server: S,
    ) -> (McpClient, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let stderr_path = dir.path().join("svc.stderr");
        std::fs::write(&stderr_path, "still indexing\n").expect("write stderr");

        let (service, _serving) = serve_in_memory(server).await;
        let child = Cmd::new("sleep")
            .arg("600")
            .spawn_owned(
                StdioSpec::bidirectional(),
                crate::process::Termination::Kill,
            )
            .expect("spawn a stand-in transport");

        let client = McpClient {
            name: "svc".to_string(),
            service,
            child: Some(child),
            stderr_path,
            call_timeout: effective_mcp_call_timeout(None, None),
        };
        (client, dir)
    }

    /// [`client_against`] a [`Stalling`] server, with the receiver for the
    /// cancels it is sent.
    async fn client_against_stalling_server() -> (
        McpClient,
        tokio::sync::mpsc::UnboundedReceiver<String>,
        tempfile::TempDir,
    ) {
        let (cancels, heard) = tokio::sync::mpsc::unbounded_channel();
        let (client, dir) = client_against(Stalling {
            calls: Default::default(),
            cancels,
        })
        .await;
        (client, heard, dir)
    }

    /// The name of the call the next cancel the server is sent names.
    async fn next_cancel(heard: &mut tokio::sync::mpsc::UnboundedReceiver<String>) -> String {
        tokio::time::timeout(Duration::from_secs(5), heard.recv())
            .await
            .expect("the server is sent a cancel")
            .expect("the server is still up")
    }

    #[tokio::test]
    async fn a_tools_list_that_never_comes_times_out_with_the_stderr_tail() {
        let (client, _heard, _dir) = client_against_stalling_server().await;

        let source = list_failure(
            client.list_tools_within(Duration::from_millis(100)).await,
            McpFailureKind::Timeout,
        );
        assert!(
            source
                .message
                .contains("no tools/list response within 100ms"),
            "{source}"
        );
    }

    /// The source of a listing that failed as `McpToolsListFailed` of `kind`,
    /// carrying the name and stderr tail [`client_against`] gave its client.
    fn list_failure(listed: Result<Vec<McpTool>>, kind: McpFailureKind) -> McpSessionError {
        let err = listed.expect_err("the listing must fail");
        let OutrigError::McpToolsListFailed {
            name,
            source,
            stderr_tail,
            ..
        } = err
        else {
            panic!("expected McpToolsListFailed, got {err:?}");
        };
        assert_eq!(name, "svc");
        assert_eq!(source.kind, kind, "{source}");
        assert!(stderr_tail.contains("still indexing"), "{stderr_tail:?}");
        source
    }

    /// The other end of an in-memory pipe for `tools/list` paging. Page `n`
    /// (counting from 1) carries one tool, `t<n>`, and the `nextCursor`
    /// `next_cursor(n)` names; `asked` counts the pages requested.
    struct Paging {
        next_cursor: fn(usize) -> Option<String>,
        asked: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }

    impl rmcp::ServerHandler for Paging {
        async fn list_tools(
            &self,
            _request: Option<PaginatedRequestParams>,
            _ctx: rmcp::service::RequestContext<rmcp::service::RoleServer>,
        ) -> std::result::Result<rmcp::model::ListToolsResult, rmcp::ErrorData> {
            let page = self.asked.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
            let tool = rmcp::model::Tool::new(format!("t{page}"), "", serde_json::Map::new());
            Ok(rmcp::model::ListToolsResult {
                next_cursor: (self.next_cursor)(page),
                ..rmcp::model::ListToolsResult::with_all_items(vec![tool])
            })
        }
    }

    /// A client against a [`Paging`] server, its listing, and how many pages
    /// the server was asked for.
    async fn list_paging(
        next_cursor: fn(usize) -> Option<String>,
    ) -> (Result<Vec<McpTool>>, usize) {
        let asked = std::sync::Arc::default();
        let (client, _dir) = client_against(Paging {
            next_cursor,
            asked: std::sync::Arc::clone(&asked),
        })
        .await;
        let listed = client.list_tools().await;
        (listed, asked.load(std::sync::atomic::Ordering::SeqCst))
    }

    #[tokio::test]
    async fn a_paged_tools_list_is_gathered_in_order() {
        let (listed, asked) = list_paging(|page| (page < 3).then(|| format!("p{page}"))).await;

        let names: Vec<String> = listed
            .expect("a listing that ends succeeds")
            .into_iter()
            .map(|tool| tool.name)
            .collect();
        assert_eq!(names, ["t1", "t2", "t3"]);
        assert_eq!(asked, 3);
    }

    /// #339's fixture: a server that hands back the cursor it was sent would
    /// be paged forever, every page's tools appended.
    #[tokio::test]
    async fn a_tools_list_that_echoes_its_cursor_fails_at_the_repeat() {
        let (listed, asked) = list_paging(|_| Some("same".to_string())).await;

        let source = list_failure(listed, McpFailureKind::Protocol);
        assert!(
            source
                .message
                .contains("tools/list page 2 handed back a nextCursor an earlier page"),
            "{source}"
        );
        assert_eq!(asked, 2, "the repeated cursor is never requested again");
    }

    /// Not just the last cursor: any earlier one closes a loop.
    #[tokio::test]
    async fn a_tools_list_whose_cursors_cycle_fails_at_the_repeat() {
        let (listed, asked) = list_paging(|page| Some(["b", "a"][page % 2].to_string())).await;

        let source = list_failure(listed, McpFailureKind::Protocol);
        assert!(source.message.contains("tools/list page 3 "), "{source}");
        assert_eq!(asked, 3);
    }

    /// A fresh cursor every page repeats nothing, so the ceiling is what ends
    /// it.
    #[tokio::test]
    async fn a_tools_list_that_never_ends_fails_at_the_page_ceiling() {
        let (listed, asked) = list_paging(|page| Some(format!("p{page}"))).await;

        let source = list_failure(listed, McpFailureKind::Protocol);
        assert!(
            source
                .message
                .contains("tools/list was still paging after 1000 pages"),
            "{source}"
        );
        assert_eq!(asked, TOOLS_LIST_PAGE_CEILING);
    }

    /// The seam a tolerant listing stands on. rmcp's typed decode of a
    /// `tools/list` page fails the whole page for one tool whose `inputSchema`
    /// is not an object; sent through `send_request_with_option`, such a page
    /// arrives whole as `CustomResult`, the last of `ServerResult`'s untagged
    /// variants. One that also carries `_meta` arrives as a `CallToolResult`
    /// instead, its tools lost: that variant's decoder takes any object holding
    /// one of its fields, and it comes first.
    #[tokio::test]
    async fn a_page_the_typed_decode_refuses_arrives_whole() {
        use crate::harness::mcp_placement::fake::{Raw, Toolbox};

        let tools = serde_json::json!([
            {"name": "bad", "inputSchema": []},
            {"name": "good", "inputSchema": {"type": "object"}},
        ]);
        let toolbox = Toolbox::new(tools.clone());
        let (client, _serving) = serve_in_memory(Raw(std::sync::Arc::clone(&toolbox))).await;
        let ask = || async {
            client
                .send_request_with_option(
                    ClientRequest::ListToolsRequest(Default::default()),
                    PeerRequestOptions::no_options(),
                )
                .await
                .expect("sent")
                .await_response()
                .await
                .expect("answered")
        };

        let ServerResult::CustomResult(page) = ask().await else {
            panic!("a page the typed decode refuses is a custom result");
        };
        assert_eq!(page.0["tools"], tools);

        toolbox.page_as(serde_json::json!({"tools": tools, "_meta": {"k": 1}}));
        assert!(matches!(ask().await, ServerResult::CallToolResult(_)));
    }

    /// A malformed tool costs a session's listing that tool alone, named with why; the facade's
    /// listing still fails for it, now naming it; and a page whose `_meta` hides its tools from
    /// the seam fails as a response of the wrong kind, as before.
    #[tokio::test]
    async fn a_malformed_tool_costs_the_tolerant_listing_that_tool() {
        use crate::harness::mcp_placement::fake::{Raw, Toolbox};

        let tools = serde_json::json!([
            {"name": "first", "inputSchema": {"type": "object"}},
            {"name": "bad", "inputSchema": []},
            {"inputSchema": 7},
            {"name": "last", "inputSchema": {"type": "object"}},
        ]);
        let toolbox = Toolbox::new(tools.clone());
        let (client, _dir) = client_against(Raw(std::sync::Arc::clone(&toolbox))).await;

        let Listing {
            tools: kept,
            undecoded,
        } = client
            .list_tools_tolerant_within(STARTUP_TIMEOUT)
            .await
            .expect("the listing is kept");
        let names: Vec<&str> = kept.iter().map(|tool| tool.name.as_str()).collect();
        assert_eq!(names, ["first", "last"]);
        let named: Vec<Option<&str>> = undecoded.iter().map(|u| u.name.as_deref()).collect();
        assert_eq!(named, [Some("bad"), None]);
        assert!(
            undecoded[0].reason.contains("invalid type"),
            "{undecoded:?}"
        );

        let source = list_failure(client.list_tools().await, McpFailureKind::Protocol);
        assert!(
            source.message.contains("cannot decode")
                && source.message.contains("\"bad\": invalid type")
                && source.message.contains("a tool with no name"),
            "{source}"
        );

        toolbox.page_as(serde_json::json!({"tools": tools, "_meta": {"k": 1}}));
        let Err(OutrigError::McpToolsListFailed { source, .. }) =
            client.list_tools_tolerant_within(STARTUP_TIMEOUT).await
        else {
            panic!("a page decoded as another result fails the listing");
        };
        assert_eq!(source.kind, McpFailureKind::Protocol, "{source}");
    }

    /// Past its deadline a call fails as a timeout *and* the server hears
    /// about it -- the work it was doing is for an answer nobody will read.
    #[tokio::test]
    async fn a_tools_call_past_its_deadline_is_cancelled_at_the_server() {
        let (client, mut heard, _dir) = client_against_stalling_server().await;
        let client = client.with_call_timeout(Duration::from_millis(100));

        let err = client
            .call_tool("slow", Value::Null)
            .await
            .expect_err("a call past its deadline must fail");

        let OutrigError::McpCallToolFailed { source, .. } = &err else {
            panic!("expected McpCallToolFailed, got {err:?}");
        };
        assert_eq!(source.kind, McpFailureKind::Timeout);
        assert!(
            source
                .message
                .contains("no tools/call response within 100ms"),
            "{source}"
        );
        assert_eq!(next_cancel(&mut heard).await, "slow");
    }

    /// The drop guard: a call abandoned by its caller -- a Ctrl-C mid-turn, a
    /// proxied client's cancel -- reaches the server as a cancel too, long
    /// before its own deadline would have.
    #[tokio::test]
    async fn a_dropped_tools_call_is_cancelled_at_the_server() {
        let (client, mut heard, _dir) = client_against_stalling_server().await;

        let abandoned = tokio::time::timeout(
            Duration::from_millis(100),
            client.call_tool("slow", Value::Null),
        )
        .await;
        assert!(abandoned.is_err(), "the call should still be pending");

        assert_eq!(next_cancel(&mut heard).await, "slow");
    }

    /// And an answered call owes nothing: the guard is disarmed. Proven by
    /// order rather than by waiting on silence -- a cancel for `quick` would
    /// cross the pipe before the one a later dropped call earns.
    #[tokio::test]
    async fn an_answered_tools_call_sends_no_cancel() {
        let (client, mut heard, _dir) = client_against_stalling_server().await;

        let result = client
            .call_tool("quick", Value::Null)
            .await
            .expect("an answered call succeeds");
        assert_eq!(result.render_text(), "done");

        let _ = tokio::time::timeout(
            Duration::from_millis(100),
            client.call_tool("slow", Value::Null),
        )
        .await;
        assert_eq!(
            next_cancel(&mut heard).await,
            "slow",
            "the first cancel is for the dropped call, not the answered one"
        );
    }

    #[test]
    fn resolve_mcp_env_missing_ref_names_server_and_key() {
        let config_env = BTreeMap::from([(
            "TOKEN".to_string(),
            EnvValue::from_raw("${OUTRIG_TEST_MCP_ENV_MISSING}".to_string()),
        )]);
        let err = resolve_mcp_env("svc", config_env, &BTreeMap::new())
            .expect_err("unset host var must fail resolution");
        let OutrigError::McpEnvResolveFailed { name, key, .. } = &err else {
            panic!("expected McpEnvResolveFailed, got {err:?}");
        };
        assert_eq!(name, "svc");
        assert_eq!(key, "TOKEN");
    }

    #[test]
    fn render_command_passes_through_simple_argv() {
        let argv = vec![
            "mcp-server-git".to_string(),
            "--repository".to_string(),
            "/workspace".to_string(),
        ];
        assert_eq!(
            render_command(&argv),
            "mcp-server-git --repository /workspace"
        );
    }

    #[test]
    fn render_command_quotes_args_with_whitespace_or_specials() {
        let argv = vec![
            "echo".to_string(),
            "hello world".to_string(),
            "it's".to_string(),
            "".to_string(),
        ];
        assert_eq!(render_command(&argv), "echo 'hello world' 'it'\\''s' ''");
    }

    #[tokio::test]
    async fn read_stderr_tail_handles_empty_missing_and_long() {
        let dir = tempfile::tempdir().unwrap();

        let missing = dir.path().join("nope.stderr");
        assert_eq!(
            read_stderr_tail_settling_for(&missing, Duration::ZERO).await,
            "(could not read stderr file)"
        );

        let empty = dir.path().join("empty.stderr");
        tokio::fs::write(&empty, b"").await.unwrap();
        assert_eq!(
            read_stderr_tail_settling_for(&empty, Duration::ZERO).await,
            "(empty)"
        );

        let normal = dir.path().join("normal.stderr");
        tokio::fs::write(&normal, b"line one\nline two\n")
            .await
            .unwrap();
        assert_eq!(
            read_stderr_tail_settling_for(&normal, Duration::ZERO).await,
            "line one\nline two"
        );

        let big = dir.path().join("big.stderr");
        let payload: Vec<u8> = (0..10_000).map(|i| b'A' + (i % 26) as u8).collect();
        tokio::fs::write(&big, &payload).await.unwrap();
        let tail = read_stderr_tail_settling_for(&big, Duration::ZERO).await;
        assert!(tail.len() <= 2048, "tail was {} bytes", tail.len());
        assert!(payload.ends_with(tail.trim_end().as_bytes()));
    }

    /// The `tools/list` failure path reads the stderr of a server that is still
    /// running, so the read itself is capped: a writer appending throughout
    /// cannot stretch the "tail" into the megabytes it wrote meanwhile.
    #[tokio::test]
    async fn a_tail_read_while_the_server_writes_stays_capped() {
        use std::io::Write;
        use std::sync::atomic::{AtomicBool, Ordering};

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("live.stderr");
        std::fs::write(&path, vec![b'a'; 10_000]).unwrap();

        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let writer = std::thread::spawn({
            let (path, stop) = (path.clone(), stop.clone());
            move || {
                let mut file = std::fs::OpenOptions::new()
                    .append(true)
                    .open(&path)
                    .unwrap();
                // Bounded, so a fast disk cannot fill up before the reads end.
                for _ in 0..4096 {
                    if stop.load(Ordering::Relaxed) {
                        break;
                    }
                    file.write_all(&[b'b'; 4096]).unwrap();
                }
            }
        });

        for _ in 0..50 {
            let tail = read_stderr_bytes(&path).await.unwrap();
            assert!(tail.len() <= 2048, "read {} bytes", tail.len());
        }
        stop.store(true, Ordering::Relaxed);
        writer.join().unwrap();
    }

    /// A client that is still running can still write the file, so an empty
    /// read is not the answer yet. (This was added for #422's `(empty)`, which
    /// turned out to be bytes podman dropped rather than bytes that were late:
    /// see `restore_lost_stderr`.)
    #[tokio::test]
    async fn a_tail_waits_for_a_writer_that_has_not_landed_yet() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("late.stderr");
        tokio::fs::write(&path, b"").await.unwrap();

        let late = tokio::spawn({
            let path = path.clone();
            async move {
                tokio::time::sleep(Duration::from_millis(150)).await;
                tokio::fs::write(
                    &path,
                    b"exec: \"nope\": executable file not found in $PATH\n",
                )
                .await
                .unwrap();
            }
        });

        let tail = read_stderr_tail_settling_for(&path, Duration::from_secs(5)).await;
        assert!(
            tail.contains("executable file not found"),
            "a late write must still reach the tail, got: {tail:?}"
        );
        late.await.unwrap();
    }

    /// And it still gives up: a server that really said nothing must not hold
    /// the error path open for the whole ceiling's worth of nothing.
    #[tokio::test]
    async fn a_tail_gives_up_on_a_file_that_stays_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("silent.stderr");
        tokio::fs::write(&path, b"").await.unwrap();

        let started = std::time::Instant::now();
        let tail = read_stderr_tail_settling_for(&path, Duration::from_millis(50)).await;
        assert_eq!(tail, "(empty)");
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "giving up took {:?}",
            started.elapsed()
        );
    }

    #[tokio::test]
    async fn enrich_startup_error_carries_name_command_and_stderr() {
        // Spawn `false` (fast, deterministic non-zero exit) with stderr piped
        // into a temp file, then drive the helper as if rmcp had returned EOF.
        let dir = tempfile::tempdir().unwrap();
        let stderr_path = dir.path().join("svc.stderr");
        let stderr_file = tokio::fs::File::create(&stderr_path).await.unwrap();

        let mut child = Cmd::new("sh")
            .args(["-c", "echo boom 1>&2; exit 7"])
            .spawn_owned(
                StdioSpec::bidirectional().with_stderr(Stdio::from(stderr_file.into_std().await)),
                crate::process::Termination::Kill,
            )
            .unwrap();

        let argv = vec![
            "sh".to_string(),
            "-c".to_string(),
            "echo boom 1>&2; exit 7".to_string(),
        ];
        let source = Handshake::failed(rmcp::service::ClientInitializeError::ConnectionClosed(
            "expect initialize response".to_string(),
        ));

        let err = enrich_startup_error(
            "svc",
            Some("launch spec"),
            &argv,
            &stderr_path,
            None,
            &mut child,
            source,
            None,
        )
        .await;

        let OutrigError::McpStartupFailed(payload) = &err else {
            panic!("expected McpStartupFailed, got {err:?}");
        };
        assert_eq!(payload.name, "svc");
        assert_eq!(payload.declaration_source.as_deref(), Some("launch spec"));
        assert!(payload.command.contains("sh"));
        assert_eq!(payload.exit, "code 7");
        assert!(
            payload.stderr_tail.contains("boom"),
            "stderr_tail={:?}",
            payload.stderr_tail
        );

        let display = err.to_string();
        assert!(display.contains("svc"));
        assert!(display.contains("from launch spec"));
        assert!(display.contains("expect initialize response"));
        assert!(display.contains("code 7"));
        assert!(display.contains("boom"));
    }

    /// An empty `<name>.stderr` for a child to be handed, and the child: `sh -c <script>`
    /// with that file as its stderr.
    async fn silent_child_with_stderr_file(
        dir: &Path,
        script: &str,
    ) -> (Owned, PathBuf, Vec<String>) {
        let stderr_path = dir.join("svc.stderr");
        let stderr_file = tokio::fs::File::create(&stderr_path).await.unwrap();
        let child = Cmd::new("sh")
            .args(["-c", script])
            .spawn_owned(
                StdioSpec::bidirectional().with_stderr(Stdio::from(stderr_file.into_std().await)),
                crate::process::Termination::Kill,
            )
            .unwrap();
        let argv = vec!["sh".to_string(), "-c".to_string(), script.to_string()];
        (child, stderr_path, argv)
    }

    fn connection_closed() -> Handshake {
        Handshake::failed(rmcp::service::ClientInitializeError::ConnectionClosed(
            "initialize response".to_string(),
        ))
    }

    /// #422: the container printed one line and exited, `podman start --attach` returned its
    /// exit code and none of the line. The line is in the container's log.
    #[tokio::test]
    async fn a_start_whose_attach_stream_was_empty_reports_what_the_container_log_holds() {
        let dir = tempfile::tempdir().unwrap();
        let (mut child, stderr_path, argv) =
            silent_child_with_stderr_file(dir.path(), "exit 1").await;
        let log = log_holding(
            &[
                entry("stdout", "F", r#"{"jsonrpc":"2.0","id":0,"error":{}}"#),
                entry("stderr", "P", "outrig-enter: open outrig-no-such-program"),
                entry(
                    "stderr",
                    "F",
                    ": No such file or directory (not found in PATH=/usr/bin)",
                ),
            ]
            .concat(),
        );

        let started = std::time::Instant::now();
        let err = enrich_startup_error(
            "svc",
            None,
            &argv,
            &stderr_path,
            Some(log.path()),
            &mut child,
            connection_closed(),
            None,
        )
        .await;

        // Not the settle's five seconds: nothing is going to land late.
        assert!(
            started.elapsed() < STDERR_SETTLE_CEILING / 2,
            "took {:?}",
            started.elapsed()
        );
        let OutrigError::McpStartupFailed(failure) = &err else {
            panic!("expected McpStartupFailed, got {err:?}");
        };
        assert_eq!(failure.exit, "code 1");
        let expected = "outrig-enter: open outrig-no-such-program: No such file or directory \
                        (not found in PATH=/usr/bin)";
        assert_eq!(failure.stderr_tail, expected);
        assert!(err.to_string().contains(expected), "{err}");
        // And `outrig logs` reads the same thing.
        assert_eq!(
            std::fs::read_to_string(&stderr_path).unwrap(),
            format!("{expected}\n")
        );
    }

    /// The log is for what the attach stream lost, not a second opinion on what it carried.
    #[tokio::test]
    async fn stderr_the_attach_stream_carried_is_not_added_to() {
        let dir = tempfile::tempdir().unwrap();
        let (mut child, stderr_path, argv) =
            silent_child_with_stderr_file(dir.path(), "echo from-attach >&2; exit 1").await;
        let log = log_holding(&entry("stderr", "F", "from-the-log"));

        let err = enrich_startup_error(
            "svc",
            None,
            &argv,
            &stderr_path,
            Some(log.path()),
            &mut child,
            connection_closed(),
            None,
        )
        .await;

        let OutrigError::McpStartupFailed(failure) = &err else {
            panic!("expected McpStartupFailed, got {err:?}");
        };
        assert_eq!(failure.stderr_tail, "from-attach");
        assert_eq!(
            std::fs::read_to_string(&stderr_path).unwrap(),
            "from-attach\n"
        );
    }

    /// A container that wrote only its protocol stream has nothing to restore.
    #[tokio::test]
    async fn a_container_log_without_stderr_restores_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let stderr_path = dir.path().join("svc.stderr");
        std::fs::write(&stderr_path, b"").unwrap();
        let log = log_holding(&entry("stdout", "F", "protocol only"));

        restore_lost_stderr("svc", &stderr_path, log.path()).await;

        assert_eq!(std::fs::read_to_string(&stderr_path).unwrap(), "");
    }

    /// While the client may still be writing, the file is its to fill. A handshake that timed out
    /// against a server that is still running is that case.
    #[tokio::test(start_paused = true)]
    async fn a_client_that_has_not_exited_has_its_file_left_to_it() {
        let dir = tempfile::tempdir().unwrap();
        let (mut child, stderr_path, argv) =
            silent_child_with_stderr_file(dir.path(), "exec sleep 600").await;
        let log = log_holding(&entry("stderr", "F", "from-the-log"));

        let err = enrich_startup_error(
            "svc",
            None,
            &argv,
            &stderr_path,
            Some(log.path()),
            &mut child,
            connection_closed(),
            None,
        )
        .await;

        let OutrigError::McpStartupFailed(failure) = &err else {
            panic!("expected McpStartupFailed, got {err:?}");
        };
        assert_eq!(failure.exit, "still running (wait timed out)");
        assert_eq!(failure.stderr_tail, "(empty)");
        assert_eq!(std::fs::read_to_string(&stderr_path).unwrap(), "");
    }

    /// The whole path inside `McpClient`: a transport that exits at once with nothing on its
    /// stderr, and a log.
    #[tokio::test]
    async fn a_connect_that_fails_at_once_reports_the_container_log() {
        let dir = tempfile::tempdir().unwrap();
        let log = log_holding(&entry("stderr", "F", "last words"));
        let cmd = Cmd::new("sh").args(["-c", "exit 1"]);

        let started = std::time::Instant::now();
        let err = McpClient::connect_stdio_cmd(
            cmd,
            "svc",
            None,
            dir.path(),
            None,
            Some(log.path()),
            &["sh".to_string()],
            STARTUP_TIMEOUT,
        )
        .await
        .expect_err("a server that exits at once does not connect");

        assert!(
            started.elapsed() < STDERR_SETTLE_CEILING / 2,
            "{:?}",
            started.elapsed()
        );
        let OutrigError::McpStartupFailed(failure) = &err else {
            panic!("expected McpStartupFailed, got {err:?}");
        };
        assert_eq!(failure.stderr_tail, "last words");
    }

    #[test]
    fn format_exit_renders_codes_signals_and_unknown() {
        use std::os::unix::process::ExitStatusExt;
        use std::process::ExitStatus;

        assert_eq!(format_exit(Some(ExitStatus::from_raw(0))), "code 0");
        assert_eq!(format_exit(Some(ExitStatus::from_raw(2 << 8))), "code 2");
        // Raw status with no exit code but a signal in the low 7 bits.
        assert_eq!(format_exit(Some(ExitStatus::from_raw(9))), "signal 9");
        assert_eq!(format_exit(None), "still running (wait timed out)");
    }
}
