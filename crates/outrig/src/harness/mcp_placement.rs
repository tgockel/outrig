//! Where a session's MCP servers run: the primary container, or a sidecar of
//! the session's own -- one each for a server an owner declared, one shared by
//! every server a configuration's `[sidecars.<sc>]` block hosts.
//!
//! A [`Placement`] starts what a server runs in when it is not running,
//! connects the server, and freezes, thaws and stops what it started. The MCP
//! table ([`super::mcp`]) holds one per container and decides everything else
//! -- which servers share one, what a failure means, when to try again -- so a
//! test drives the table with a fake placement and no container. A server a
//! URL names runs in nothing of the session's: its placement, [`Http`], dials
//! it, and has nothing to freeze or stop.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use futures_util::future::BoxFuture;
use http::{HeaderName, HeaderValue};
use tokio::sync::Mutex;

use super::mcp::TrustAnchor;
use crate::config::{McpTrust, ResolvedEnvValue};
use crate::container::{Container, ExecTarget};
use crate::error::{OutrigError, Result};
use crate::mcp::{HttpFailed, McpClient};
use crate::outrig_::{SHUTDOWN_GRACE, SidecarHost, SidecarServerSpec, SidecarSpec};

/// One server, as a placement starts it: its name, which names its stderr
/// file, where it was declared, how it runs, and its environment, resolved
/// once at the session's start and held, so a reconnect starts it with the
/// values it had.
#[derive(Debug, Clone)]
pub(crate) struct Server {
    pub(crate) name: String,
    /// Where it was declared, as a startup failure names it.
    pub(crate) declared: Option<&'static str>,
    pub(crate) launch: Launch,
    pub(crate) env: BTreeMap<String, ResolvedEnvValue>,
}

impl Server {
    /// Connect it as `command`, exec'd in the container `target` names.
    async fn connect_exec(
        &self,
        target: &ExecTarget,
        command: &[String],
        log_dir: &std::path::Path,
        within: Duration,
    ) -> Result<McpClient> {
        McpClient::connect_exec(
            target,
            command,
            self.env.clone(),
            &self.name,
            self.declared,
            log_dir,
            within,
        )
        .await
    }
}

/// How a server runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Launch {
    /// `podman exec -i` of this argv in a running container.
    Exec(Vec<String>),
    /// The container's own `ENTRYPOINT`, handed these arguments: the server is
    /// the container.
    Entrypoint(Vec<String>),
    /// Dialed at this URL, over streamable HTTP: its `env` is the headers
    /// each request carries.
    Url(Box<UrlLaunch>),
}

/// Where a URL server is, and what its client trusts there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct UrlLaunch {
    pub(crate) url: String,
    pub(crate) trust_anchor: Option<TrustAnchor>,
    pub(crate) trust: McpTrust,
    pub(crate) allow_stateless: bool,
}

/// Why a placement did not connect a server: what starting or connecting it
/// returned, or a streamable-HTTP connect, classified, which has no process
/// to report on.
#[derive(Debug)]
pub(crate) enum Unconnected {
    Outrig(OutrigError),
    Http(HttpFailed),
}

impl From<OutrigError> for Unconnected {
    fn from(e: OutrigError) -> Self {
        Self::Outrig(e)
    }
}

/// Where one or more of a session's servers run. Crate-private, which is
/// what seals it: the session's own placements, and a test's fake, are the
/// only ones.
pub(crate) trait Placement: Send + Sync + 'static {
    /// Connect `server` -- starting what it runs in first, when that is not
    /// running -- within `within`, that start included: its `initialize`
    /// handshake gets what the start left. A failure is the error starting or
    /// connecting returned, for the table to classify; what the attempt
    /// started is left for [`Placement::stop`].
    fn connect<'a>(
        &'a self,
        server: &'a Server,
        within: Duration,
    ) -> BoxFuture<'a, std::result::Result<McpClient, Unconnected>>;

    /// Freeze every process the placement runs, or say why not.
    fn freeze(&self) -> BoxFuture<'_, std::result::Result<(), String>>;

    /// Thaw what [`Placement::freeze`] froze, or say why not.
    fn thaw(&self) -> BoxFuture<'_, std::result::Result<(), String>>;

    /// Stop what the placement started, or say what was not proven stopped.
    fn stop(&self) -> BoxFuture<'_, std::result::Result<(), String>>;

    /// Ready what the placement runs for a server whose transport failed to be
    /// connected again, or say why it cannot be: before a reconnect, never a
    /// start. Nothing, unless a placement can tell what it runs is gone.
    fn recover(&self) -> BoxFuture<'_, std::result::Result<(), String>> {
        Box::pin(async { Ok(()) })
    }
}

/// The primary container, where a `Command` server runs beside the
/// interpreter. It cannot be frozen apart from the interpreter, and its
/// servers end with it.
pub(crate) struct Primary {
    target: ExecTarget,
    log_dir: PathBuf,
}

impl Primary {
    pub(crate) fn new(target: ExecTarget, log_dir: PathBuf) -> Self {
        Self { target, log_dir }
    }
}

impl Placement for Primary {
    fn connect<'a>(
        &'a self,
        server: &'a Server,
        within: Duration,
    ) -> BoxFuture<'a, std::result::Result<McpClient, Unconnected>> {
        Box::pin(async move {
            let Launch::Exec(command) = &server.launch else {
                return Err(OutrigError::Configuration(format!(
                    "mcp server {:?}: the primary runs only a command",
                    server.name
                ))
                .into());
            };
            Ok(server
                .connect_exec(&self.target, command, &self.log_dir, within)
                .await?)
        })
    }

    fn freeze(&self) -> BoxFuture<'_, std::result::Result<(), String>> {
        Box::pin(async { Err(SHARES_THE_PRIMARY.to_string()) })
    }

    fn thaw(&self) -> BoxFuture<'_, std::result::Result<(), String>> {
        Box::pin(async { Err(SHARES_THE_PRIMARY.to_string()) })
    }

    fn stop(&self) -> BoxFuture<'_, std::result::Result<(), String>> {
        // Its servers end with the primary: closing their transports is all
        // there is to do apart from the container's own stop.
        Box::pin(async { Ok(()) })
    }
}

/// Why a server in the primary is not frozen.
pub(crate) const SHARES_THE_PRIMARY: &str =
    "it runs in the primary container beside the interpreter, which cannot be frozen apart from it";

/// A sidecar of the session's own, hosting one server an owner declared or
/// every server a configuration's block hosts.
pub(crate) struct Sidecar {
    host: SidecarHost,
    /// What the container is: its image, workspace, mounts and security, and
    /// for an exec host the servers it hosts.
    spec: SidecarSpec,
    /// The container's name.
    name: String,
    /// The container, while one runs.
    container: Mutex<Option<Container>>,
    /// Containers that would not stop when a failed start unwound them, kept
    /// for the stop to try again.
    abandoned: Mutex<Vec<Container>>,
    /// Whether a container was stopped and the interceptor not yet detached
    /// from its name: a stop dropped before its detach leaves it for the next.
    detach_owed: AtomicBool,
}

impl Sidecar {
    /// The sidecar `spec` describes, in the session `host` is of.
    pub(crate) fn new(host: SidecarHost, spec: SidecarSpec) -> Self {
        Self {
            name: host.container_name(&spec.name),
            host,
            spec,
            container: Mutex::new(None),
            abandoned: Mutex::new(Vec::new()),
            detach_owed: AtomicBool::new(false),
        }
    }

    /// A fresh entrypoint container for `server`, created with its
    /// environment and arguments, attached and not yet started, in `held`;
    /// whatever ran before it is stopped first, since an entrypoint host is
    /// its server, and one that would not stop is not replaced -- the new one
    /// would take its name, and its filter with it. A container this is
    /// dropped holding is in `held` or `abandoned`, for a stop to find.
    async fn entrypoint(
        &self,
        held: &mut Option<Container>,
        server: &Server,
        args: &[String],
    ) -> Result<()> {
        let mut abandoned = self.abandoned.lock().await;
        if let Err(mut failed) = self.stop_all(held, &mut abandoned).await {
            return Err(failed.swap_remove(0));
        }
        let hosted = SidecarServerSpec::entrypoint(args.iter().cloned());
        let spec = self
            .spec
            .clone()
            .with_server_spec(server.name.clone(), hosted.clone());
        let created = self
            .host
            .prepare(
                &spec,
                self.name.clone(),
                Some((&hosted, server.env.clone())),
                &mut abandoned,
            )
            .await?;
        *held = Some(created);
        Ok(())
    }

    /// What `failed`, the stops of its containers that failed, says.
    fn said(&self, failed: &[OutrigError]) -> String {
        failed
            .iter()
            .map(|e| format!("sidecar {:?}: {e}", self.name))
            .collect::<Vec<_>>()
            .join("; ")
    }

    /// Stop every container this placement holds -- `held`, and each an
    /// earlier stop could not -- each in place, so one stays held until its
    /// stop is proven: a stop that fails, or is dropped part-way, leaves it
    /// for the next. The interceptor is detached only once every one has
    /// stopped, so one still running stays filtered and audited, and a detach
    /// a dropped stop owed is made by the next. Each stop that failed, when
    /// any did.
    async fn stop_all(
        &self,
        held: &mut Option<Container>,
        abandoned: &mut Vec<Container>,
    ) -> std::result::Result<(), Vec<OutrigError>> {
        // With no await between, so nothing is held by this frame alone.
        abandoned.extend(held.take());
        let mut failed = Vec::new();
        let mut next = 0;
        while let Some(container) = abandoned.get_mut(next) {
            match container.stop_in_place(SHUTDOWN_GRACE).await {
                Ok(()) => {
                    abandoned.remove(next);
                    self.detach_owed.store(true, Ordering::SeqCst);
                }
                Err(e) => {
                    failed.push(e);
                    next += 1;
                }
            }
        }
        if !failed.is_empty() {
            return Err(failed);
        }
        // A stopped container took its namespaces, and the interception in
        // them, with it: this frees its name and ends its loops.
        if self.detach_owed.load(Ordering::SeqCst) {
            if let Err(e) = self.host.detach(&self.name).await {
                tracing::warn!(target: "outrig::mcp", "sidecar {:?}: {e}", self.name);
            }
            self.detach_owed.store(false, Ordering::SeqCst);
        }
        Ok(())
    }
}

impl Placement for Sidecar {
    fn connect<'a>(
        &'a self,
        server: &'a Server,
        within: Duration,
    ) -> BoxFuture<'a, std::result::Result<McpClient, Unconnected>> {
        Box::pin(async move {
            let by = tokio::time::Instant::now() + within;
            let left = || by.saturating_duration_since(tokio::time::Instant::now());
            let log_dir = self.host.log_dir().to_path_buf();
            match &server.launch {
                Launch::Exec(command) => {
                    // Started once, and held while each of its servers connects: a
                    // server's attempt that fails leaves the container to the next,
                    // or to the stop the table makes of a container of its own.
                    let target = {
                        let mut held = self.container.lock().await;
                        if held.is_none() {
                            let mut abandoned = self.abandoned.lock().await;
                            let started = self
                                .host
                                .prepare(&self.spec, self.name.clone(), None, &mut abandoned)
                                .await?;
                            *held = Some(started);
                        }
                        held.as_ref().expect("started above").exec_target()
                    };
                    Ok(server
                        .connect_exec(&target, command, &log_dir, left())
                        .await?)
                }
                Launch::Entrypoint(args) => {
                    // Held from its creation, so a stop finds the container
                    // however this attempt ends -- failed, cut off at the
                    // budget, or cancelled -- and stops it before it detaches.
                    let mut held = self.container.lock().await;
                    self.entrypoint(&mut held, server, args).await?;
                    let container = held.as_ref().expect("created above");
                    // Retired if this future is dropped mid-connect, as an add's is.
                    let attachment = self.host.uncommitted(container.name());
                    let connected = McpClient::connect_via_podman_start_within(
                        container,
                        &server.name,
                        server.declared,
                        &log_dir,
                        left(),
                    )
                    .await;
                    attachment.disarm();
                    Ok(connected?)
                }
                Launch::Url(_) => Err(OutrigError::Configuration(format!(
                    "mcp server {:?}: a sidecar runs no server a URL names",
                    server.name
                ))
                .into()),
            }
        })
    }

    fn freeze(&self) -> BoxFuture<'_, std::result::Result<(), String>> {
        Box::pin(async move {
            match &*self.container.lock().await {
                Some(container) => container.pause().await.map_err(|e| e.to_string()),
                None => Err(format!("sidecar {:?} is not running", self.name)),
            }
        })
    }

    fn thaw(&self) -> BoxFuture<'_, std::result::Result<(), String>> {
        Box::pin(async move {
            match &*self.container.lock().await {
                Some(container) => container.unpause().await.map_err(|e| e.to_string()),
                None => Err(format!("sidecar {:?} is not running", self.name)),
            }
        })
    }

    /// A container that is gone -- killed, its servers' transports with it --
    /// is stopped as a held one is, which proves it gone and frees its name,
    /// so the connect after starts a new one; a running one is left for the
    /// connect to exec into. Gone or not held, what an earlier stop left -- a
    /// container that did not stop, a detach not made -- is finished first:
    /// the new container takes the same name, and its attachment too.
    fn recover(&self) -> BoxFuture<'_, std::result::Result<(), String>> {
        Box::pin(async move {
            let mut held = self.container.lock().await;
            if let Some(container) = held.as_ref()
                && Container::is_running(container.name())
                    .await
                    .unwrap_or(true)
            {
                return Ok(());
            }
            let mut abandoned = self.abandoned.lock().await;
            self.stop_all(&mut held, &mut abandoned)
                .await
                .map_err(|failed| self.said(&failed))
        })
    }

    fn stop(&self) -> BoxFuture<'_, std::result::Result<(), String>> {
        Box::pin(async move {
            let mut held = self.container.lock().await;
            let mut abandoned = self.abandoned.lock().await;
            self.stop_all(&mut held, &mut abandoned)
                .await
                .map_err(|failed| self.said(&failed))
        })
    }
}

/// A server a URL names, dialed over streamable HTTP: nothing of the
/// session's runs it, so there is nothing to freeze, thaw or stop, and a
/// quiesce holds its calls and nothing else. Each connect is a new HTTP
/// session, through one client -- built at the first, with the declaration's
/// trust anchor -- carrying the server's resolved headers and, for one the
/// owner declared, the owner's identity headers.
pub(crate) struct Http {
    /// The owner's identity headers, never read, logged or shown: empty for
    /// a server a configuration declared.
    identity: Vec<(String, String)>,
    client: tokio::sync::OnceCell<reqwest::Client>,
}

impl Http {
    pub(crate) fn new(identity: Vec<(String, String)>) -> Self {
        Self {
            identity,
            client: tokio::sync::OnceCell::new(),
        }
    }

    /// The client every connect of `url`'s server goes through, built once
    /// with its trust anchor; or why it cannot be.
    async fn client(&self, url: &UrlLaunch) -> std::result::Result<&reqwest::Client, String> {
        self.client
            .get_or_try_init(|| async {
                let anchor = match &url.trust_anchor {
                    None => None,
                    Some(TrustAnchor::Pem(pem)) => Some(pem.clone()),
                    Some(TrustAnchor::Path(path)) => {
                        Some(tokio::fs::read(path).await.map_err(|e| {
                            format!("its trust anchor {} could not be read: {e}", path.display())
                        })?)
                    }
                };
                let only = url.trust == McpTrust::Only;
                // It reads and parses the system's roots: off the runtime's
                // threads, so the other services' starts go on meanwhile.
                tokio::task::spawn_blocking(move || {
                    crate::mcp::http_client(anchor.as_deref(), only)
                })
                .await
                .map_err(|e| format!("its HTTP client was not built: {e}"))?
            })
            .await
    }

    /// `server`'s resolved headers, then the identity headers, as each
    /// request carries them.
    fn headers(
        &self,
        server: &Server,
    ) -> std::result::Result<HashMap<HeaderName, HeaderValue>, String> {
        let declared = server
            .env
            .iter()
            .map(|(name, value)| (name.as_str(), value.value()));
        let identity = self
            .identity
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str()));
        declared
            .chain(identity)
            .map(|(name, value)| crate::mcp::http_header(name, value))
            .collect()
    }
}

impl Placement for Http {
    fn connect<'a>(
        &'a self,
        server: &'a Server,
        within: Duration,
    ) -> BoxFuture<'a, std::result::Result<McpClient, Unconnected>> {
        Box::pin(async move {
            let misconfigured = |why: String| {
                OutrigError::Configuration(format!("mcp server {:?}: {why}", server.name))
            };
            let Launch::Url(url) = &server.launch else {
                return Err(misconfigured("it names no URL".to_string()).into());
            };
            let client = self.client(url).await.map_err(misconfigured)?.clone();
            let headers = self.headers(server).map_err(misconfigured)?;
            McpClient::connect_http(
                &server.name,
                client,
                &url.url,
                headers,
                url.allow_stateless,
                within,
            )
            .await
            .map_err(Unconnected::Http)
        })
    }

    fn freeze(&self) -> BoxFuture<'_, std::result::Result<(), String>> {
        Box::pin(async { Ok(()) })
    }

    fn thaw(&self) -> BoxFuture<'_, std::result::Result<(), String>> {
        Box::pin(async { Ok(()) })
    }

    fn stop(&self) -> BoxFuture<'_, std::result::Result<(), String>> {
        // The server is not the session's to stop: closing its transport,
        // which the table does first, ends the HTTP session.
        Box::pin(async { Ok(()) })
    }
}

/// A placement with no container behind it, whose servers are rmcp servers
/// in memory, for the MCP table's tests.
#[cfg(test)]
pub(crate) mod fake {
    use std::collections::{HashMap, VecDeque};
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use std::sync::Arc;

    use futures_util::TryFutureExt;
    use futures_util::future::BoxFuture;
    use rmcp::model::{
        CallToolRequestParams, CallToolResponse, CallToolResult, CancelledNotificationParam,
        ClientNotification, ClientRequest, ContentBlock, CustomResult, ErrorCode, ErrorData,
        InitializeRequestParams, InitializeResult, ListToolsResult, PaginatedRequestParams,
        RequestId, ServerCapabilities, ServerConfig, ServerResult, Tool,
    };
    use rmcp::service::{NotificationContext, RequestContext, RoleServer};
    use serde_json::Value;
    use tokio_util::sync::CancellationToken;

    use super::*;

    /// What one attempt to connect a server does.
    #[derive(Clone)]
    pub(crate) enum Behavior {
        /// Answer, with these tools, as [`Server`] does.
        Serve(Vec<&'static str>),
        /// Never answer `initialize`.
        Hang,
        /// Answer `initialize` with a JSON-RPC error.
        Refuse,
        /// Refuse once the whole bound it was given has passed, as a server
        /// whose diagnosis waits as long as its handshake had left.
        RefuseAtBound,
        /// Exit with this code before `initialize`, having written this.
        Exit(i32, &'static str),
        /// Never finish connecting, as a container whose preparation wedged.
        Wedged,
        /// Answer `initialize`, and never `tools/list`.
        Unlisted,
        /// Run this shell script, a server that is a real process.
        Script(&'static str),
        /// Answer as this [`Toolbox`] says, which the test keeps.
        Toolbox(Arc<Toolbox>),
    }

    /// A placement that runs no container: each connect does what its
    /// server's script says next, or serves `echo`, `sleep` and `slow`.
    #[derive(Default)]
    pub(crate) struct Fake {
        scripts: Mutex<HashMap<String, VecDeque<Behavior>>>,
        connects: AtomicUsize,
        /// Each connect's server and its environment, values included.
        envs: Mutex<Vec<(String, BTreeMap<String, String>)>>,
        pub(crate) frozen: AtomicBool,
        pub(crate) stopped: AtomicBool,
        freezes: AtomicUsize,
        thaws: AtomicUsize,
        stops: AtomicUsize,
        freeze_fails: Mutex<Option<String>>,
        stop_fails: Mutex<Option<String>>,
        /// How long each stop takes.
        stop_takes: Mutex<Duration>,
        /// How long readying it again, before a reconnect, takes.
        recover_takes: Mutex<Duration>,
        /// Where each script's stderr is kept.
        logs: Mutex<Vec<tempfile::TempDir>>,
    }

    impl Fake {
        /// `server`'s attempts, in order; once they run out it serves.
        pub(crate) fn script(&self, server: &str, attempts: impl IntoIterator<Item = Behavior>) {
            self.scripts
                .lock()
                .unwrap()
                .insert(server.to_string(), attempts.into_iter().collect());
        }

        pub(crate) fn connects(&self) -> usize {
            self.connects.load(Ordering::SeqCst)
        }

        pub(crate) fn envs(&self) -> Vec<(String, BTreeMap<String, String>)> {
            self.envs.lock().unwrap().clone()
        }

        pub(crate) fn freezes(&self) -> usize {
            self.freezes.load(Ordering::SeqCst)
        }

        pub(crate) fn thaws(&self) -> usize {
            self.thaws.load(Ordering::SeqCst)
        }

        pub(crate) fn stops(&self) -> usize {
            self.stops.load(Ordering::SeqCst)
        }

        pub(crate) fn fail_freeze(&self, why: &str) {
            *self.freeze_fails.lock().unwrap() = Some(why.to_string());
        }

        pub(crate) fn fail_stop(&self, why: &str) {
            *self.stop_fails.lock().unwrap() = Some(why.to_string());
        }

        pub(crate) fn slow_stop(&self, takes: Duration) {
            *self.stop_takes.lock().unwrap() = takes;
        }

        pub(crate) fn slow_recover(&self, takes: Duration) {
            *self.recover_takes.lock().unwrap() = takes;
        }
    }

    impl Placement for Fake {
        fn connect<'a>(
            &'a self,
            server: &'a Server,
            within: Duration,
        ) -> BoxFuture<'a, std::result::Result<McpClient, Unconnected>> {
            Box::pin(
                self.connect_as_scripted(server, within)
                    .map_err(Unconnected::from),
            )
        }

        fn freeze(&self) -> BoxFuture<'_, std::result::Result<(), String>> {
            Box::pin(async move {
                self.freezes.fetch_add(1, Ordering::SeqCst);
                if let Some(why) = self.freeze_fails.lock().unwrap().clone() {
                    return Err(why);
                }
                self.frozen.store(true, Ordering::SeqCst);
                Ok(())
            })
        }

        fn thaw(&self) -> BoxFuture<'_, std::result::Result<(), String>> {
            Box::pin(async move {
                self.thaws.fetch_add(1, Ordering::SeqCst);
                self.frozen.store(false, Ordering::SeqCst);
                Ok(())
            })
        }

        fn stop(&self) -> BoxFuture<'_, std::result::Result<(), String>> {
            Box::pin(async move {
                self.stops.fetch_add(1, Ordering::SeqCst);
                let takes = *self.stop_takes.lock().unwrap();
                tokio::time::sleep(takes).await;
                if let Some(why) = self.stop_fails.lock().unwrap().clone() {
                    return Err(why);
                }
                self.stopped.store(true, Ordering::SeqCst);
                Ok(())
            })
        }

        fn recover(&self) -> BoxFuture<'_, std::result::Result<(), String>> {
            Box::pin(async move {
                let takes = *self.recover_takes.lock().unwrap();
                tokio::time::sleep(takes).await;
                Ok(())
            })
        }
    }

    impl Fake {
        /// What `server`'s next scripted attempt does.
        async fn connect_as_scripted(
            &self,
            server: &Server,
            within: Duration,
        ) -> Result<McpClient> {
            self.connects.fetch_add(1, Ordering::SeqCst);
            self.envs.lock().unwrap().push((
                server.name.clone(),
                server
                    .env
                    .iter()
                    .map(|(key, value)| (key.clone(), value.value().to_string()))
                    .collect(),
            ));
            let next = self
                .scripts
                .lock()
                .unwrap()
                .get_mut(&server.name)
                .and_then(VecDeque::pop_front)
                .unwrap_or(Behavior::Serve(vec!["echo", "sleep", "slow"]));
            let tools = match next {
                Behavior::Serve(tools) => tools,
                Behavior::Hang => {
                    return McpClient::handshake_in_memory(&server.name, Hanging, within).await;
                }
                Behavior::Refuse => {
                    return McpClient::handshake_in_memory(&server.name, Refusing, within).await;
                }
                Behavior::RefuseAtBound => {
                    tokio::time::sleep(within).await;
                    return McpClient::handshake_in_memory(&server.name, Refusing, within).await;
                }
                Behavior::Wedged => return std::future::pending().await,
                Behavior::Toolbox(toolbox) => {
                    let hang_up = toolbox.hang_up.clone();
                    return McpClient::handshake_in_memory_until(
                        &server.name,
                        Raw(toolbox),
                        within,
                        hang_up,
                    )
                    .await;
                }
                Behavior::Unlisted => {
                    return McpClient::handshake_in_memory(&server.name, Unlisting, within).await;
                }
                Behavior::Script(script) => {
                    let log = tempfile::tempdir().expect("tempdir");
                    let log_dir = log.path().to_path_buf();
                    self.logs.lock().unwrap().push(log);
                    return McpClient::connect_script(&server.name, script, &log_dir, within).await;
                }
                Behavior::Exit(code, stderr) => {
                    let command = match &server.launch {
                        Launch::Exec(command) => command.clone(),
                        Launch::Entrypoint(args) => args.clone(),
                        Launch::Url(_) => Vec::new(),
                    };
                    return Err(McpClient::exited_before_initialize(
                        &server.name,
                        &command,
                        code,
                        stderr,
                    ));
                }
            };
            McpClient::handshake_in_memory(&server.name, Serving { tools }, within).await
        }
    }

    /// A server offering `tools`: `echo` answers its `text`, `sleep` waits
    /// `secs` seconds, and `slow` never answers.
    pub(crate) struct Serving {
        pub(crate) tools: Vec<&'static str>,
    }

    impl rmcp::ServerHandler for Serving {
        fn get_info(&self) -> ServerConfig {
            ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
                .with_instructions("A test server.\nIt says more on later lines.")
        }

        async fn list_tools(
            &self,
            _request: Option<PaginatedRequestParams>,
            _context: RequestContext<RoleServer>,
        ) -> std::result::Result<ListToolsResult, ErrorData> {
            let schema = serde_json::json!({
                "type": "object",
                "properties": {"text": {"type": "string"}, "secs": {"type": "number"}},
            });
            let serde_json::Value::Object(schema) = schema else {
                unreachable!()
            };
            let schema = std::sync::Arc::new(schema);
            Ok(ListToolsResult::with_all_items(
                self.tools
                    .iter()
                    .map(|name| {
                        Tool::new(
                            *name,
                            format!("The {name} tool."),
                            std::sync::Arc::clone(&schema),
                        )
                    })
                    .collect(),
            ))
        }

        async fn call_tool(
            &self,
            request: CallToolRequestParams,
            _context: RequestContext<RoleServer>,
        ) -> std::result::Result<CallToolResponse, ErrorData> {
            let arguments = request.arguments.unwrap_or_default();
            let text = match &*request.name {
                "sleep" => {
                    let secs = arguments
                        .get("secs")
                        .and_then(serde_json::Value::as_f64)
                        .unwrap_or(0.0);
                    tokio::time::sleep(Duration::from_secs_f64(secs)).await;
                    "slept".to_string()
                }
                "slow" => std::future::pending().await,
                _ => arguments
                    .get("text")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            };
            Ok(CallToolResult::success(vec![ContentBlock::text(text)]).into())
        }
    }

    /// What a call of one of a [`Toolbox`]'s tools does in place of
    /// answering at once.
    #[derive(Clone)]
    pub(crate) enum Act {
        /// Close its end of the transport on receipt, answering nothing: a
        /// server whose process died with the request in hand.
        HangUp,
        /// Answer nothing until the client cancels the call.
        Stall,
        /// Answer after this long, cancelled or not.
        After(Duration),
        /// Answer with this JSON-RPC error code and message.
        Fail(i32, &'static str),
    }

    /// A server whose `tools/list` page and `tools/call` results are JSON a
    /// test sets, sent as they are -- what no typed server can send, such as a
    /// tool whose `inputSchema` is not an object. It records each call under
    /// the name it was made by, and each cancel by the name of the call it
    /// names; can tell its client its listing changed; and can make a tool
    /// [`Act`] otherwise than answering.
    #[derive(Default)]
    pub(crate) struct Toolbox {
        /// The one page `tools/list` answers: `{"tools": [...]}`, and whatever
        /// else a test puts beside it.
        page: Mutex<Value>,
        /// What a call of each tool answers; one not here echoes its `text`.
        results: Mutex<HashMap<String, Value>>,
        /// Each call, by its exact name, with its arguments.
        calls: Mutex<Vec<(String, Value)>>,
        /// The client, once it has asked anything.
        peer: Mutex<Option<rmcp::Peer<RoleServer>>>,
        /// Whether the next `tools/list` is answered after announcing a change.
        announce: AtomicBool,
        /// What a call of each tool does in place of answering at once.
        acts: Mutex<HashMap<String, Act>>,
        /// Each call's tool, by its request id, for the cancels that name it.
        requests: Mutex<HashMap<RequestId, String>>,
        /// Each `notifications/cancelled` the client sent, by the tool its
        /// request called.
        cancels: Mutex<Vec<String>>,
        /// Cancelled to close its end of the transport.
        pub(crate) hang_up: CancellationToken,
    }

    impl Toolbox {
        /// A server listing `tools`, each a descriptor as the wire has it.
        pub(crate) fn new(tools: Value) -> Arc<Self> {
            let toolbox = Self::default();
            toolbox.list(tools);
            Arc::new(toolbox)
        }

        /// List `tools` from now on.
        pub(crate) fn list(&self, tools: Value) {
            self.page_as(serde_json::json!({ "tools": tools }));
        }

        /// Answer `tools/list` with `page`, whole.
        pub(crate) fn page_as(&self, page: Value) {
            *self.page.lock().unwrap() = page;
        }

        /// Answer each call of `tool` with `result`.
        pub(crate) fn answer(&self, tool: &str, result: Value) {
            self.results
                .lock()
                .unwrap()
                .insert(tool.to_string(), result);
        }

        /// Make each call of `tool` do `act` in place of answering at once.
        pub(crate) fn act(&self, tool: &str, act: Act) {
            self.acts.lock().unwrap().insert(tool.to_string(), act);
        }

        /// The cancels the client sent, each by the tool its request called.
        pub(crate) fn cancels(&self) -> Vec<String> {
            self.cancels.lock().unwrap().clone()
        }

        /// The calls made, in order, each by the name it was made under.
        pub(crate) fn calls(&self) -> Vec<String> {
            let calls = self.calls.lock().unwrap();
            calls.iter().map(|(name, _)| name.clone()).collect()
        }

        /// The arguments of each call made, in order.
        pub(crate) fn arguments(&self) -> Vec<Value> {
            let calls = self.calls.lock().unwrap();
            calls.iter().map(|(_, args)| args.clone()).collect()
        }

        /// Send the client `notifications/tools/list_changed` while answering
        /// the next `tools/list`, before its page.
        pub(crate) fn announce_while_listing(&self) {
            self.announce.store(true, Ordering::SeqCst);
        }

        /// Send the client `notifications/tools/list_changed`.
        pub(crate) async fn list_changed(&self) {
            let peer = self.peer.lock().unwrap().clone();
            peer.expect("the client has asked something")
                .notify_tool_list_changed()
                .await
                .expect("notify the client");
        }
    }

    /// A [`Toolbox`] as rmcp serves it: its own answers for `tools/list` and
    /// `tools/call`, and a plain server's for everything else.
    pub(crate) struct Raw(pub(crate) Arc<Toolbox>);

    /// What a [`Raw`] server is besides its tools.
    struct Plain;

    impl rmcp::ServerHandler for Plain {
        fn get_info(&self) -> ServerConfig {
            ServerConfig::new(
                ServerCapabilities::builder()
                    .enable_tools()
                    .enable_tool_list_changed()
                    .build(),
            )
        }
    }

    impl Toolbox {
        /// The page `tools/list` answers, a change announced to the client first when asked
        /// for.
        async fn listed(&self, context: &RequestContext<RoleServer>) -> Value {
            *self.peer.lock().unwrap() = Some(context.peer.clone());
            if self.announce.swap(false, Ordering::SeqCst) {
                context
                    .peer
                    .notify_tool_list_changed()
                    .await
                    .expect("notify the client");
            }
            self.page.lock().unwrap().clone()
        }

        /// What a call of `name` with `arguments` answers, once recorded and its tool's act
        /// done.
        async fn called(
            &self,
            name: String,
            arguments: Value,
            context: &RequestContext<RoleServer>,
        ) -> std::result::Result<Value, ErrorData> {
            *self.peer.lock().unwrap() = Some(context.peer.clone());
            let text = arguments["text"].as_str().unwrap_or_default().to_string();
            self.calls.lock().unwrap().push((name.clone(), arguments));
            self.requests
                .lock()
                .unwrap()
                .insert(context.id.clone(), name.clone());
            let act = self.acts.lock().unwrap().get(&name).cloned();
            match act {
                None => {}
                Some(Act::HangUp) => {
                    self.hang_up.cancel();
                    return std::future::pending().await;
                }
                Some(Act::Stall) => {
                    context.ct.cancelled().await;
                    return Err(ErrorData::internal_error("cancelled", None));
                }
                Some(Act::After(delay)) => tokio::time::sleep(delay).await,
                Some(Act::Fail(code, message)) => {
                    return Err(ErrorData::new(ErrorCode(code), message, None));
                }
            }
            let result = self.results.lock().unwrap().get(&name).cloned();
            Ok(result.unwrap_or_else(
                || serde_json::json!({"content": [{"type": "text", "text": text}]}),
            ))
        }

        /// Record a cancel the client sent of `request`, by the tool it called.
        fn cancelled(&self, request: Option<&RequestId>) {
            let tool = request
                .and_then(|id| self.requests.lock().unwrap().get(id).cloned())
                .unwrap_or_else(|| "an unknown request".to_string());
            self.cancels.lock().unwrap().push(tool);
        }
    }

    impl rmcp::Service<RoleServer> for Raw {
        async fn handle_request(
            &self,
            request: ClientRequest,
            context: RequestContext<RoleServer>,
        ) -> std::result::Result<ServerResult, ErrorData> {
            *self.0.peer.lock().unwrap() = Some(context.peer.clone());
            let answered = match request {
                ClientRequest::ListToolsRequest(_) => self.0.listed(&context).await,
                ClientRequest::CallToolRequest(call) => {
                    let arguments = Value::Object(call.params.arguments.unwrap_or_default());
                    let name = call.params.name.to_string();
                    self.0.called(name, arguments, &context).await?
                }
                other => return rmcp::Service::handle_request(&Plain, other, context).await,
            };
            Ok(ServerResult::CustomResult(CustomResult(answered)))
        }

        async fn handle_notification(
            &self,
            notification: ClientNotification,
            context: NotificationContext<RoleServer>,
        ) -> std::result::Result<(), ErrorData> {
            if let ClientNotification::CancelledNotification(cancelled) = &notification {
                self.0.cancelled(cancelled.params.request_id.as_ref());
            }
            rmcp::Service::handle_notification(&Plain, notification, context).await
        }

        fn get_info(&self) -> ServerConfig {
            rmcp::Service::get_info(&Plain)
        }
    }

    /// A [`Toolbox`] as a typed rmcp server answers it -- what rmcp's streamable-HTTP server
    /// takes -- its pages and results decoded: what the raw form sends as it is, this one
    /// cannot.
    pub(crate) struct Served(pub(crate) Arc<Toolbox>);

    impl rmcp::ServerHandler for Served {
        fn get_info(&self) -> ServerConfig {
            rmcp::ServerHandler::get_info(&Plain)
        }

        async fn list_tools(
            &self,
            _request: Option<PaginatedRequestParams>,
            context: RequestContext<RoleServer>,
        ) -> std::result::Result<ListToolsResult, ErrorData> {
            let page = self.0.listed(&context).await;
            serde_json::from_value(page).map_err(|e| ErrorData::internal_error(e.to_string(), None))
        }

        async fn call_tool(
            &self,
            request: CallToolRequestParams,
            context: RequestContext<RoleServer>,
        ) -> std::result::Result<CallToolResponse, ErrorData> {
            let arguments = Value::Object(request.arguments.unwrap_or_default());
            let answered = self
                .0
                .called(request.name.to_string(), arguments, &context)
                .await?;
            let result: CallToolResult = serde_json::from_value(answered)
                .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
            Ok(result.into())
        }

        async fn on_cancelled(
            &self,
            notification: CancelledNotificationParam,
            _context: NotificationContext<RoleServer>,
        ) {
            self.0.cancelled(notification.request_id.as_ref());
        }
    }

    /// A server that answers `initialize` and never lists its tools.
    struct Unlisting;

    impl rmcp::ServerHandler for Unlisting {
        fn get_info(&self) -> ServerConfig {
            ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
        }

        async fn list_tools(
            &self,
            _request: Option<PaginatedRequestParams>,
            _context: RequestContext<RoleServer>,
        ) -> std::result::Result<ListToolsResult, ErrorData> {
            std::future::pending().await
        }
    }

    /// A server that never answers `initialize`.
    struct Hanging;

    impl rmcp::ServerHandler for Hanging {
        async fn initialize(
            &self,
            _request: InitializeRequestParams,
            _context: RequestContext<RoleServer>,
        ) -> std::result::Result<InitializeResult, ErrorData> {
            std::future::pending().await
        }
    }

    /// A server that answers `initialize` with an error.
    struct Refusing;

    impl rmcp::ServerHandler for Refusing {
        async fn initialize(
            &self,
            _request: InitializeRequestParams,
            _context: RequestContext<RoleServer>,
        ) -> std::result::Result<InitializeResult, ErrorData> {
            Err(ErrorData::invalid_request(
                "this client is not welcome",
                None,
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::outrig_::SharedNetwork;

    /// A sidecar placement whose session's interceptor is `network`, holding
    /// `container`.
    async fn holding(network: Option<SharedNetwork>, container: Container) -> Sidecar {
        let placement = Sidecar::new(
            SidecarHost::bare(network),
            SidecarSpec::from_image("tools", "mcp-image"),
        );
        *placement.container.lock().await = Some(container);
        placement
    }

    /// A container that did not stop keeps its filter: nothing detaches it, so
    /// what still runs there reaches only what the policy allows, and is
    /// audited; the stop says it failed and keeps the container for the next.
    #[tokio::test]
    async fn a_sidecar_that_did_not_stop_stays_attached() {
        let network: SharedNetwork = Arc::new(tokio::sync::Mutex::new(None));
        let placement = holding(Some(Arc::clone(&network)), Container::unstoppable()).await;
        // Held, so a detach would wait on it for good.
        let _attaching = network.lock().await;
        let stopped = tokio::time::timeout(Duration::from_secs(5), placement.stop())
            .await
            .expect("nothing detached the container");
        assert!(stopped.is_err());
        assert_eq!(placement.abandoned.lock().await.len(), 1);
    }

    /// A recovery after a stop that failed -- an earlier recovery's, of a
    /// container that was gone -- tries that stop again before the connect
    /// after it prepares a container under the same name.
    #[tokio::test]
    async fn a_recovery_finishes_a_stop_that_failed() {
        let marks = tempfile::tempdir().expect("tempdir");
        let container = Container::stops_the_second_time(&marks.path().join("asked"));
        let placement = holding(None, container).await;
        assert!(placement.stop().await.is_err(), "the first stop fails");
        assert_eq!(placement.abandoned.lock().await.len(), 1);
        placement.recover().await.expect("the stop was made again");
        assert!(placement.abandoned.lock().await.is_empty());
    }

    /// A recovery after a stop dropped at its detach makes that detach before
    /// the connect after it attaches a container under the same name; once.
    #[tokio::test]
    async fn a_recovery_makes_the_detach_a_dropped_stop_owed() {
        let network: SharedNetwork = Arc::new(tokio::sync::Mutex::new(None));
        let placement = holding(Some(Arc::clone(&network)), Container::stops_cleanly()).await;
        let cut = Duration::from_millis(200);
        // Held, so a detach waits on it until it is let go.
        let attaching = network.lock().await;
        let stopping = tokio::time::timeout(cut, placement.stop()).await;
        assert!(stopping.is_err(), "the stop was cut off at its detach");
        assert!(placement.abandoned.lock().await.is_empty(), "it stopped");
        let recovering = tokio::time::timeout(cut, placement.recover()).await;
        assert!(recovering.is_err(), "the recovery waits on the detach");
        drop(attaching);
        let recovered = tokio::time::timeout(Duration::from_secs(5), placement.recover()).await;
        assert!(matches!(recovered, Ok(Ok(()))), "{recovered:?}");
        let _attaching = network.lock().await;
        let again = tokio::time::timeout(cut, placement.recover()).await;
        assert!(matches!(again, Ok(Ok(()))), "made once: {again:?}");
    }

    /// A stop dropped part-way -- an owner's timeout -- leaves the container
    /// held, for the next stop to find and prove, not to `Drop`'s removal,
    /// which proves nothing.
    #[tokio::test]
    async fn a_stop_dropped_part_way_keeps_the_container() {
        let placement = holding(None, Container::slow_to_stop()).await;
        let dropped = tokio::time::timeout(Duration::from_millis(200), placement.stop()).await;
        assert!(dropped.is_err(), "the stop was cut off");
        assert_eq!(placement.abandoned.lock().await.len(), 1);
    }
}
