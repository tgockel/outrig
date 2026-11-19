//! Where a session's MCP servers run: the primary container, or a sidecar of
//! the session's own -- one each for a server an owner declared, one shared by
//! every server a configuration's `[sidecars.<sc>]` block hosts.
//!
//! A [`Placement`] starts what a server runs in when it is not running,
//! connects the server, and freezes, thaws and stops what it started. The MCP
//! table ([`super::mcp`]) holds one per container and decides everything else
//! -- which servers share one, what a failure means, when to try again -- so a
//! test drives the table with a fake placement and no container.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use futures_util::future::BoxFuture;
use tokio::sync::Mutex;

use crate::config::ResolvedEnvValue;
use crate::container::{Container, ExecTarget};
use crate::error::{OutrigError, Result};
use crate::mcp::McpClient;
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
    ) -> BoxFuture<'a, Result<McpClient>>;

    /// Freeze every process the placement runs, or say why not.
    fn freeze(&self) -> BoxFuture<'_, std::result::Result<(), String>>;

    /// Thaw what [`Placement::freeze`] froze, or say why not.
    fn thaw(&self) -> BoxFuture<'_, std::result::Result<(), String>>;

    /// Stop what the placement started, or say what was not proven stopped.
    fn stop(&self) -> BoxFuture<'_, std::result::Result<(), String>>;
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
    ) -> BoxFuture<'a, Result<McpClient>> {
        Box::pin(async move {
            let Launch::Exec(command) = &server.launch else {
                return Err(OutrigError::Configuration(format!(
                    "mcp server {:?}: the primary runs no entrypoint server",
                    server.name
                )));
            };
            server
                .connect_exec(&self.target, command, &self.log_dir, within)
                .await
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

    /// Stop every container this placement holds -- `held`, and each an
    /// earlier stop could not -- each in place, so one stays held until its
    /// stop is proven: a stop that fails, or is dropped part-way, leaves it
    /// for the next. The interceptor is detached only once every one has
    /// stopped, so one still running stays filtered and audited. Each stop
    /// that failed, when any did.
    async fn stop_all(
        &self,
        held: &mut Option<Container>,
        abandoned: &mut Vec<Container>,
    ) -> std::result::Result<(), Vec<OutrigError>> {
        // With no await between, so nothing is held by this frame alone.
        abandoned.extend(held.take());
        let mut failed = Vec::new();
        let mut stopped = false;
        let mut next = 0;
        while let Some(container) = abandoned.get_mut(next) {
            match container.stop_in_place(SHUTDOWN_GRACE).await {
                Ok(()) => {
                    abandoned.remove(next);
                    stopped = true;
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
        if stopped && let Err(e) = self.host.detach(&self.name).await {
            tracing::warn!(target: "outrig::mcp", "sidecar {:?}: {e}", self.name);
        }
        Ok(())
    }
}

impl Placement for Sidecar {
    fn connect<'a>(
        &'a self,
        server: &'a Server,
        within: Duration,
    ) -> BoxFuture<'a, Result<McpClient>> {
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
                    server
                        .connect_exec(&target, command, &log_dir, left())
                        .await
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
                    connected
                }
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

    fn stop(&self) -> BoxFuture<'_, std::result::Result<(), String>> {
        Box::pin(async move {
            let mut held = self.container.lock().await;
            let mut abandoned = self.abandoned.lock().await;
            self.stop_all(&mut held, &mut abandoned)
                .await
                .map_err(|failed| {
                    failed
                        .iter()
                        .map(|e| format!("sidecar {:?}: {e}", self.name))
                        .collect::<Vec<_>>()
                        .join("; ")
                })
        })
    }
}

/// A placement with no container behind it, whose servers are rmcp servers
/// in memory, for the MCP table's tests.
#[cfg(test)]
pub(crate) mod fake {
    use std::collections::{HashMap, VecDeque};
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use futures_util::future::BoxFuture;
    use rmcp::model::{
        CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, ErrorData,
        InitializeRequestParams, InitializeResult, ListToolsResult, PaginatedRequestParams,
        ServerCapabilities, ServerConfig, Tool,
    };
    use rmcp::service::{RequestContext, RoleServer};

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
    }

    impl Placement for Fake {
        fn connect<'a>(
            &'a self,
            server: &'a Server,
            within: Duration,
        ) -> BoxFuture<'a, Result<McpClient>> {
            Box::pin(async move {
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
                        return McpClient::handshake_in_memory(&server.name, Refusing, within)
                            .await;
                    }
                    Behavior::RefuseAtBound => {
                        tokio::time::sleep(within).await;
                        return McpClient::handshake_in_memory(&server.name, Refusing, within)
                            .await;
                    }
                    Behavior::Wedged => return std::future::pending().await,
                    Behavior::Unlisted => {
                        return McpClient::handshake_in_memory(&server.name, Unlisting, within)
                            .await;
                    }
                    Behavior::Script(script) => {
                        let log = tempfile::tempdir().expect("tempdir");
                        let log_dir = log.path().to_path_buf();
                        self.logs.lock().unwrap().push(log);
                        return McpClient::connect_script(&server.name, script, &log_dir, within)
                            .await;
                    }
                    Behavior::Exit(code, stderr) => {
                        let command = match &server.launch {
                            Launch::Exec(command) => command.clone(),
                            Launch::Entrypoint(args) => args.clone(),
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
            })
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
