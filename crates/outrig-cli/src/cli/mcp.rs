//! `outrig mcp` orchestrator: wire the shared `SessionSetup` bootstrap to a
//! [`ProxyServer`] served over rmcp's stdio transport by default, or
//! Streamable HTTP when `--listen` is set. Runs as a server (not a REPL):
//! the external stdio client speaks JSON-RPC on the binary's stdout, and
//! everything else (banner, tracing) goes to stderr.
//!
//! The exit triggers -- stdio stdin EOF (peer disconnect), SIGINT, SIGTERM,
//! and attached-container stop -- all funnel through the same teardown order
//! as `outrig run`: cancel the rmcp service so its dispatcher quiesces ->
//! `McpClient::shutdown` per backing server -> `Container::stop` ->
//! `SessionStore::finalize`. Backing MCPs are `podman exec` processes whose
//! pipes ride through the container, so tearing the container down before
//! stopping the rmcp service races them.

#![deny(clippy::print_stdout)]

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::future::IntoFuture;
use std::io::Write as _;
use std::net::SocketAddr;
#[cfg(unix)]
use std::os::unix::fs::FileTypeExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use clap::{ArgAction, Parser, Subcommand};
use rmcp::transport::streamable_http_server::{
    SessionManager, StreamableHttpServerConfig, StreamableHttpService,
    session::local::LocalSessionManager,
};
use serde::Serialize;
use tokio::signal::unix::{SignalKind, signal};
use tokio_util::sync::CancellationToken;

use crate::cli::env_arg::CliEnvEntries;
use crate::cli::session_setup::{self, SessionSetup, SessionSetupArgs};
use crate::error::{OutrigError, Result};
use outrig::config::{ContainerConfig, McpServerSpec, NetworkMode};
use outrig::container::Container;
use outrig::image::ImageTag;
use outrig::mcp::McpClient;
use outrig::mcp_proxy::ProxyServer;

const ATTACH_MONITOR_SHUTDOWN_GRACE: Duration = Duration::from_secs(2);
const HTTP_SESSION_SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ListenAddr {
    Tcp(SocketAddr),
    Unix(PathBuf),
}

#[derive(Debug, Parser)]
pub struct McpArgs {
    #[command(subcommand)]
    pub cmd: Option<McpCommand>,

    /// Pick a `[containers.<name>]` block. Falls back to top-level
    /// `default-container` only -- `outrig mcp` has no agent, so there is no
    /// `agent.container` to consult.
    #[arg(long, global = true, value_name = "NAME")]
    pub container: Option<String>,

    /// Write the session into an explicit, already-existing directory. The
    /// session root gets a symlink at `<root>/<sid>` pointing at this path.
    #[arg(long = "session-dir", global = true, value_name = "PATH")]
    pub session_dir: Option<PathBuf>,

    /// Serve MCP over Streamable HTTP at a TCP address or Unix socket
    /// (`127.0.0.1:7331`, `0.0.0.0:7331`, or `unix:/tmp/outrig.sock`).
    #[arg(long, value_name = "ADDR", value_parser = parse_listen_addr)]
    pub listen: Option<ListenAddr>,

    /// Attach to an existing outrig session id or podman container name
    /// instead of starting a fresh container.
    #[arg(long, global = true, value_name = "SESSION_OR_CONTAINER")]
    pub attach: Option<String>,

    /// Add or override env vars for MCP servers. Repeatable.
    /// `KEY=VALUE` applies to every server; `SERVER:KEY=VALUE` targets one.
    #[arg(long = "env", global = true, value_name = "KEY=VALUE", action = ArgAction::Append)]
    pub env: Vec<String>,

    /// Override network monitoring for this session.
    #[arg(long = "network", global = true, value_name = "MODE", value_parser = parse_network_mode)]
    pub network: Option<NetworkMode>,
}

#[derive(Debug, Subcommand)]
pub enum McpCommand {
    /// Serve OutRig's self-description tools over stdio.
    #[command(name = "self")]
    SelfDescription,
    /// Print the image/config merged MCP table and exit.
    ShowMerged,
}

impl McpArgs {
    pub fn is_self_description(&self) -> bool {
        matches!(self.cmd, Some(McpCommand::SelfDescription))
    }
}

/// Run one `outrig mcp` invocation end-to-end. Returns the process exit code.
pub async fn execute(
    repo_cfg_path: &Path,
    global_cfg_path: &Path,
    session_root_flag: Option<&Path>,
    args: &McpArgs,
    verbose: u8,
) -> Result<i32> {
    let cli_env =
        CliEnvEntries::parse(&args.env).map_err(|e| OutrigError::Configuration(e.to_string()))?;
    if matches!(args.cmd, Some(McpCommand::ShowMerged)) && args.listen.is_some() {
        return Err(OutrigError::Configuration(
            "`outrig mcp show-merged` does not serve MCP; remove --listen".to_string(),
        )
        .into());
    }

    let setup = session_setup::setup(SessionSetupArgs {
        repo_cfg_path,
        global_cfg_path,
        session_root_flag,
        container_flag: args.container.as_deref(),
        attach_target: args.attach.as_deref(),
        agent_flag: None,
        require_agent: false,
        explicit_session_dir: args.session_dir.as_deref(),
        network_mode_override: args.network,
        device_override: None,
        verbose,
    })
    .await?;

    match &args.cmd {
        Some(McpCommand::SelfDescription) => unreachable!("handled before repo context"),
        None => serve(setup, cli_env, args.listen.as_ref()).await,
        Some(McpCommand::ShowMerged) => show_merged(setup).await,
    }
}

async fn serve(
    setup: SessionSetup,
    cli_env: CliEnvEntries,
    listen: Option<&ListenAddr>,
) -> Result<i32> {
    let SessionSetup {
        container_cfg_name,
        container_cfg,
        image_tag,
        container,
        sid,
        log_dir,
        store,
        attached,
        network,
        cfg: _,
        session: _,
        session_dir: _,
    } = setup;

    // Validate per-server env entries against the resolved MCP map.
    let mcp = session_setup::merged_mcp(&container, &container_cfg).await?;
    for name in cli_env.per_server_names() {
        if !mcp.contains_key(name) {
            return Err(OutrigError::Configuration(format!(
                "--env {name}:...: container '{}' has no MCP server '{name}'",
                container_cfg_name
            ))
            .into());
        }
    }

    let mut mcp_arcs: Vec<Arc<McpClient>> = Vec::new();
    let outcome: Result<i32> = serve_inner(
        &container_cfg_name,
        &image_tag,
        &container,
        &log_dir,
        sid.as_str(),
        &mut mcp_arcs,
        &mcp,
        &cli_env,
        attached,
        listen,
    )
    .await;

    let final_exit = outcome.as_ref().copied().unwrap_or(1);
    session_setup::teardown(mcp_arcs, network, container, &store, &sid, final_exit).await;
    if attached
        && outcome
            .as_ref()
            .err()
            .is_some_and(is_attached_container_stopped)
    {
        eprintln!(
            "error: {}",
            outcome.as_ref().expect_err("checked err above")
        );
        std::process::exit(final_exit.clamp(0, 255));
    }
    outcome
}

fn is_attached_container_stopped(err: &crate::error::CliError) -> bool {
    matches!(
        err,
        crate::error::CliError::Outrig(OutrigError::Configuration(msg))
            if msg.contains("attached container") && msg.contains("stopped"),
    )
}

async fn show_merged(setup: SessionSetup) -> Result<i32> {
    let SessionSetup {
        container_cfg,
        container,
        sid,
        store,
        attached: _,
        network,
        cfg: _,
        container_cfg_name: _,
        image_tag: _,
        session: _,
        session_dir: _,
        log_dir: _,
    } = setup;

    let outcome = show_merged_inner(&container_cfg, &container).await;
    let final_exit = outcome.as_ref().copied().unwrap_or(1);
    session_setup::teardown(Vec::new(), network, container, &store, &sid, final_exit).await;
    outcome
}

fn parse_network_mode(s: &str) -> std::result::Result<NetworkMode, String> {
    s.parse()
}

fn parse_listen_addr(s: &str) -> std::result::Result<ListenAddr, String> {
    if let Some(path) = s.strip_prefix("unix:") {
        if path.is_empty() {
            return Err("unix listen address must include a socket path".to_string());
        }
        return Ok(ListenAddr::Unix(PathBuf::from(path)));
    }

    s.parse::<SocketAddr>().map(ListenAddr::Tcp).map_err(|_| {
        "listen address must be HOST:PORT, [IPv6]:PORT, or unix:/path/to/socket".to_string()
    })
}

#[allow(clippy::too_many_arguments)]
async fn serve_inner(
    container_cfg_name: &str,
    image_tag: &ImageTag,
    container: &Container,
    log_dir: &Path,
    session_id: &str,
    mcp_arcs: &mut Vec<Arc<McpClient>>,
    mcp: &BTreeMap<String, McpServerSpec>,
    cli_env: &CliEnvEntries,
    attached: bool,
    listen: Option<&ListenAddr>,
) -> Result<i32> {
    let connected = session_setup::connect_mcp_clients(container, mcp, log_dir, cli_env).await?;
    if connected.is_empty() {
        return Err(OutrigError::Configuration(
            "outrig mcp with no merged MCP entries has nothing to proxy".to_string(),
        )
        .into());
    }
    mcp_arcs.extend(connected);

    let proxy = ProxyServer::build(mcp_arcs.clone()).await?;
    let per_server_counts: Vec<(String, usize)> = proxy
        .per_server_counts()
        .into_iter()
        .map(|(n, c)| (n.to_string(), c))
        .collect();
    let public_names: Vec<String> = proxy.iter_public_names().map(str::to_string).collect();

    let transport = match listen {
        None => "stdio",
        Some(ListenAddr::Tcp(_) | ListenAddr::Unix(_)) => "streamable-http",
    };

    print_banner(StartupBanner {
        container_name: container_cfg_name,
        image_tag,
        container_pod_name: container.name(),
        per_server_counts: &per_server_counts,
        public_names: &public_names,
        session_id,
        attached,
        transport,
    });

    match listen {
        None => serve_stdio_transport(proxy, container, attached).await,
        Some(addr) => serve_http_transport(proxy, addr, container, attached, mcp_arcs).await,
    }
}

async fn serve_stdio_transport(
    proxy: ProxyServer,
    container: &Container,
    attached: bool,
) -> Result<i32> {
    // `serve_server_with_ct` lets us hold the cancellation token outside the
    // service, which is otherwise consumed by `waiting()`. Cancel-on-signal
    // -> dispatcher quiesces -> `waiting()` returns -> teardown runs.
    let ct = CancellationToken::new();
    let service =
        rmcp::service::serve_server_with_ct(proxy, rmcp::transport::stdio(), ct.clone()).await?;
    eprintln!("[outrig] mcp server ready");

    let mut waiter = tokio::spawn(service.waiting());
    let mut sigterm = signal(SignalKind::terminate()).map_err(OutrigError::Io)?;
    let mut monitor = Box::pin(async {
        if attached {
            wait_for_attached_container_stop(container.name().to_string()).await
        } else {
            std::future::pending::<Result<()>>().await
        }
    });

    tokio::select! {
        biased;
        _ = tokio::signal::ctrl_c() => {
            tracing::info!(target: "outrig::cli::mcp", "received SIGINT; shutting down");
            ct.cancel();
        }
        _ = sigterm.recv() => {
            tracing::info!(target: "outrig::cli::mcp", "received SIGTERM; shutting down");
            ct.cancel();
        }
        result = &mut waiter => {
            log_waiter_result(result);
            return Ok(0);
        }
        result = &mut monitor => {
            ct.cancel();
            match tokio::time::timeout(ATTACH_MONITOR_SHUTDOWN_GRACE, &mut waiter).await {
                Ok(waiter_result) => log_waiter_result(waiter_result),
                Err(_) => {
                    waiter.abort();
                    tracing::warn!(
                        target: "outrig::cli::mcp",
                        "rmcp service did not stop after attached container disappeared"
                    );
                }
            }
            return match result {
                Ok(()) => Err(OutrigError::Configuration(
                    "attached container monitor ended unexpectedly".to_string(),
                ).into()),
                Err(e) => Err(e),
            };
        }
    }

    // Signal path: wait for the service to wind down after cancellation.
    let result = waiter.await;
    log_waiter_result(result);
    Ok(0)
}

async fn serve_http_transport(
    proxy: ProxyServer,
    listen: &ListenAddr,
    container: &Container,
    attached: bool,
    backing_clients: &[Arc<McpClient>],
) -> Result<i32> {
    let ct = CancellationToken::new();
    let session_manager = Arc::new(LocalSessionManager::default());
    let router = streamable_http_router(proxy, listen, ct.child_token(), session_manager.clone());

    let outcome = match listen {
        ListenAddr::Tcp(addr) => {
            let listener = tokio::net::TcpListener::bind(addr).await?;
            let local_addr = listener.local_addr()?;
            if let Some(warning) = listen_exposure_warning(&ListenAddr::Tcp(local_addr)) {
                eprintln!("{warning}");
            }
            eprintln!(
                "[outrig] listen: {}",
                listen_endpoint(&ListenAddr::Tcp(local_addr))
            );
            let shutdown = http_shutdown(ct.clone());
            let server = axum::serve(listener, router).with_graceful_shutdown(shutdown);
            wait_for_http_shutdown(server, ct, container, attached).await
        }
        ListenAddr::Unix(path) => {
            serve_unix_http_transport(router, path, ct, container, attached).await
        }
    };
    close_http_sessions(&session_manager).await;
    wait_for_http_session_refs(backing_clients).await;
    outcome
}

#[cfg(unix)]
async fn serve_unix_http_transport(
    router: axum::Router,
    path: &Path,
    ct: CancellationToken,
    container: &Container,
    attached: bool,
) -> Result<i32> {
    prepare_unix_socket(path)?;
    let listener = tokio::net::UnixListener::bind(path)?;
    let _cleanup = UnixSocketCleanup {
        path: path.to_path_buf(),
    };
    eprintln!("[outrig] listen: unix:{}", path.display());
    let shutdown = http_shutdown(ct.clone());
    let server = axum::serve(listener, router).with_graceful_shutdown(shutdown);
    wait_for_http_shutdown(server, ct, container, attached).await
}

#[cfg(not(unix))]
async fn serve_unix_http_transport(
    _router: axum::Router,
    _path: &Path,
    _ct: CancellationToken,
    _container: &Container,
    _attached: bool,
) -> Result<i32> {
    Err(
        OutrigError::Configuration("unix listen addresses require a Unix platform".to_string())
            .into(),
    )
}

fn streamable_http_router(
    proxy: ProxyServer,
    listen: &ListenAddr,
    ct: CancellationToken,
    session_manager: Arc<LocalSessionManager>,
) -> axum::Router {
    let service = StreamableHttpService::new(
        move || Ok(proxy.clone()),
        session_manager,
        streamable_http_config(listen, ct),
    );
    axum::Router::new().nest_service("/mcp", service)
}

fn streamable_http_config(
    listen: &ListenAddr,
    ct: CancellationToken,
) -> StreamableHttpServerConfig {
    let config = StreamableHttpServerConfig::default().with_cancellation_token(ct);
    match listen {
        ListenAddr::Tcp(addr) if addr.ip().is_loopback() => config,
        ListenAddr::Tcp(_) | ListenAddr::Unix(_) => config.disable_allowed_hosts(),
    }
}

async fn http_shutdown(ct: CancellationToken) {
    ct.cancelled_owned().await;
}

async fn wait_for_http_shutdown<F>(
    server: F,
    ct: CancellationToken,
    container: &Container,
    attached: bool,
) -> Result<i32>
where
    F: IntoFuture<Output = std::io::Result<()>>,
{
    eprintln!("[outrig] mcp server ready");
    let mut server = Box::pin(server.into_future());
    let mut sigterm = signal(SignalKind::terminate()).map_err(OutrigError::Io)?;
    let mut monitor = Box::pin(async {
        if attached {
            wait_for_attached_container_stop(container.name().to_string()).await
        } else {
            std::future::pending::<Result<()>>().await
        }
    });

    tokio::select! {
        biased;
        _ = tokio::signal::ctrl_c() => {
            tracing::info!(target: "outrig::cli::mcp", "received SIGINT; shutting down");
            ct.cancel();
        }
        _ = sigterm.recv() => {
            tracing::info!(target: "outrig::cli::mcp", "received SIGTERM; shutting down");
            ct.cancel();
        }
        result = &mut server => {
            result?;
            return Ok(0);
        }
        result = &mut monitor => {
            ct.cancel();
            match tokio::time::timeout(ATTACH_MONITOR_SHUTDOWN_GRACE, &mut server).await {
                Ok(server_result) => server_result?,
                Err(_) => {
                    tracing::warn!(
                        target: "outrig::cli::mcp",
                        "HTTP MCP service did not stop after attached container disappeared"
                    );
                }
            }
            return match result {
                Ok(()) => Err(OutrigError::Configuration(
                    "attached container monitor ended unexpectedly".to_string(),
                ).into()),
                Err(e) => Err(e),
            };
        }
    }

    server.await?;
    Ok(0)
}

async fn close_http_sessions(session_manager: &LocalSessionManager) {
    let session_ids = session_manager
        .sessions
        .read()
        .await
        .keys()
        .cloned()
        .collect::<Vec<_>>();
    for session_id in session_ids {
        if let Err(e) = session_manager.close_session(&session_id).await {
            tracing::warn!(
                target: "outrig::cli::mcp",
                "failed to close HTTP MCP session {session_id}: {e}"
            );
        }
    }
}

async fn wait_for_http_session_refs(backing_clients: &[Arc<McpClient>]) {
    let released = tokio::time::timeout(HTTP_SESSION_SHUTDOWN_GRACE, async {
        while backing_clients
            .iter()
            .any(|client| Arc::strong_count(client) > 1)
        {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await;

    if released.is_err() {
        let counts = backing_clients
            .iter()
            .map(|client| format!("{}={}", client.name(), Arc::strong_count(client)))
            .collect::<Vec<_>>()
            .join(", ");
        tracing::warn!(
            target: "outrig::cli::mcp",
            "HTTP MCP sessions still hold backing clients after shutdown grace: {counts}"
        );
    }
}

fn listen_endpoint(listen: &ListenAddr) -> String {
    match listen {
        ListenAddr::Tcp(addr) => format!("http://{addr}/mcp"),
        ListenAddr::Unix(path) => format!("unix:{}", path.display()),
    }
}

fn listen_exposure_warning(listen: &ListenAddr) -> Option<String> {
    match listen {
        ListenAddr::Tcp(addr) if !addr.ip().is_loopback() => Some(format!(
            "[outrig] WARNING: listening on {addr} exposes this container's MCP tool surface \
             to anything that can reach the port; v1 has no built-in auth"
        )),
        _ => None,
    }
}

#[cfg(unix)]
fn prepare_unix_socket(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
        && !parent.exists()
    {
        return Err(OutrigError::Configuration(format!(
            "unix listen socket parent directory does not exist: {}",
            parent.display()
        ))
        .into());
    }

    match std::fs::metadata(path) {
        Ok(meta) if meta.file_type().is_socket() => {
            std::fs::remove_file(path)?;
            Ok(())
        }
        Ok(_) => Err(OutrigError::Configuration(format!(
            "unix listen path exists and is not a socket: {}",
            path.display()
        ))
        .into()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(OutrigError::Io(e).into()),
    }
}

#[cfg(unix)]
struct UnixSocketCleanup {
    path: PathBuf,
}

#[cfg(unix)]
impl Drop for UnixSocketCleanup {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

async fn wait_for_attached_container_stop(container_name: String) -> Result<()> {
    let mut child = tokio::process::Command::new("podman")
        .arg("wait")
        .arg(&container_name)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()?;
    let status = child.wait().await?;
    if !status.success() {
        tracing::warn!(
            target: "outrig::cli::mcp",
            "podman wait for attached container {container_name:?} exited with {status}"
        );
    }
    Err(OutrigError::Configuration(format!(
        "attached container {container_name:?} stopped while `outrig mcp` was attached"
    ))
    .into())
}

async fn show_merged_inner(container_cfg: &ContainerConfig, container: &Container) -> Result<i32> {
    let mcp = session_setup::merged_mcp(container, container_cfg).await?;
    write_merged_mcp(&mcp)?;
    Ok(0)
}

fn write_merged_mcp(mcp: &BTreeMap<String, McpServerSpec>) -> Result<()> {
    #[derive(Serialize)]
    struct MergedMcpView<'a> {
        mcp: &'a BTreeMap<String, McpServerSpec>,
    }

    let rendered = if mcp.is_empty() {
        "[mcp]\n".to_string()
    } else {
        toml::to_string_pretty(&MergedMcpView { mcp }).map_err(|source| {
            OutrigError::Configuration(format!("serialize merged MCP TOML: {source}"))
        })?
    };

    let mut stdout = std::io::stdout().lock();
    stdout.write_all(rendered.as_bytes())?;
    stdout.flush()?;
    Ok(())
}

fn log_waiter_result(
    result: std::result::Result<
        std::result::Result<rmcp::service::QuitReason, tokio::task::JoinError>,
        tokio::task::JoinError,
    >,
) {
    match result {
        Ok(Ok(reason)) => {
            tracing::debug!(
                target: "outrig::cli::mcp",
                "rmcp service exited: {reason:?}"
            );
        }
        Ok(Err(join_err)) => {
            tracing::warn!(
                target: "outrig::cli::mcp",
                "rmcp dispatcher join error: {join_err}"
            );
        }
        Err(join_err) => {
            tracing::warn!(
                target: "outrig::cli::mcp",
                "rmcp waiter join error: {join_err}"
            );
        }
    }
}

struct StartupBanner<'a> {
    container_name: &'a str,
    image_tag: &'a ImageTag,
    container_pod_name: &'a str,
    per_server_counts: &'a [(String, usize)],
    public_names: &'a [String],
    session_id: &'a str,
    attached: bool,
    transport: &'a str,
}

fn print_banner(banner: StartupBanner<'_>) {
    let mut buf = String::new();
    let _ = writeln!(buf, "[outrig] container-config:  {}", banner.container_name);
    let _ = writeln!(buf, "[outrig] image:             {}", banner.image_tag);
    let container_action = if banner.attached {
        "attached"
    } else {
        "started"
    };
    let _ = writeln!(
        buf,
        "[outrig] container {container_action}: {}",
        banner.container_pod_name
    );
    for (name, count) in banner.per_server_counts {
        let plural = if *count == 1 { "tool" } else { "tools" };
        let _ = writeln!(buf, "[outrig] mcp {name}: initialized ({count} {plural})");
    }
    let names_joined = banner
        .public_names
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join(", ");
    let _ = writeln!(buf, "[outrig] tools available: {names_joined}");
    let _ = writeln!(buf, "[outrig] session id: {}", banner.session_id);
    let _ = writeln!(buf, "[outrig] transport: {}", banner.transport);
    eprint!("{buf}");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_listen_addr_accepts_tcp_socket_addr() {
        let parsed = parse_listen_addr("127.0.0.1:7331").expect("parse listen addr");
        assert_eq!(
            parsed,
            ListenAddr::Tcp("127.0.0.1:7331".parse().expect("socket addr"))
        );
    }

    #[test]
    fn parse_listen_addr_accepts_unix_prefix() {
        let parsed = parse_listen_addr("unix:/tmp/outrig.sock").expect("parse listen addr");
        assert_eq!(parsed, ListenAddr::Unix(PathBuf::from("/tmp/outrig.sock")));
    }

    #[test]
    fn parse_listen_addr_rejects_missing_port() {
        let err = parse_listen_addr("127.0.0.1").expect_err("missing port should fail");
        assert!(
            err.contains("HOST:PORT"),
            "error should explain accepted forms: {err}"
        );
    }

    #[test]
    fn mcp_args_parse_listen_flag() {
        let args =
            McpArgs::try_parse_from(["mcp", "--listen", "127.0.0.1:7331"]).expect("arg parses");
        assert_eq!(
            args.listen,
            Some(ListenAddr::Tcp(
                "127.0.0.1:7331".parse().expect("socket addr")
            ))
        );
    }

    #[test]
    fn listen_exposure_warning_only_for_non_loopback_tcp() {
        let loopback = ListenAddr::Tcp("127.0.0.1:7331".parse().expect("socket addr"));
        assert!(listen_exposure_warning(&loopback).is_none());

        let public = ListenAddr::Tcp("0.0.0.0:7331".parse().expect("socket addr"));
        let warning = listen_exposure_warning(&public).expect("warning");
        assert!(warning.contains("WARNING"));
        assert!(warning.contains("no built-in auth"));
        assert!(warning.contains("MCP tool surface"));

        let unix = ListenAddr::Unix(PathBuf::from("/tmp/outrig.sock"));
        assert!(listen_exposure_warning(&unix).is_none());
    }
}
