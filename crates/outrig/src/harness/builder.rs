//! Everything a session needs from outside, given before it starts, and the
//! one call that starts it.

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock, Weak};

use super::approval::Approver;
use super::bindings::{self, BindingSpec, Bindings, Brief, Declared, Places, Setup};
use super::event::{DEFAULT_CAPACITY, Subscription};
use super::lifecycle::Lifecycle;
use super::{Failure, Session, SessionError};
use crate::agent::ledger::Ledger;
use crate::agent::{self, Agent, AgentError, ResolvedAgent};
use crate::config::{Config, ConfigValidationError, EnvSecrets, ImageConfig, Secrets, lexical};
use crate::container::ContainerMount;
use crate::error::{IoPathExt, OutrigError};
use crate::events::{Events, StreamBuilder};
use crate::image::ImageTag;
use crate::outrig_::resolve_session_id;
use crate::python::host::Interpreter;
use crate::{EmbeddedMcpPolicy, LaunchSpec, Outrig};

/// A step of a session's start, as a progress callback is told of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Step {
    /// Installing the packages the session's bindings require, each set once
    /// into the user's cache. Reported only when the session has a binding.
    Packages,
    /// Starting each binding's process on the host and running its factory.
    /// Reported only when the session has a binding.
    Bindings,
    /// Launching the container.
    Container,
    /// Starting the Python interpreter in it.
    Python,
}

/// What a progress callback is told.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Progress {
    Started(Step),
    Finished(Step),
}

/// What a session needs from outside, given before it starts: the
/// configuration, the agent and the model to run, the container, the secrets
/// its model calls need, its bindings and who approves a repository's, who
/// subscribes to its events, and whether it records them.
/// [`SessionBuilder::start`] starts it, in one call.
///
/// ```no_run
/// # async fn example(
///     config: outrig::config::Config,
/// ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
/// use outrig::harness::{DEFAULT_DRAIN, SessionBuilder};
///
/// let mut builder = SessionBuilder::new(config, Some("coding"), None)
///     .secrets(|var: &str| (var == "ANTHROPIC_API_KEY").then(|| "sk-...".to_string()));
/// let mut events = builder.subscribe();
/// let mut session = builder.start().await?;
/// session.user_channel().send("summarize the README").await?;
/// let outcome = session.round().await?;
/// session.close_admission();
/// let report = session.shutdown(DEFAULT_DRAIN).await;
/// # let _ = (outcome, report, events.try_recv());
/// # Ok(())
/// # }
/// ```
pub struct SessionBuilder {
    config: Config,
    agent: Option<String>,
    model: Option<String>,
    container: Option<LaunchSpec>,
    secrets: Arc<dyn Secrets>,
    /// The resolution [`SessionBuilder::check`] made, for `start` to use
    /// rather than ask the resolver again.
    resolved: OnceLock<ResolvedAgent>,
    stream: StreamBuilder,
    record: bool,
    progress: Option<Box<dyn FnMut(Progress) + Send>>,
    /// Bindings given through [`SessionBuilder::bind`], as given.
    bound: Vec<(String, BindingSpec)>,
    approver: Option<Arc<dyn Approver>>,
    repo_root: Option<PathBuf>,
    /// Where a test's bindings install and remember approvals, in place of
    /// the user's own.
    #[cfg(test)]
    places: Option<Places>,
    /// Where a host session's log goes, for a test.
    #[cfg(test)]
    host_log: Option<PathBuf>,
    /// The stream a test built itself, in place of the builder's.
    #[cfg(test)]
    events: Option<Events>,
}

impl SessionBuilder {
    /// A session of `agent` -- an `[agents.<name>]` block, or `None` for one
    /// that names none -- over `config`. `model` overrides the model the agent
    /// or `default-model` would choose.
    ///
    /// By default its keys are read from the process environment, nothing
    /// subscribes, nothing is recorded, and the container is the one
    /// [`Session::start`] describes.
    pub fn new(config: Config, agent: Option<&str>, model: Option<&str>) -> Self {
        Self {
            config,
            agent: agent.map(str::to_string),
            model: model.map(str::to_string),
            container: None,
            secrets: Arc::new(EnvSecrets),
            resolved: OnceLock::new(),
            stream: StreamBuilder::default(),
            record: false,
            progress: None,
            bound: Vec::new(),
            approver: None,
            repo_root: None,
            #[cfg(test)]
            places: None,
            #[cfg(test)]
            host_log: None,
            #[cfg(test)]
            events: None,
        }
    }

    /// Launch `spec` as the session's container, once its model and keys have
    /// resolved. A spec that names no session id is given one, which names the
    /// container and the session's event log alike. Its image must already be
    /// present, or buildable by [`Outrig::launch`].
    pub fn container(mut self, spec: LaunchSpec) -> Self {
        self.container = Some(spec);
        self
    }

    /// Resolve each `${VAR}` the session's model calls need through
    /// `secrets`, in place of the process environment.
    pub fn secrets(mut self, secrets: impl Secrets + 'static) -> Self {
        self.secrets = Arc::new(secrets);
        self.resolved = OnceLock::new();
        self
    }

    /// Record the session's events in `events.jsonl` in the container's log
    /// directory, readable by its owner alone. A log that cannot be opened, or
    /// already holds a recording, fails the start before anything starts.
    pub fn record_events(mut self) -> Self {
        self.record = true;
        self
    }

    /// Call `progress` as each step of the start begins and ends.
    pub fn progress(mut self, progress: impl FnMut(Progress) + Send + 'static) -> Self {
        self.progress = Some(Box::new(progress));
        self
    }

    /// Host the object `spec`'s factory builds as `name`, beside the
    /// configuration's `[bindings]`, with no approval asked: the owner is
    /// trusted as the operator is.
    ///
    /// Its process starts on the host before the interpreter, in the
    /// workspace's host directory, with `spec`'s environment or else the
    /// owner's less every variable the configuration names through `${VAR}`.
    /// A name that is not a Python identifier, that is a keyword, `runtime`,
    /// `asyncio` or `outrig`, or that the configuration or another `bind`
    /// already uses, fails the start.
    ///
    /// When the session has any binding, every directory it mounts appears
    /// in the container at its host path, so a path means one file to the
    /// agent and to the library: the workspace's container path is replaced
    /// by its host path, and one the configuration declared otherwise, or a
    /// mount whose container path differs from its host path, fails the
    /// start.
    pub fn bind(mut self, name: impl Into<String>, spec: impl Into<BindingSpec>) -> Self {
        self.bound.push((name.into(), spec.into()));
        self
    }

    /// Ask `approver` whether a binding a repository's config declared may
    /// start, for each whose exact declaration has not been approved in this
    /// repository before. Without one, no such binding starts, and the
    /// session says so in a `binding.refused` event.
    pub fn approver(mut self, approver: impl Approver + 'static) -> Self {
        self.approver = Some(Arc::new(approver));
        self
    }

    /// The repository the session works on: what a relative `{ path }` in a
    /// binding's arguments resolves against, whichever file declared it,
    /// what a repository's approval is for, and -- for the container
    /// [`Session::start`] describes -- what repository-relative paths in the
    /// configuration resolve against. The current directory by default; a
    /// relative root resolves against the current directory when the session
    /// starts.
    pub fn repo_root(mut self, root: impl Into<PathBuf>) -> Self {
        self.repo_root = Some(root.into());
        self
    }

    /// A subscription to the session's events, from its first, holding up to
    /// [`DEFAULT_CAPACITY`] of them for its reader.
    pub fn subscribe(&mut self) -> Subscription {
        self.subscribe_with_capacity(DEFAULT_CAPACITY)
    }

    /// A subscription holding up to `capacity` events for its reader. A reader
    /// that falls further behind loses the oldest, and is told how many.
    ///
    /// # Panics
    ///
    /// If `capacity` is zero.
    pub fn subscribe_with_capacity(&mut self, capacity: usize) -> Subscription {
        self.stream.subscribe(capacity)
    }

    /// Whether [`SessionBuilder::start`] would resolve the model and every key
    /// it needs, checked without starting anything -- so a caller can fail on
    /// a configuration that names no usable model before it pulls or builds an
    /// image.
    pub fn check(&self) -> Result<(), SessionError> {
        self.resolved()?;
        bindings::gather(&self.config, &self.bound).map_err(start_error)?;
        Ok(())
    }

    /// The model and its keys, resolved once: a resolver backed by a secret
    /// store is asked once a session, however often this is.
    fn resolved(&self) -> Result<&ResolvedAgent, SessionError> {
        if let Some(resolved) = self.resolved.get() {
            return Ok(resolved);
        }
        let resolved = agent::resolve(
            &self.config,
            self.agent.as_deref(),
            self.model.as_deref(),
            &*self.secrets,
        )
        .map_err(|e| SessionError::Resolve(Failure(e)))?;
        Ok(self.resolved.get_or_init(|| resolved))
    }

    /// [`SessionBuilder::resolved`], taken for the start.
    fn take_resolved(&mut self) -> Result<ResolvedAgent, SessionError> {
        self.resolved()?;
        Ok(self.resolved.take().expect("resolved by the line above"))
    }

    fn report(&mut self, progress: Progress) {
        if let Some(report) = &mut self.progress {
            report(progress);
        }
    }

    /// Start the session: resolve its model, approve, install and start its
    /// bindings, launch its container, start the interpreter in it, and build
    /// the agent. On failure nothing it started is left running.
    pub async fn start(mut self) -> Result<Session, SessionError> {
        let resolved = self.take_resolved()?;
        let declared = bindings::gather(&self.config, &self.bound).map_err(start_error)?;
        let repo_root = self.chosen_repo_root().map_err(start_error)?;
        let mut spec = match self.container.take() {
            Some(spec) => spec,
            None => default_container(&self.config, self.agent.as_deref(), &repo_root)
                .await
                .map_err(start_error)?,
        };
        let id = resolve_session_id(&spec).map_err(start_error)?;
        spec.session_id = Some(id.clone());
        let (events, lifecycle) = self.open_stream(&id, &spec.log_dir).await?;

        let cwd = spec
            .workspace
            .as_ref()
            .map_or_else(|| repo_root.clone(), |workspace| lexical(&workspace.host));
        let mut bindings = self
            .start_bindings(declared, &repo_root, &cwd, &events)
            .await
            .map_err(start_error)?;
        if !bindings.is_empty()
            && let Err(e) = same_paths(&self.config, &mut spec, &bindings)
        {
            bindings.stop_after_failed_start().await;
            return Err(start_error(e));
        }

        self.report(Progress::Started(Step::Container));
        let outrig = match Outrig::launch(&spec).await {
            Ok(outrig) => outrig,
            Err(e) => {
                bindings.stop_after_failed_start().await;
                return Err(start_error(e));
            }
        };
        self.report(Progress::Finished(Step::Container));

        self.report(Progress::Started(Step::Python));
        let interpreter = match Interpreter::start(outrig.primary(), events.clone()).await {
            Ok(interpreter) => interpreter,
            Err(e) => {
                stop(outrig, &mut bindings).await;
                return Err(SessionError::Start(Failure(e.into())));
            }
        };
        bindings.declare(&interpreter, bindings::container_path);
        self.report(Progress::Finished(Step::Python));

        let primary = outrig.primary();
        // A container launched without a workspace holds an empty path.
        let workspace = Some(primary.container_workspace())
            .filter(|workspace| !workspace.as_os_str().is_empty())
            .map(Path::to_path_buf);
        let name = primary.name().to_string();
        match assemble(
            &resolved,
            interpreter,
            &name,
            workspace.as_deref(),
            &bindings.briefs(),
            &events,
            &lifecycle,
        ) {
            Ok(agent) => Ok(Session::new(id, agent, Some(outrig), lifecycle, bindings)),
            Err(e) => {
                stop(outrig, &mut bindings).await;
                Err(SessionError::Start(Failure(e)))
            }
        }
    }

    /// The repository root the owner gave, made absolute against the current
    /// directory, or the current directory.
    fn chosen_repo_root(&self) -> Result<PathBuf, OutrigError> {
        match &self.repo_root {
            // Before anything keys on it: a relative root names another
            // repository from another directory, under the same approval.
            Some(root) => std::path::absolute(root).path_ctx("resolve", root),
            None => std::env::current_dir().path_ctx("read", "."),
        }
    }

    /// `declared`, less the repository's bindings not approved, installed and
    /// started, each step reported as progress when there is any binding.
    async fn start_bindings(
        &mut self,
        declared: Vec<Declared>,
        repo_root: &Path,
        cwd: &Path,
        events: &Events,
    ) -> Result<Bindings, OutrigError> {
        if declared.is_empty() {
            return Ok(Bindings::default());
        }
        #[cfg(test)]
        let places = self.places.clone().map_or_else(Places::user, Ok)?;
        #[cfg(not(test))]
        let places = Places::user()?;
        let setup = Setup {
            config: &self.config,
            repo_root,
            cwd,
            approver: self.approver.clone(),
            places,
            events,
        };
        let declared = bindings::approve(declared, &setup).await;
        if declared.is_empty() {
            return Ok(Bindings::default());
        }
        let progress = &mut self.progress;
        let mut report = |step| {
            if let Some(report) = progress.as_mut() {
                report(step);
            }
        };
        report(Progress::Started(Step::Packages));
        let packages = bindings::install(&declared, &setup).await?;
        report(Progress::Finished(Step::Packages));
        report(Progress::Started(Step::Bindings));
        let started = bindings::start(declared, packages, &setup).await?;
        report(Progress::Finished(Step::Bindings));
        Ok(started)
    }

    /// The session's stream, opened before anything starts so its first
    /// event says it is starting: its subscriptions, and its log when it
    /// records one, in `log_dir` for the session `id`.
    async fn open_stream(
        &mut self,
        id: &str,
        log_dir: &Path,
    ) -> Result<(Events, Arc<Lifecycle>), SessionError> {
        #[cfg(test)]
        if let Some(events) = self.events.take() {
            let lifecycle = Lifecycle::starting(events.clone());
            return Ok((events, lifecycle));
        }
        let mut stream = std::mem::take(&mut self.stream);
        if self.record {
            stream
                .record(log_dir, format!("/outrig/session/{id}"))
                .await
                .map_err(|e| SessionError::Start(Failure(e.into())))?;
        }
        let events = stream.build();
        let lifecycle = Lifecycle::starting(events.clone());
        Ok((events, lifecycle))
    }

    /// Give a host session `dir` as its log directory, which it records in
    /// when asked to.
    #[cfg(test)]
    pub(crate) fn log_in(mut self, dir: &Path) -> Self {
        self.host_log = Some(dir.to_path_buf());
        self
    }

    /// Publish to `events`, a stream the test built, in place of the
    /// builder's own subscriptions and log.
    #[cfg(test)]
    pub(crate) fn events(mut self, events: Events) -> Self {
        self.events = Some(events);
        self
    }

    /// Install the session's requirement sets under `root`, with `pip_env`
    /// added to pip's environment, and remember approvals in `approvals`, in
    /// place of the user's cache, index and state directory.
    #[cfg(test)]
    pub(crate) fn binding_places(
        mut self,
        root: &Path,
        pip_env: Vec<(std::ffi::OsString, std::ffi::OsString)>,
        approvals: &Path,
    ) -> Self {
        self.places = Some(Places {
            packages_root: root.to_path_buf(),
            pip_env,
            store: Some(super::approval::Store::at(approvals)),
        });
        self
    }

    /// [`SessionBuilder::start`] over an interpreter on the host, which has no
    /// container and no workspace: bindings run in the repository root, and
    /// the interpreter imports their packages from where they are on the
    /// host. The session's id is `test`.
    #[cfg(test)]
    pub(crate) async fn start_on_host(mut self) -> Result<Session, SessionError> {
        let resolved = self.take_resolved()?;
        let declared = bindings::gather(&self.config, &self.bound).map_err(start_error)?;
        let repo_root = self.chosen_repo_root().map_err(start_error)?;
        let id = "test".to_string();
        let log_dir = self.host_log.clone().unwrap_or_default();
        let (events, lifecycle) = self.open_stream(&id, &log_dir).await?;
        let mut bindings = self
            .start_bindings(declared, &repo_root, &repo_root, &events)
            .await
            .map_err(start_error)?;
        self.report(Progress::Started(Step::Python));
        let interpreter = crate::python::testing::start_on_host_with(events.clone()).await;
        bindings.declare(&interpreter, Path::to_path_buf);
        self.report(Progress::Finished(Step::Python));
        match assemble(
            &resolved,
            interpreter,
            "(host)",
            None,
            &bindings.briefs(),
            &events,
            &lifecycle,
        ) {
            Ok(agent) => Ok(Session::new(id, agent, None, lifecycle, bindings)),
            Err(e) => {
                bindings.stop_after_failed_start().await;
                Err(SessionError::Start(Failure(e)))
            }
        }
    }
}

/// The container [`Session::start`] describes, for `config` and the agent
/// named `agent`.
///
/// Given the configuration rather than the builder: a borrow held across an
/// await is the future's, and the builder's progress callback is not `Sync`,
/// so borrowing it would leave `start`'s future unable to cross threads.
async fn default_container(
    config: &Config,
    agent: Option<&str>,
    repo_root: &Path,
) -> Result<LaunchSpec, OutrigError> {
    let agent = agent.and_then(|name| config.agents.get(name));
    let image = agent
        .and_then(|agent| agent.image.as_deref())
        .or(config.default_image.as_deref())
        .ok_or_else(|| {
            OutrigError::Configuration(
                "no image to run the session in: name one with the agent's `image` or \
                 `default-image`, or give the session a container"
                    .to_string(),
            )
        })?;
    let id = crate::container::mint_session_id();
    let log_dir = config
        .session_root
        .clone()
        .unwrap_or_else(|| std::env::temp_dir().join("outrig-sessions"))
        .join(&id)
        .join("logs");
    tokio::fs::create_dir_all(&log_dir)
        .await
        .path_ctx("create directory", &log_dir)?;
    let ContainerSpec { spec, .. } =
        container_spec(config, image, None, repo_root, log_dir).await?;
    // Only a workspace the configuration declares is mounted: a library
    // never mounts whatever directory its caller happens to run in.
    let spec = match config.workspace.declared_host_path() {
        Some(_) => spec,
        None => spec.without_workspace(),
    };
    Ok(spec.with_session_id(id))
}

/// The agent over `interpreter`, its admission the session's, and the session
/// idle.
fn assemble(
    resolved: &ResolvedAgent,
    interpreter: Interpreter,
    container_name: &str,
    workspace: Option<&Path>,
    bindings: &[Brief],
    events: &Events,
    lifecycle: &Arc<Lifecycle>,
) -> Result<Agent, AgentError> {
    lifecycle.attach(interpreter.gate());
    // Weak: the interpreter outlives nothing the session holds, and a strong
    // reference from it back to the session would.
    let weak: Weak<Lifecycle> = Arc::downgrade(lifecycle);
    interpreter.on_exit(move || {
        if let Some(lifecycle) = weak.upgrade() {
            lifecycle.exited();
        }
    });
    let agent = Agent::build(
        resolved,
        interpreter,
        container_name,
        workspace,
        bindings,
        Ledger::new(events.clone()),
        Arc::clone(lifecycle),
    )?;
    lifecycle.set(super::event::SessionState::Idle);
    Ok(agent)
}

/// Stop what a failed start launched -- its bindings, then its container --
/// saying so if that fails too.
async fn stop(outrig: Outrig, bindings: &mut Bindings) {
    bindings.stop_after_failed_start().await;
    for failed in outrig.stop().await {
        tracing::warn!(target: "outrig::harness", "stopping a session that failed to start: {failed}");
    }
}

/// A start failure from OutRig's own error.
fn start_error(e: OutrigError) -> SessionError {
    SessionError::Start(Failure(AgentError::Outrig(e)))
}

/// `spec`, rewritten for a session with bindings: every directory it mounts
/// appears in the container at its host path, so a path the agent writes and
/// a path the library reads name one file, and each binding's package
/// directory is bound read-only beside the payload.
///
/// The workspace's container path becomes its host path, unless `config`
/// declared another, which is an error naming the key. A `LaunchSpec` cannot
/// say whether its container path was declared -- one from the configuration
/// holds `/workspace` when none was -- so the configuration decides. A mount
/// whose container path is not its host path is an error.
fn same_paths(
    config: &Config,
    spec: &mut LaunchSpec,
    bindings: &Bindings,
) -> Result<(), OutrigError> {
    let differs = |key: String, container: &Path, host: &Path, declared_in| {
        ConfigValidationError::BindingContainerPath {
            key,
            container: container.to_path_buf(),
            host: host.to_path_buf(),
            declared_in,
        }
    };
    if let Some(workspace) = &mut spec.workspace {
        let host = lexical(&workspace.host);
        if let Some(declared) = config.workspace.declared_container_path()
            && lexical(declared) != host
        {
            let declared_in = config.workspace.declared_in();
            Err(differs(
                "workspace.container-path".into(),
                declared,
                &host,
                declared_in,
            ))?;
        }
        workspace.container = host;
    }
    for mount in &spec.mounts {
        if lexical(&mount.container) != lexical(&mount.host) {
            let key = format!(
                "the container path of the mount of {}",
                mount.host.display()
            );
            Err(differs(key, &mount.container, &mount.host, None))?;
        }
    }
    spec.packages = bindings
        .package_dirs()
        .into_iter()
        .map(|dir| ContainerMount::shared_read_only(dir, bindings::container_path(dir)))
        .collect();
    Ok(())
}

/// A container for a session, and the MCP servers it leaves out.
#[non_exhaustive]
pub struct ContainerSpec {
    pub spec: LaunchSpec,
    /// The configured MCP servers the container does not start.
    pub left_out: Vec<String>,
}

/// The container a session runs in, for the `[images.<image>]` block of
/// `config`: its workspace, mounts, security and network as `config` declares
/// them, with repository-relative paths against `repo_root` and its logs in
/// `log_dir`. With `pinned`, the image is that already-ensured tag, keeping the
/// block's security; otherwise it is whatever the block names, which
/// [`Outrig::launch`] builds or runs.
///
/// **No MCP server and no sidecar starts**, including servers an image
/// declares in its own `org.outrig.mcp` label. The model's one tool submits
/// Python, so nothing could call them; and a server placed in the primary
/// would run beside the interpreter as the same user, with whatever secrets
/// its `env` resolved readable from Python wherever `/proc` allows. Not handing
/// the model a tool does not put a credential out of reach; not starting the
/// server does. The servers left out are named.
pub async fn container_spec(
    config: &Config,
    image: &str,
    pinned: Option<&ImageTag>,
    repo_root: &Path,
    log_dir: PathBuf,
) -> Result<ContainerSpec, OutrigError> {
    // Sidecars go before lowering rather than after: lowering builds or pulls
    // every sidecar image it plans, for containers that would never start.
    let mut config = config.clone();
    config.sidecars.clear();
    let mut left_out = Vec::new();
    if let Some(block) = config.images.get_mut(image) {
        left_out = block.mcp.keys().cloned().collect();
        match pinned {
            Some(tag) => {
                let security = std::mem::take(&mut block.security);
                *block = ImageConfig::from_image_name(tag.as_str());
                block.security = security;
            }
            None => block.mcp.clear(),
        }
    }
    let spec = LaunchSpec::from_config(&config, image, repo_root, log_dir)
        .await?
        .with_embedded_mcp_policy(EmbeddedMcpPolicy::Ignore);
    Ok(ContainerSpec { spec, left_out })
}
