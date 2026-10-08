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
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use rmcp::model::{
    CallToolRequest, CallToolRequestParams, CancelledNotificationParam, ClientRequest,
    PaginatedRequestParams, RequestId, ServerResult,
};
use rmcp::service::{
    Peer, PeerRequestOptions, RoleClient, RunningService, ServiceError, serve_client,
};
use serde_json::Value;

use crate::config::{EnvValue, McpServerSpec, ResolvedEnvValue, effective_mcp_call_timeout};
use crate::container::{Container, ExecOptions, embedded::McpDeclarationSource};
use crate::error::{IoPathExt, McpFailureKind, McpSessionError, OutrigError, Result};
use crate::mcp_content::{McpTool, McpToolResult, result_from_rmcp, tool_from_rmcp};
use crate::process::{Cmd, Owned, StdioSpec, Transcript};

const SHUTDOWN_GRACE: Duration = Duration::from_secs(2);

/// How long a server has to answer `initialize`, and then again to finish
/// answering `tools/list`, every page included. The `initialize` clock starts
/// at the spawn of the transport, so it covers `podman exec` or `podman start
/// --attach` as well as the server's own start, and a server that fetches
/// itself on first launch (`npx -y`, `uvx`) spends its download inside it too
/// -- which is why it is minutes rather than seconds. Past it the server is
/// presumed hung, and startup fails the way it does for a server that crashed.
const STARTUP_TIMEOUT: Duration = Duration::from_secs(120);

/// How many `tools/list` pages a server may send before its listing is
/// refused. No server a model could use comes near it -- most send every tool
/// on one page -- so one still paging here is minting cursors, not listing
/// tools, and would otherwise grow the listing until [`STARTUP_TIMEOUT`].
const TOOLS_LIST_PAGE_CEILING: usize = 1000;

/// The `reason` a dropped call's `notifications/cancelled` carries.
const ABANDONED_REASON: &str = "outrig stopped waiting for this call";

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
    service: RunningService<RoleClient, ()>,
    child: Owned,
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
        let env = resolve_mcp_env_values(name, env_spec, extra_env)?;
        // Sidecar servers run wherever their image puts them; no exec-side
        // working directory has come up for one yet.
        let exec_cmd =
            container.build_exec_argv(&command, &ExecOptions::new().with_resolved_env(env));
        let client = Self::connect_stdio_cmd(
            exec_cmd,
            name,
            declaration_source,
            log_dir,
            container.transcript(),
            &command,
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
        let cmd = Cmd::new("podman")
            .args(["start", "--attach", "--interactive"])
            .arg(container.name());
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
            Some(declaration_source.description()),
            log_dir,
            container.transcript(),
            &argv,
            STARTUP_TIMEOUT,
        )
        .await
    }

    /// Transport-agnostic connection tail: spawn `cmd` with piped stdio and
    /// stderr redirected to `<log_dir>/<name>.stderr`, then drive the MCP
    /// `initialize` handshake, giving the server `initialize_within` to
    /// answer. `display_command` is what a startup failure reports as the
    /// attempted command.
    async fn connect_stdio_cmd(
        cmd: Cmd,
        name: &str,
        declaration_source: Option<&'static str>,
        log_dir: &Path,
        transcript: Option<Transcript>,
        display_command: &[String],
        initialize_within: Duration,
    ) -> Result<Self> {
        tokio::fs::create_dir_all(log_dir)
            .await
            .path_ctx("create directory", log_dir)?;
        let stderr_path = log_dir.join(format!("{name}.stderr"));
        let stderr_file = tokio::fs::File::create(&stderr_path)
            .await
            .path_ctx("create", &stderr_path)?;
        let stderr_std = stderr_file.into_std().await;

        if let Some(transcript) = transcript {
            transcript
                .line("podman", &format!("$ {}", cmd.render()))
                .await?;
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
            tokio::time::timeout(initialize_within, serve_client((), (stdout, stdin))).await;
        let source: Box<dyn std::error::Error + Send + Sync> = match handshake {
            Ok(Ok(service)) => {
                return Ok(Self {
                    name: name.to_string(),
                    service,
                    child,
                    stderr_path,
                    call_timeout: effective_mcp_call_timeout(None, None),
                });
            }
            Ok(Err(source)) => Box::new(source),
            // rmcp drives the handshake inline, so the elapsed future took the
            // transport with it: the server's stdin is closed by the time
            // `enrich_startup_error` waits on it.
            Err(_) => Box::new(McpSessionError::new(
                McpFailureKind::Timeout,
                format!("no `initialize` response within {initialize_within:?}"),
            )),
        };
        Err(enrich_startup_error(
            name,
            declaration_source,
            display_command,
            &stderr_path,
            &mut child,
            source,
        )
        .await)
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

    /// Issue an MCP `tools/list`, following its pages until the server stops
    /// sending a `nextCursor`, and translate the results into our own
    /// [`McpTool`] type. Every page together gets the same fixed bound
    /// `initialize` does, 120 seconds; a listing that outlives it fails with
    /// [`McpFailureKind::Timeout`]. One whose pages would never end -- a
    /// `nextCursor` an earlier page already sent, or a server still paging
    /// after 1000 pages -- fails at once with [`McpFailureKind::Protocol`]. A
    /// failure carries the server's stderr tail.
    pub async fn list_tools(&self) -> Result<Vec<McpTool>> {
        self.list_tools_within(STARTUP_TIMEOUT).await
    }

    /// [`Self::list_tools`] with the bound named, so a test can reach the
    /// timeout without waiting out the real one. No cancel goes upstream on
    /// expiry: every caller abandons a server whose listing failed.
    async fn list_tools_within(&self, bound: Duration) -> Result<Vec<McpTool>> {
        let source = match tokio::time::timeout(bound, self.list_every_page()).await {
            Ok(Ok(tools)) => return Ok(tools.into_iter().map(tool_from_rmcp).collect()),
            Ok(Err(source)) => source,
            Err(_) => McpSessionError::new(
                McpFailureKind::Timeout,
                format!("no tools/list response within {bound:?}"),
            ),
        };
        Err(OutrigError::McpToolsListFailed {
            name: self.name.clone(),
            source,
            stderr_path: self.stderr_path.clone(),
            // Read once: the server may still be running, and the settle wait
            // exists for a writer that has just exited.
            stderr_tail: read_stderr_tail_settling_for(&self.stderr_path, Duration::ZERO).await,
        })
    }

    /// Every page of `tools/list`, in order. rmcp's `list_all_tools` pages
    /// until the server omits `nextCursor`, so a server that echoes the cursor
    /// it was sent would be paged forever. Here a cursor any earlier page
    /// already sent -- the echo, or a longer cycle -- is refused before it is
    /// requested again, and so is a listing past [`TOOLS_LIST_PAGE_CEILING`].
    /// Neither message quotes the cursor: the server chose it.
    async fn list_every_page(
        &self,
    ) -> std::result::Result<Vec<rmcp::model::Tool>, McpSessionError> {
        let mut tools = Vec::new();
        let mut seen = HashSet::new();
        let mut cursor = None;
        for page in 1..=TOOLS_LIST_PAGE_CEILING {
            let listed = self
                .service
                .list_tools(Some(PaginatedRequestParams::default().with_cursor(cursor)))
                .await
                .map_err(|source| session_error_from_rmcp(&source))?;
            tools.extend(listed.tools);
            let Some(next) = listed.next_cursor else {
                return Ok(tools);
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

        let mut params = CallToolRequestParams::new(name.to_string());
        if let Some(arguments) = arguments {
            params = params.with_arguments(arguments);
        }
        let handle = self
            .service
            .send_request_with_option(
                ClientRequest::CallToolRequest(CallToolRequest::new(params)),
                PeerRequestOptions::no_options(),
            )
            .await
            .map_err(|source| OutrigError::McpService(session_error_from_rmcp(&source)))?;
        // The deadline is ours rather than rmcp's: on expiry the dropped wait
        // leaves the guard armed, and it sends the cancel -- the same path a
        // caller's own drop takes, so there is one sender and one reason.
        let guard = CancelOnDrop::new(handle.peer.clone(), handle.id.clone());
        let bound = self.call_timeout;
        let answered = tokio::time::timeout(bound, handle.await_response())
            .await
            .map_err(|_| {
                // Read by the model as often as by a person, so it says what
                // happened to the work and where a longer deadline is set.
                OutrigError::McpService(McpSessionError::new(
                    McpFailureKind::Timeout,
                    format!(
                        "no tools/call response within {bound:?}, so the call was cancelled; \
                         the server's `call-timeout-secs` sets how long its calls may run"
                    ),
                ))
            })?;
        guard.disarm();
        match answered
            .map_err(|source| OutrigError::McpService(session_error_from_rmcp(&source)))?
        {
            ServerResult::CallToolResult(result) => Ok(result_from_rmcp(result)),
            _ => Err(OutrigError::McpService(session_error_from_rmcp(
                &ServiceError::UnexpectedResponse,
            ))),
        }
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
        let Self {
            service, mut child, ..
        } = self;

        // A `JoinError` here just means the rmcp task panicked, which doesn't
        // affect our ability to clean up the child below. Bound cancellation
        // too: if the owning container disappeared, the transport task can be
        // wedged behind already-broken podman exec pipes.
        let _ = tokio::time::timeout(SHUTDOWN_GRACE, service.cancel()).await;

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
}

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        let Some((peer, id)) = self.owed.take() else {
            return;
        };
        // `Drop` cannot await, so the notice goes on a task of its own -- and
        // spawning outside a runtime panics, so a drop during runtime teardown,
        // with no one left to deliver it, owes nothing. Bounded like the rest
        // of the transport's goodbyes: a wedged pipe must not keep it alive.
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        runtime.spawn(async move {
            let notice =
                CancelledNotificationParam::new(Some(id), Some(ABANDONED_REASON.to_string()));
            let _ = tokio::time::timeout(SHUTDOWN_GRACE, peer.notify_cancelled(notice)).await;
        });
    }
}

/// Layer the CLI `--env` overlay onto the config-file env (overlay wins on
/// key conflict) and resolve each value -- literals verbatim, `${VAR}` refs
/// from the host environment. `name` labels resolution failures with the
/// owning server.
///
/// The values come back as plain strings, which podman is handed and every
/// diagnostic shows as written. For an environment headed to podman, use
/// [`resolve_mcp_env_values`], which keeps each reference out of both.
pub fn resolve_mcp_env(
    name: &str,
    config_env: BTreeMap<String, EnvValue>,
    extra_env: &BTreeMap<String, EnvValue>,
) -> Result<BTreeMap<String, String>> {
    Ok(resolve_mcp_env_values(name, config_env, extra_env)?
        .into_iter()
        .map(|(key, value)| (key, value.into_value()))
        .collect())
}

/// [`resolve_mcp_env`], keeping what each value was resolved from. Shared by
/// both stdio transports: exec-stdio resolves at connect time (`podman exec
/// --env`), entrypoint-stdio at container-create time (`podman create
/// --env`), and each hands the result to `with_resolved_env`, so a `${VAR}`
/// value reaches podman by name and is shown as the reference.
pub fn resolve_mcp_env_values(
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
/// before we read them.
async fn enrich_startup_error(
    name: &str,
    declaration_source: Option<&str>,
    command: &[String],
    stderr_path: &Path,
    child: &mut Owned,
    source: Box<dyn std::error::Error + Send + Sync>,
) -> OutrigError {
    let exit_status = match tokio::time::timeout(CHILD_EXIT_CEILING, child.wait()).await {
        Ok(Ok(status)) => Some(status),
        _ => None,
    };
    let exit = format_exit(exit_status);
    let stderr_tail = read_stderr_tail_settled(stderr_path).await;

    let what = match exit_status {
        Some(status) if status.success() => None,
        Some(_) => Some(format!("terminated before initialize ({exit})")),
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

    OutrigError::McpStartupFailed(Box::new(crate::error::McpStartupFailure {
        name: name.to_string(),
        declaration_source: declaration_source.map(str::to_string),
        command: render_command(command),
        exit,
        stderr_path: stderr_path.to_path_buf(),
        stderr_tail,
        source,
    }))
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

/// How long [`read_stderr_tail_settled`] keeps looking for stderr that has not
/// landed yet.
///
/// The bytes a reader needs are not necessarily written by the process that
/// just exited: for a container whose entrypoint cannot be resolved, the
/// message comes from the OCI runtime by way of conmon, whose write is not
/// ordered against `podman start --attach` returning. Reading once wins on an
/// idle machine and loses on a loaded one -- CI reported `exit: code 1` beside
/// a `(empty)` tail for exactly that container, which is the one thing the
/// field exists to prevent.
///
/// Paid only on a failure path, and only while the file is *still* empty: a
/// server that genuinely said nothing waits this out once, on its way to an
/// error it was going to return regardless.
const STDERR_SETTLE_CEILING: Duration = Duration::from_secs(5);

/// How often [`read_stderr_tail_settled`] re-reads while it waits.
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

/// The tail of `path`, giving a writer that has not caught up a bounded chance
/// to land. See [`STDERR_SETTLE_CEILING`] for why one read is not enough.
async fn read_stderr_tail_settled(path: &Path) -> String {
    read_stderr_tail_settling_for(path, STDERR_SETTLE_CEILING).await
}

/// The above with the ceiling named, so a test can assert the giving-up branch
/// without waiting out the real one.
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

fn render_command(argv: &[String]) -> String {
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
    // Review this mapping on every rmcp bump, alongside
    // `SUPPORTED_PROTOCOL_VERSIONS` -- `plan/next/rmcp-list-result-spec-gaps.md`
    // carries both obligations.
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

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

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

        let env = resolve_mcp_env("svc", config_env.clone(), &extra_env).expect("resolves");
        assert_eq!(env["KEEP"], "literal");
        assert_eq!(env["BOTH"], "overlay");
        assert_eq!(env["REF"], "from-host");

        let values = resolve_mcp_env_values("svc", config_env, &extra_env).expect("resolves");
        assert_eq!(
            values["REF"].source(),
            &EnvValue::EnvRef("OUTRIG_TEST_MCP_ENV".to_string())
        );
        assert_eq!(
            values["BOTH"].source(),
            &EnvValue::Literal("overlay".to_string())
        );
        let debug = format!("{values:?}");
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
    pub(crate) async fn serve_in_memory<S: rmcp::ServerHandler>(
        server: S,
    ) -> (RunningService<RoleClient, ()>, tokio::task::JoinHandle<()>) {
        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        let serving = tokio::spawn(async move {
            let running = rmcp::service::serve_server(server, server_io)
                .await
                .expect("serve in memory");
            let _ = running.waiting().await;
        });
        let client = serve_client((), client_io)
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
    async fn client_against<S: rmcp::ServerHandler>(server: S) -> (McpClient, tempfile::TempDir) {
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
            child,
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

        let OutrigError::McpService(source) = &err else {
            panic!("expected McpService, got {err:?}");
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

    /// The defect this exists for: the process that exits and the process that
    /// writes the useful stderr are not always the same one. For a container
    /// whose entrypoint cannot be resolved, `podman start --attach` returns
    /// while the OCI runtime's message is still on its way through conmon, and
    /// a single read gets an empty file.
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
        let source = Box::new(rmcp::service::ClientInitializeError::ConnectionClosed(
            "expect initialize response".to_string(),
        ));

        let err = enrich_startup_error(
            "svc",
            Some("launch spec"),
            &argv,
            &stderr_path,
            &mut child,
            source,
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
