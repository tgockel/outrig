//! Everything a session needs from outside, given before it starts, and the
//! one call that starts it.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use super::application::{self, ApplicationSpec, Prepared};
use super::approval::{self, Approver};
use super::bindings::{self, BindingSpec, Bindings, Declared, Places, Setup};
use super::channels::{self, ChannelBackend, ChannelDefinition, Channels};
use super::event::{DEFAULT_CAPACITY, Subscription};
use super::lifecycle::Lifecycle;
use super::mcp::{
    self, HostPlan, Labels as ConfiguredLabels, McpPolicy, McpServiceSpec, McpServices, Plan,
};
use super::mcp_placement::Placement;
use super::root::ConstructSpec;
use super::services::{self, Service, ServicePolicy, Services};
use super::skills::{self, Catalog, SkillSource};
use super::{Failure, Session, SessionError};
use crate::agent::{self, AgentError, ResolvedAgent};
use crate::config::{Config, ConfigValidationError, EnvSecrets, ImageConfig, Secrets, lexical};
use crate::container::ContainerMount;
use crate::error::{IoPathExt, OutrigError};
use crate::events::{Events, StreamBuilder};
use crate::image::ImageTag;
use crate::outrig_::resolve_session_id;
use crate::python::host::{Interpreter, READY_TIMEOUT};
use crate::python::payload::RPYC_MOUNT;
use crate::{EmbeddedMcpPolicy, LaunchSpec, Outrig};
use tokio_util::sync::CancellationToken;

/// A step of a session's start, as a progress callback is told of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Step {
    /// Installing the packages the session's bindings require, each set once
    /// into the user's cache. Reported only when the session has a binding.
    Packages,
    /// Starting the bindings' process on the host and running each factory.
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
    /// The step began.
    Started(Step),
    /// The step completed. A step that fails is not reported finished: the
    /// start returns its error instead.
    Finished(Step),
}

/// What a session needs from outside, given before it starts: the
/// configuration, the agent and the model to run -- or none, for a session
/// with no primary agent -- the container, the secrets its model calls need,
/// its bindings and who approves a repository's, who subscribes to its events,
/// and whether it records them. [`SessionBuilder::start`] starts it, in one
/// call.
///
/// ```no_run
/// # async fn example(
///     config: outrig::config::Config,
/// ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
/// use outrig::harness::{DEFAULT_DRAIN, SessionBuilder, SessionError};
///
/// let mut builder = SessionBuilder::new(config, Some("coding"), None)
///     .secrets(|var: &str| (var == "ANTHROPIC_API_KEY").then(|| "sk-...".to_string()));
/// let mut events = builder.subscribe();
/// let mut session = builder.start().await?;
/// let user = session.user_channel().ok_or(SessionError::NoPrimary)?;
/// user.send("summarize the README").await?;
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
    /// Services given through [`SessionBuilder::serve`], in order.
    served: Vec<(String, Arc<dyn Service>)>,
    /// Channels given through [`SessionBuilder::channel`], in order.
    channels: Vec<(ChannelDefinition, Arc<dyn ChannelBackend>)>,
    question_backend: Option<Arc<dyn ChannelBackend>>,
    /// How the session's services are called.
    service_policy: Option<ServicePolicy>,
    /// MCP services given through [`SessionBuilder::mcp`], in order.
    mcp: Vec<(String, McpServiceSpec)>,
    /// How they start, when the owner said.
    mcp_policy: Option<McpPolicy>,
    /// The owner's identity headers, for the URL services it declares.
    mcp_identity: Vec<(String, String)>,
    /// What places a test's MCP servers, in place of containers.
    #[cfg(test)]
    mcp_placements: Option<Placer>,
    /// What a test's configuration declares of MCP, and its primary image's
    /// label entries, for a host session to plan.
    #[cfg(test)]
    mcp_configured: Option<(crate::outrig_::ConfiguredMcp, ConfiguredLabels)>,
    /// The bindings process's environment, exactly, when the owner gave one.
    bindings_env: Option<Vec<(OsString, OsString)>>,
    approver: Option<Arc<dyn Approver>>,
    repo_root: Option<PathBuf>,
    /// The sources of the session's skills, by name, in precedence order.
    skill_sources: Vec<(String, Arc<dyn SkillSource>)>,
    /// The application code given through [`SessionBuilder::application`].
    application: Option<ApplicationSpec>,
    /// The root given through [`SessionBuilder::construct`].
    construct: Option<ConstructSpec>,
    /// How long each module the application imports at start may take, in
    /// place of [`READY_TIMEOUT`].
    #[cfg(test)]
    import_bound: Option<std::time::Duration>,
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
    /// A host session's id, in place of `test`.
    #[cfg(test)]
    host_id: Option<String>,
    /// The workspace a host session names, in place of none.
    #[cfg(test)]
    host_workspace: Option<PathBuf>,
    /// Why the session's shutdown is to say it is not proven stopped.
    #[cfg(test)]
    fail_stop: Option<String>,
    /// Whether the session has a primary agent: until
    /// [`SessionBuilder::no_primary`], it does.
    primary: bool,
    /// Whether a host program owns the session, and may inject live objects.
    program: bool,
    /// The interpreter's working directory in the container, in place of the
    /// image's or the workspace's.
    workdir: Option<PathBuf>,
}

impl SessionBuilder {
    /// A session of `agent` -- an `[agents.<name>]` block, or `None` for one
    /// that names none -- over `config`. `model` overrides the model the agent
    /// or `default-model` would choose.
    ///
    /// By default its keys are read from the process environment, nothing
    /// subscribes, nothing is recorded, and the container is the one
    /// [`Session::start`] describes. [`SessionBuilder::no_primary`] makes it
    /// a session with no primary agent.
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
            served: Vec::new(),
            channels: Vec::new(),
            question_backend: None,
            service_policy: None,
            mcp: Vec::new(),
            mcp_policy: None,
            mcp_identity: Vec::new(),
            #[cfg(test)]
            mcp_placements: None,
            #[cfg(test)]
            mcp_configured: None,
            bindings_env: None,
            approver: None,
            repo_root: None,
            skill_sources: Vec::new(),
            application: None,
            construct: None,
            #[cfg(test)]
            import_bound: None,
            #[cfg(test)]
            places: None,
            #[cfg(test)]
            host_log: None,
            #[cfg(test)]
            events: None,
            #[cfg(test)]
            host_id: None,
            #[cfg(test)]
            host_workspace: None,
            #[cfg(test)]
            fail_stop: None,
            primary: true,
            program: false,
            workdir: None,
        }
    }

    /// The session of a host program, which may inject live objects into it,
    /// its interpreter working in `cwd` when given.
    pub(crate) fn program(mut self, cwd: Option<PathBuf>) -> Self {
        self.program = true;
        self.workdir = cwd;
        self
    }

    /// Start the session with no primary agent: it resolves no model, and asks
    /// the secret resolver for nothing, as it starts. It has no rounds, no
    /// `user` channel and no interrupter (see [`Session`]); everything else --
    /// its container, interpreter and bindings, its admission, its events and
    /// its report -- is the session's, as it is with one. Its state is `idle`
    /// from `starting` until `closing`.
    ///
    /// The agent [`SessionBuilder::new`] names still chooses the image of the
    /// container [`Session::start`] describes, and [`SessionBuilder::check`]
    /// still resolves the model it names, when it names one. An agent the
    /// configuration does not define fails `check` and the start alike.
    pub fn no_primary(mut self) -> Self {
        self.primary = false;
        self
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
    /// One process on the host holds every binding of the session, started
    /// before the interpreter in the workspace's host directory, with the
    /// environment [`SessionBuilder::bindings_env`] gives it or else the
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

    /// Serve `service` to agent Python as `name`, beside the bindings, with no
    /// approval asked: the owner is trusted as the operator is.
    ///
    /// Every kernel gets an object of a class generated from the service's
    /// description, under `name` where the name is free and as
    /// `outrig.bindings[name]` always; an agent given facilities sees it only
    /// when its mapping names it. Each call of a described method is checked
    /// against its schema and dispatched to [`Service::call`] under the
    /// [`SessionBuilder::service_policy`]. A name a binding could not take --
    /// not a Python identifier, a keyword, `runtime`, `asyncio`, `outrig` -- or
    /// that a binding or another service already takes, fails the start.
    pub fn serve(mut self, name: impl Into<String>, service: Arc<dyn Service>) -> Self {
        self.served.push((name.into(), service));
        self
    }

    /// Declare an external channel: `definition` says what it is, and `backend`
    /// is its far end, the owner's own messaging, which OutRig calls for each
    /// item the holder sends and receives.
    ///
    /// One agent holds the channel, as `runtime.channels[name]`: the primary
    /// agent, when the session has one; or, in a session with
    /// [`SessionBuilder::no_primary`], the agent whose facility dict names it
    /// with [`Facility::channel`](super::Facility::channel), and none until one
    /// does. Its window exists from the start either way, holding the
    /// definition's initial items and what [`Session::channel`] pushes. A name
    /// a binding could not take, `user`, or one a binding, a service or another
    /// channel already takes, fails the start; so does a bound past
    /// [`CHANNEL_BOUND_MAX`](super::CHANNEL_BOUND_MAX), and a
    /// [`Contract::Typed`](super::Contract::Typed) in a session with no
    /// [`SessionBuilder::application`] or whose type does not import.
    pub fn channel(
        mut self,
        definition: ChannelDefinition,
        backend: Arc<dyn ChannelBackend>,
    ) -> Self {
        self.channels.push((definition, backend));
        self
    }

    /// Place, start and serve the MCP server `spec` declares to agent Python
    /// as `name`, as a service: under `name` where the name is free and as
    /// `outrig.bindings[name]` always, its tools its methods by their exact
    /// names, each call under the [`SessionBuilder::service_policy`]. No
    /// approval is asked: the owner is trusted as the operator is.
    ///
    /// It starts with the session, beside its interpreter, concurrently with
    /// the other servers: placed, its `initialize` answered and its tools
    /// listed, a failure that may pass tried again within the
    /// [`McpPolicy::connect_budget`]. A `${VAR}` in its environment is
    /// resolved once by the session's [`Secrets`], and reaches podman by name.
    /// A server that does not start does not fail the session: it starts
    /// without it, and [`Session::mcp`] says why. A name a binding could not
    /// take, or that a binding, a service, a channel or another `mcp` takes,
    /// fails the start.
    ///
    /// The configuration's `[images.<name>.mcp]` servers start the same way,
    /// in a container [`container_spec`] describes, under the name each has
    /// there -- `-` read as `_` where that name is free.
    pub fn mcp(mut self, name: impl Into<String>, spec: McpServiceSpec) -> Self {
        self.mcp.push((name.into(), spec));
        self
    }

    /// Start the session's MCP services under `policy`, in place of
    /// [`McpPolicy::default`] with the configuration's
    /// `mcp-connect-budget-secs`.
    pub fn mcp_policy(mut self, policy: McpPolicy) -> Self {
        self.mcp_policy = Some(policy);
        self
    }

    /// Receive the primary agent's questions as outbound requests on the reserved
    /// `__questions` channel. Without a backend, questions fail clearly rather than waiting.
    pub fn question_backend(mut self, backend: Arc<dyn ChannelBackend>) -> Self {
        self.question_backend = Some(backend);
        self
    }

    /// Carry `headers`, unread and unchanged, on every request to every URL
    /// MCP service the owner declares -- one of them may be a fence token --
    /// and on none a configuration declares. A server in a container receives
    /// identity only through the environment its declaration gives it. A
    /// header HTTP or the MCP transport sets itself -- `Mcp-Name`, any
    /// `Mcp-Param-*` -- would not travel unchanged, and leaves each of the
    /// owner's URL services misconfigured.
    pub fn mcp_identity<K, V>(mut self, headers: impl IntoIterator<Item = (K, V)>) -> Self
    where
        K: Into<String>,
        V: Into<String>,
    {
        self.mcp_identity = headers
            .into_iter()
            .map(|(k, v)| (k.into(), v.into()))
            .collect();
        self
    }

    /// Call the session's services under `policy`, in place of
    /// [`ServicePolicy::default`] with the configuration's
    /// `service-result-max-bytes`.
    pub fn service_policy(mut self, policy: ServicePolicy) -> Self {
        self.service_policy = Some(policy);
        self
    }

    /// Run the process holding the session's bindings with exactly `vars` as
    /// its environment. The only credentials its libraries then hold are
    /// those in `vars`, shared by every binding of the session, since one
    /// process holds them all.
    pub fn bindings_env<K, V>(mut self, vars: impl IntoIterator<Item = (K, V)>) -> Self
    where
        K: Into<OsString>,
        V: Into<OsString>,
    {
        self.bindings_env = Some(
            vars.into_iter()
                .map(|(k, v)| (k.into(), v.into()))
                .collect(),
        );
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

    /// Serve the skills `source` holds, under `name`, which the session's
    /// events and the agent's `outrig.skills` report it by. Sources rank in
    /// the order given: a skill more than one holds is served from the first,
    /// and each later copy is reported as shadowed. A source with no name, or
    /// one another source has, fails the start.
    ///
    /// Each skill's metadata is read as the session starts, with no Python
    /// run, and the primary agent is told the skills by name and description.
    /// A skill's files reach the container only as its code imports or opens
    /// them, and nothing is mounted for them.
    pub fn skill_source(self, name: impl Into<String>, source: impl SkillSource + 'static) -> Self {
        self.shared_skill_source(name.into(), Arc::new(source))
    }

    /// Share a program's source with one of its sessions, retaining the declared precedence.
    pub(crate) fn shared_skill_source(
        mut self,
        name: String,
        source: Arc<dyn SkillSource>,
    ) -> Self {
        self.skill_sources.push((name, source));
        self
    }

    /// Provision `spec`'s application code -- the package holding the task-bound class and what
    /// it imports -- into the session's interpreter, importable in every kernel, before any
    /// opens. No approval is asked: the owner is trusted as the operator is.
    ///
    /// The code is read as the session starts and copied whole to a directory named for its
    /// digest, which the container mounts read-only under `/outrig/app/`: the session runs that
    /// snapshot, whatever happens to the original meanwhile. It goes on `sys.path` after OutRig's
    /// own package and before the workspace, so a file in the workspace cannot take the place of
    /// one of its modules, and its requirement set is installed and placed beside it. The
    /// modules its specification names are then imported, each on a thread of the interpreter's
    /// own, and one that does not import fails the start, naming it and ending with the end of
    /// its traceback; the session's events record each step. [`Session::application`] says
    /// which code the session runs.
    ///
    /// A session holds one application: a later call replaces an earlier one.
    pub fn application(mut self, spec: ApplicationSpec) -> Self {
        self.application = Some(spec);
        self
    }

    /// Construct the root `spec` describes as the session starts, once its application code
    /// is provisioned, and before the start returns: [`Session::constructed`] is its handle.
    /// [`SessionBuilder::check`], and the start before anything is launched, resolve its model
    /// and each `${VAR}` its keys name. A root that is not constructed fails the start, naming
    /// why; one whose orienting round fails says so at [`RootHandle::ready`]. See
    /// [`Session::construct`] for what construction is; a session with a primary agent
    /// constructs none.
    ///
    /// A later call replaces an earlier one; [`Session::construct`] constructs more.
    ///
    /// [`RootHandle::ready`]: super::RootHandle::ready
    pub fn construct(mut self, spec: ConstructSpec) -> Self {
        self.construct = Some(spec);
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
    ///
    /// A session with no primary agent resolves no model as it starts, so
    /// this resolves one only when the builder names it: a misspelled name
    /// fails here all the same, as does an agent the configuration does not
    /// define.
    pub fn check(&self) -> Result<(), SessionError> {
        if self.primary || self.model.is_some() {
            self.resolved()?;
        } else if let Some(name) = &self.agent {
            agent::find_agent(&self.config, name).map_err(|e| SessionError::Resolve(Failure(e)))?;
        }
        let declared = bindings::gather(&self.config, &self.bound).map_err(start_error)?;
        services::check_names(&declared, &self.served).map_err(start_error)?;
        self.check_channels(&declared)?;
        self.check_mcp(&declared)?;
        skills::check_sources(&self.skill_sources)
            .map_err(|e| start_error(OutrigError::Configuration(e)))?;
        application::check(self.application.as_ref()).map_err(start_error)?;
        self.check_root()
    }

    /// The MCP services given through [`SessionBuilder::mcp`], against the
    /// bindings `declared` and the services and channels given.
    fn check_mcp(&self, declared: &[Declared]) -> Result<(), SessionError> {
        mcp::check_names(declared, &self.taken(), &self.mcp).map_err(start_error)?;
        if let Some(spec) = &self.container {
            mcp::check_launch_sidecars(&self.mcp, &spec.sidecars).map_err(start_error)?;
        }
        if let Some(policy) = &self.mcp_policy {
            mcp::check_connect_budget(policy.connect_budget)
                .map_err(|why| start_error(OutrigError::Configuration(why)))?;
        }
        Ok(())
    }

    /// The names the services and channels given take.
    fn taken(&self) -> Vec<&str> {
        self.served
            .iter()
            .map(|(name, _)| name.as_str())
            .chain(
                self.channels
                    .iter()
                    .map(|(definition, _)| definition.name()),
            )
            .collect()
    }

    /// How the session's services are called: the owner's policy, or the
    /// default with the configuration's result bound.
    pub(super) fn service_policy_or_default(&self) -> ServicePolicy {
        self.service_policy
            .clone()
            .unwrap_or_else(|| ServicePolicy::for_config(&self.config))
    }

    /// How the session's MCP services start: the owner's policy, or the
    /// default with the configuration's budget.
    pub(super) fn mcp_policy_or_default(&self) -> McpPolicy {
        self.mcp_policy
            .clone()
            .unwrap_or_else(|| McpPolicy::for_config(&self.config))
    }

    /// What planning and starting the session's MCP services takes, held
    /// apart from the builder -- a borrow of it held across an await would be
    /// the start's future's, and the progress callback is not `Sync` -- where
    /// a configured name one of the `bindings` takes is not bound.
    pub(super) fn mcp_inputs(&self, bindings: Vec<String>, events: &Events) -> McpInputs {
        let mut taken: Vec<String> = self.taken().into_iter().map(str::to_string).collect();
        taken.extend(bindings);
        taken.extend(self.mcp.iter().map(|(name, _)| name.clone()));
        McpInputs {
            owner: self.mcp.clone(),
            taken,
            launched: BTreeSet::new(),
            start: mcp::Start {
                policy: self.mcp_policy_or_default(),
                identity: self.mcp_identity.clone(),
                secrets: Arc::clone(&self.secrets),
                events: events.clone(),
            },
        }
    }

    /// The channels given through [`SessionBuilder::channel`], against the bindings `declared`
    /// and the services given.
    fn check_channels(&self, declared: &[Declared]) -> Result<(), SessionError> {
        channels::check(
            declared,
            &self.served,
            &self.channels,
            self.application.is_some(),
        )
        .map_err(start_error)
    }

    /// The root [`SessionBuilder::construct`] asked for: its model, and the keys it names,
    /// resolved as a root's are; and a session that may construct one.
    fn check_root(&self) -> Result<(), SessionError> {
        let Some(spec) = &self.construct else {
            return Ok(());
        };
        if self.primary {
            return Err(SessionError::Primary);
        }
        agent::resolve(&self.config, None, Some(&spec.model), &*self.secrets)
            .map(|_| ())
            .map_err(|e| SessionError::Resolve(Failure(e)))
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

    /// [`SessionBuilder::resolved`], taken for the start, or `None` for a
    /// session with no primary agent, which resolves nothing but fails on an
    /// agent the configuration does not define.
    fn resolution(&mut self) -> Result<Option<ResolvedAgent>, SessionError> {
        if !self.primary {
            if let Some(name) = &self.agent {
                agent::find_agent(&self.config, name)
                    .map_err(|e| SessionError::Resolve(Failure(e)))?;
            }
            return Ok(None);
        }
        self.resolved()?;
        Ok(Some(
            self.resolved.take().expect("resolved by the line above"),
        ))
    }

    /// What the session's child agents are made from, `catalog` the skills the root its owner
    /// constructs is told of.
    fn child_setup(
        &self,
        workspace: Option<std::path::PathBuf>,
        catalog: &Catalog,
    ) -> super::session::ChildSetup {
        super::session::ChildSetup {
            config: Arc::new(self.config.clone()),
            secrets: Arc::clone(&self.secrets),
            agent: self.agent.clone(),
            workspace,
            skills: catalog.briefs(),
        }
    }

    /// Where the session's requirement sets install and its approvals are remembered: the
    /// user's, unless a test gave its own.
    fn places(&self) -> Result<Places, OutrigError> {
        #[cfg(test)]
        if let Some(places) = &self.places {
            return Ok(places.clone());
        }
        Places::user()
    }

    /// The application code given, read, copied and its requirement set installed, or `None`
    /// when none was given.
    async fn prepare_application(&mut self) -> Result<Option<Prepared>, OutrigError> {
        let Some(spec) = self.application.take() else {
            return Ok(None);
        };
        let places = self.places()?;
        Ok(Some(application::prepare(spec, &places).await?))
    }

    /// How long each module the application imports at start may take.
    fn import_wait(&self) -> std::time::Duration {
        #[cfg(test)]
        if let Some(bound) = self.import_bound {
            return bound;
        }
        READY_TIMEOUT
    }

    /// Serve the services given through [`SessionBuilder::serve`] to
    /// `interpreter`, before any kernel opens.
    fn serve_services(
        &mut self,
        interpreter: &Interpreter,
        lifecycle: &Arc<Lifecycle>,
    ) -> Services {
        let policy = self.service_policy_or_default();
        Services::serve(
            std::mem::take(&mut self.served),
            policy,
            interpreter,
            lifecycle,
        )
    }

    /// The channels given through [`SessionBuilder::channel`], for `interpreter`'s session: told
    /// the interpreter in [`assemble`], once the application code is there.
    fn channels(
        &mut self,
        interpreter: &Interpreter,
        lifecycle: &Arc<Lifecycle>,
        catalog: &Arc<Catalog>,
    ) -> Channels {
        let backend = self.question_backend.take().unwrap_or_else(|| {
            let (backend, receiver) = super::questions::QuestionBackend::new();
            drop(receiver);
            backend
        });
        self.channels
            .push((super::questions::definition(), backend));
        Channels::new(
            std::mem::take(&mut self.channels),
            interpreter,
            lifecycle,
            Some(Arc::clone(catalog)),
        )
    }

    fn report(&mut self, progress: Progress) {
        if let Some(report) = &mut self.progress {
            report(progress);
        }
    }

    /// Start the session: resolve its model, approve, install and start its
    /// bindings, launch its container, start the interpreter in it, and build
    /// the agent -- or, for a session with no primary agent, everything but the
    /// model and the agent. On failure nothing it started is left running.
    pub async fn start(mut self) -> Result<Session, SessionError> {
        let mut resolved = self.resolution()?;
        let declared = bindings::gather(&self.config, &self.bound).map_err(start_error)?;
        services::check_names(&declared, &self.served).map_err(start_error)?;
        self.check_channels(&declared)?;
        self.check_mcp(&declared)?;
        skills::check_sources(&self.skill_sources)
            .map_err(|e| start_error(OutrigError::Configuration(e)))?;
        application::check(self.application.as_ref()).map_err(start_error)?;
        self.check_root()?;
        let repo_root = chosen_repo_root(self.repo_root.as_deref()).map_err(start_error)?;
        let mut spec = match self.container.take() {
            Some(spec) => spec,
            None => default_container(&self.config, self.agent.as_deref(), &repo_root)
                .await
                .map_err(start_error)?,
        };
        let id = resolve_session_id(&spec).map_err(start_error)?;
        spec.session_id = Some(id.clone());
        let (events, lifecycle) = self.open_stream(&id, &spec.log_dir).await?;
        let catalog = skills::discover(
            std::mem::take(&mut self.skill_sources),
            &events,
            skills::SOURCE_TIMEOUT,
        )
        .await;
        // The launch leaves the configuration's servers out; the MCP table
        // starts them, once the primary they may run in is up. A
        // repository's URL entries are put to the approver first, as its
        // bindings are.
        let mut configured = spec.configured_mcp.take();
        self.approve_mcp_urls(configured.as_mut(), &repo_root)
            .await
            .map_err(start_error)?;
        let mcp = McpInputs {
            launched: spec
                .sidecars
                .iter()
                .map(|sidecar| sidecar.name.clone())
                .collect(),
            ..self.mcp_inputs(
                declared
                    .iter()
                    .map(|binding| binding.name.clone())
                    .collect(),
                &events,
            )
        };

        // A program's session works in its `cwd`, at the same path on both sides.
        let cwd = self.workdir.clone().unwrap_or_else(|| {
            spec.workspace
                .as_ref()
                .map_or_else(|| repo_root.clone(), |workspace| lexical(&workspace.host))
        });
        let application = self.prepare_application().await.map_err(start_error)?;
        let mut bindings = self
            .start_bindings(declared, &repo_root, &cwd, &events)
            .await
            .map_err(start_error)?;
        if self.program {
            bindings.allow_live().await;
        }
        if bindings.rpyc_dir().is_some()
            && let Err(e) = same_paths(&self.config, &mut spec)
        {
            bindings.stop_after_failed_start().await;
            return Err(start_error(e));
        }
        spec.packages = package_mounts(&bindings, application.as_ref());

        self.report(Progress::Started(Step::Container));
        let outrig = match Outrig::launch(&spec).await {
            Ok(outrig) => outrig,
            Err(e) => {
                bindings.stop_after_failed_start().await;
                return Err(start_error(e));
            }
        };
        self.report(Progress::Finished(Step::Container));

        // The primary's facts, which it read as it launched.
        let host = match outrig.sidecar_host().await {
            Ok(host) => host,
            Err(e) => {
                stop(Some(outrig), &mut bindings).await;
                return Err(start_error(e));
            }
        };
        let target = outrig.primary().exec_target();
        let log_dir = spec.log_dir.clone();
        let place = |planned: &HostPlan| -> Arc<dyn Placement> {
            match planned {
                HostPlan::Primary => Arc::new(super::mcp_placement::Primary::new(
                    target.clone(),
                    log_dir.clone(),
                )),
                HostPlan::Sidecar(spec) => Arc::new(super::mcp_placement::Sidecar::new(
                    host.clone(),
                    crate::SidecarSpec::clone(spec),
                )),
            }
        };
        let workspace = spec
            .workspace
            .as_ref()
            .map(|workspace| lexical(&workspace.host));

        self.report(Progress::Started(Step::Python));
        let rpyc = bindings.rpyc_dir().map(|_| RPYC_MOUNT);
        let workdir = self.workdir.as_deref();
        // Both need only the container, so the servers start -- the primary
        // image's label read first -- while the interpreter does, and both are
        // in before anything is told of them. An interpreter that does not
        // start gives up on the servers still starting.
        let cancel = CancellationToken::new();
        let labels = async {
            match &configured {
                Some(_) => primary_labels(&outrig).await,
                None => ConfiguredLabels::default(),
            }
        };
        let (started, mcp) = tokio::join!(
            async {
                let started =
                    Interpreter::start(outrig.primary(), events.clone(), rpyc, workdir).await;
                if started.is_err() {
                    cancel.cancel();
                }
                started
            },
            mcp.start(
                configured.as_ref(),
                labels,
                workspace,
                place,
                cancel.clone()
            ),
        );
        let interpreter = match started {
            Ok(interpreter) => interpreter,
            Err(e) => {
                mcp.close().await;
                stop(Some(outrig), &mut bindings).await;
                return Err(SessionError::Start(Failure(e.into())));
            }
        };
        self.served.extend(mcp.ready());
        bindings.connect(&interpreter, &events);
        bindings.declare(&interpreter, bindings::container_path);
        skills::serve(&catalog, &interpreter, &events);
        let services = self.serve_services(&interpreter, &lifecycle);
        mcp.bind(&services, &interpreter);
        let channels = self.channels(&interpreter, &lifecycle, &catalog);
        self.report(Progress::Finished(Step::Python));

        let primary = outrig.primary();
        // A container launched without a workspace holds an empty path.
        let workspace = self.workdir.clone().or_else(|| {
            Some(primary.container_workspace())
                .filter(|workspace| !workspace.as_os_str().is_empty())
                .map(Path::to_path_buf)
        });
        let name = primary.name().to_string();
        let setup = self.child_setup(workspace.clone(), &catalog);
        let session = Session::new(
            id,
            interpreter,
            Some(outrig),
            name,
            lifecycle,
            bindings,
            services,
            mcp,
            channels,
            setup,
        );
        #[cfg(test)]
        let session = session.failing(self.fail_stop.take());
        let wait = self.import_wait();
        assemble(
            session,
            application,
            wait,
            resolved.as_mut(),
            self.construct.take(),
            workspace.as_deref(),
            &catalog,
        )
        .await
    }

    /// Ask about each `url` MCP entry of `configured` a repository declared,
    /// as a repository's binding is asked about: one approved before in
    /// `repo_root`, or now, may start; another stays unapproved, saying why.
    async fn approve_mcp_urls(
        &mut self,
        configured: Option<&mut crate::outrig_::ConfiguredMcp>,
        repo_root: &Path,
    ) -> Result<(), OutrigError> {
        let Some(configured) = configured else {
            return Ok(());
        };
        if !configured
            .urls
            .values()
            .any(|entry| entry.unapproved.is_some())
        {
            return Ok(());
        }
        let store = self.places()?.store;
        for (name, entry) in &mut configured.urls {
            if entry.unapproved.is_none() {
                continue;
            }
            let asked = approval::mcp_url_request(name, entry, repo_root);
            entry.unapproved = approval::ask(&asked, store.as_ref(), self.approver.as_ref())
                .await
                .err()
                .map(str::to_string);
        }
        Ok(())
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
        let places = self.places()?;
        let setup = Setup {
            config: &self.config,
            repo_root,
            cwd,
            env: self.bindings_env.as_deref(),
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

    /// Name a host session `id`, in place of `test`.
    #[cfg(test)]
    pub(crate) fn host_id(mut self, id: impl Into<String>) -> Self {
        self.host_id = Some(id.into());
        self
    }

    /// Give a host session `dir` as its workspace: the path its agent names as
    /// its working directory, which on the host is where the files are.
    #[cfg(test)]
    pub(crate) fn host_workspace(mut self, dir: &Path) -> Self {
        self.host_workspace = Some(dir.to_path_buf());
        self
    }

    /// Make the session's shutdown report it not proven stopped, for
    /// `reason`, as a container stop that failed would: the report, its
    /// verdict and its `session.report` event all say so.
    #[cfg(test)]
    pub(crate) fn fail_stop(mut self, reason: impl Into<String>) -> Self {
        self.fail_stop = Some(reason.into());
        self
    }

    /// Give each module the application imports at start `bound` to import in.
    #[cfg(test)]
    pub(crate) fn import_bound(mut self, bound: std::time::Duration) -> Self {
        self.import_bound = Some(bound);
        self
    }

    /// Place a host session's MCP servers with `place`, in place of
    /// containers.
    #[cfg(test)]
    pub(crate) fn mcp_placements(
        mut self,
        place: impl Fn(&HostPlan) -> Arc<dyn Placement> + Send + Sync + 'static,
    ) -> Self {
        self.mcp_placements = Some(Arc::new(place));
        self
    }

    /// Plan what `configured` declares of MCP, with `labels` as the primary
    /// image's label entries, in a host session.
    #[cfg(test)]
    pub(crate) fn mcp_configured(
        mut self,
        configured: crate::outrig_::ConfiguredMcp,
        labels: ConfiguredLabels,
    ) -> Self {
        self.mcp_configured = Some((configured, labels));
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
    /// container and no workspace unless [`SessionBuilder::host_workspace`]
    /// gives one: bindings run in the repository root, and the interpreter
    /// imports their packages from where they are on the host. The session's
    /// id is `test`.
    #[cfg(test)]
    pub(crate) async fn start_on_host(mut self) -> Result<Session, SessionError> {
        skills::check_sources(&self.skill_sources)
            .map_err(|e| start_error(OutrigError::Configuration(e)))?;
        let mut resolved = self.resolution()?;
        let declared = bindings::gather(&self.config, &self.bound).map_err(start_error)?;
        services::check_names(&declared, &self.served).map_err(start_error)?;
        self.check_channels(&declared)?;
        self.check_mcp(&declared)?;
        application::check(self.application.as_ref()).map_err(start_error)?;
        self.check_root()?;
        let repo_root = chosen_repo_root(self.repo_root.as_deref()).map_err(start_error)?;
        let id = self.host_id.take().unwrap_or_else(|| "test".to_string());
        let log_dir = self.host_log.clone().unwrap_or_default();
        let (events, lifecycle) = self.open_stream(&id, &log_dir).await?;
        let catalog = skills::discover(
            std::mem::take(&mut self.skill_sources),
            &events,
            skills::SOURCE_TIMEOUT,
        )
        .await;
        let cwd = self.workdir.clone().unwrap_or_else(|| repo_root.clone());
        let (mut configured, labels) = match self.mcp_configured.take() {
            Some((configured, labels)) => (Some(configured), labels),
            None => (None, ConfiguredLabels::default()),
        };
        self.approve_mcp_urls(configured.as_mut(), &repo_root)
            .await
            .map_err(start_error)?;
        let mcp = self.mcp_inputs(
            declared
                .iter()
                .map(|binding| binding.name.clone())
                .collect(),
            &events,
        );
        let application = self.prepare_application().await.map_err(start_error)?;
        let mut bindings = self
            .start_bindings(declared, &repo_root, &cwd, &events)
            .await
            .map_err(start_error)?;
        if self.program {
            bindings.allow_live().await;
        }
        self.report(Progress::Started(Step::Python));
        let placements = self.mcp_placements.clone();
        let place = |planned: &HostPlan| -> Arc<dyn Placement> {
            match &placements {
                Some(place) => place(planned),
                None => Arc::new(super::mcp_placement::fake::Fake::default()),
            }
        };
        let workspace = self.host_workspace.clone();
        let (interpreter, mcp) = tokio::join!(
            crate::python::testing::start_on_host_in(
                events.clone(),
                bindings.rpyc_dir(),
                self.workdir.as_deref(),
            ),
            mcp.start(
                configured.as_ref(),
                async { labels },
                workspace,
                place,
                CancellationToken::new(),
            ),
        );
        self.served.extend(mcp.ready());
        bindings.connect(&interpreter, &events);
        bindings.declare(&interpreter, Path::to_path_buf);
        skills::serve(&catalog, &interpreter, &events);
        let services = self.serve_services(&interpreter, &lifecycle);
        mcp.bind(&services, &interpreter);
        let channels = self.channels(&interpreter, &lifecycle, &catalog);
        self.report(Progress::Finished(Step::Python));
        let workspace = self.host_workspace.take().or(self.workdir.clone());
        let setup = self.child_setup(workspace.clone(), &catalog);
        let session = Session::new(
            id,
            interpreter,
            None,
            "(host)".into(),
            lifecycle,
            bindings,
            services,
            mcp,
            channels,
            setup,
        )
        .failing(self.fail_stop.take());
        let wait = self.import_wait();
        assemble(
            session,
            application,
            wait,
            resolved.as_mut(),
            self.construct.take(),
            workspace.as_deref(),
            &catalog,
        )
        .await
    }
}

/// What planning and starting a session's MCP services takes: see
/// [`SessionBuilder::mcp_inputs`].
pub(super) struct McpInputs {
    owner: Vec<(String, McpServiceSpec)>,
    /// The names a configured server cannot be bound as.
    taken: Vec<String>,
    /// The sidecars the launch starts, by name.
    launched: BTreeSet<String>,
    start: mcp::Start,
}

impl McpInputs {
    /// Plan the owner's services and what `configured` declares, with the
    /// primary image's `labels` once they are read, and start them, each in
    /// the placement `place` makes, until `cancel` gives up on them.
    pub(super) async fn start(
        self,
        configured: Option<&crate::outrig_::ConfiguredMcp>,
        labels: impl Future<Output = ConfiguredLabels>,
        workspace: Option<PathBuf>,
        place: impl Fn(&HostPlan) -> Arc<dyn Placement>,
        cancel: CancellationToken,
    ) -> McpServices {
        // Given up with the servers, and within their connect budget: an
        // engine that never answers the read would otherwise hold the
        // session's start -- and the unwind of one whose interpreter failed.
        let budget = self.start.policy.connect_budget;
        let by = tokio::time::Instant::now() + budget;
        let labels = tokio::select! {
            read = tokio::time::timeout_at(by, labels) => read.unwrap_or_else(|_| {
                tracing::warn!(
                    target: "outrig::harness",
                    "the primary image's org.outrig.mcp label was not read within the {budget:?} \
                     connect budget, so none of its servers start"
                );
                ConfiguredLabels::default()
            }),
            () = cancel.cancelled() => ConfiguredLabels::default(),
        };
        let plan = Plan::new(
            &self.owner,
            configured,
            labels,
            |name| self.taken.iter().any(|taken| taken == name),
            workspace,
            self.launched,
            &self.start.identity,
        );
        McpServices::start(self.start, plan, place, by, cancel).await
    }
}

/// What places a test's MCP servers.
#[cfg(test)]
type Placer = Arc<dyn Fn(&HostPlan) -> Arc<dyn Placement> + Send + Sync>;

/// What the primary `outrig` runs declares in its image's `org.outrig.mcp`
/// label. A label that cannot be read is said in the log and adds nothing:
/// the configuration's own servers start all the same.
async fn primary_labels(outrig: &Outrig) -> ConfiguredLabels {
    let image = outrig.primary().image_tag().as_str().to_string();
    match crate::container::embedded::read_embedded_mcp_of(outrig.primary()).await {
        Ok(servers) => ConfiguredLabels { image, servers },
        Err(e) => {
            tracing::warn!(
                target: "outrig::harness",
                "the primary image's org.outrig.mcp label was not read, so none of its servers \
                 start: {e}"
            );
            ConfiguredLabels::default()
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

/// `session`, with its application code provisioned -- each module imported
/// at start within `wait` -- its channels declared within the same, the
/// primary agent `resolved` names -- none, for a session with no primary agent
/// -- or the root `construct` describes, in a container whose workspace is
/// `workspace`, holding the session's skills, `catalog`, and idle. Application
/// code that does not import, a channel that cannot be declared, a primary
/// that cannot be built, or a root not constructed, stops what the start
/// launched.
async fn assemble(
    mut session: Session,
    application: Option<Prepared>,
    wait: std::time::Duration,
    resolved: Option<&mut ResolvedAgent>,
    construct: Option<ConstructSpec>,
    workspace: Option<&Path>,
    catalog: &Arc<Catalog>,
) -> Result<Session, SessionError> {
    if let Some(application) = application
        && let Err(e) = session.provision(application, wait).await
    {
        session.abandon().await;
        return Err(SessionError::Start(Failure(e.into())));
    }
    // Declared once the application code is there, which a typed contract names, and before
    // any agent is built, so each window holds its initial items when its holder first looks.
    if let Err(e) = session.declare_channels(wait).await {
        session.abandon().await;
        return Err(start_error(OutrigError::Configuration(e)));
    }
    if let Some(resolved) = resolved
        && let Err(e) = session.build_primary(resolved, workspace, catalog).await
    {
        session.abandon().await;
        return Err(SessionError::Start(Failure(e)));
    }
    if let Some(spec) = construct
        && let Err(e) = session.construct_at_start(spec).await
    {
        session.abandon().await;
        return Err(e);
    }
    session.idle();
    Ok(session)
}

/// Stop what a failed start launched -- its bindings, then its container when
/// it has one -- saying so if that fails too.
pub(super) async fn stop(container: Option<Outrig>, bindings: &mut Bindings) {
    bindings.stop_after_failed_start().await;
    let Some(outrig) = container else {
        return;
    };
    for failed in outrig.stop().await {
        tracing::warn!(target: "outrig::harness", "stopping a session that failed to start: {failed}");
    }
}

/// A start failure from OutRig's own error.
fn start_error(e: OutrigError) -> SessionError {
    SessionError::Start(Failure(AgentError::from(e)))
}

/// `spec`, rewritten for a session with bindings: every directory it mounts
/// appears in the container at a path that names it on the host, so a path the
/// agent writes and a path the library reads name one file. That is its host
/// path, or a path that resolves to it there: a repository's mount is bound
/// from where its declared path led when it was checked, and appears at the
/// declared path (#604).
///
/// The workspace's container path becomes its host path, unless `config`
/// declared one, which must name the same directory or is an error naming the
/// key. A `LaunchSpec` cannot say whether its container path was declared --
/// one from the configuration holds `/workspace` when none was -- so the
/// configuration decides. A mount whose container path does not name its host
/// directory is an error.
fn same_paths(config: &Config, spec: &mut LaunchSpec) -> Result<(), OutrigError> {
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
        workspace.container = match config.workspace.declared_container_path() {
            None => host,
            Some(declared) if names_on_host(declared, &host) => lexical(declared),
            Some(declared) => {
                let declared_in = config.workspace.declared_in();
                Err(differs(
                    "workspace.container-path".into(),
                    declared,
                    &host,
                    declared_in,
                ))?
            }
        };
    }
    for mount in &spec.mounts {
        if !names_on_host(&mount.container, &mount.host) {
            let key = format!(
                "the container path of the mount of {}",
                mount.host.display()
            );
            Err(differs(key, &mount.container, &mount.host, None))?;
        }
    }
    Ok(())
}

/// Whether `container`, read on the host, names the directory at `host`: the
/// same path, or one that resolves to the same directory.
fn names_on_host(container: &Path, host: &Path) -> bool {
    let container = lexical(container);
    container == lexical(host)
        || matches!(
            (container.canonicalize(), host.canonicalize()),
            (Ok(c), Ok(h)) if c == h
        )
}

/// What the container binds read-only beside the payload, under OutRig's own
/// root: each requirement set's directory, the bindings' and the
/// application's, once each; the application's snapshot; and the vendored RPyC
/// the interpreter reaches the bindings with.
fn package_mounts(bindings: &Bindings, application: Option<&Prepared>) -> Vec<ContainerMount> {
    let mut dirs = bindings.package_dirs();
    if let Some(dir) = application.and_then(Prepared::packages)
        && !dirs.contains(&dir)
    {
        dirs.push(dir);
    }
    dirs.into_iter()
        .map(|dir| ContainerMount::shared_read_only(dir, bindings::container_path(dir)))
        .chain(application.map(Prepared::snapshot))
        .chain(
            bindings
                .rpyc_dir()
                .map(|dir| ContainerMount::shared_read_only(dir, Path::new(RPYC_MOUNT))),
        )
        .collect()
}

/// A container for a session, and the MCP servers it leaves out.
#[non_exhaustive]
pub struct ContainerSpec {
    /// The container's description, to give [`SessionBuilder::container`].
    pub spec: LaunchSpec,
    /// The configured MCP servers the container itself does not start: a
    /// session given `spec` starts them, under its MCP table's rules (see
    /// [`SessionBuilder::mcp`]).
    pub left_out: Vec<String>,
}

/// The repository root an owner gave, made absolute against the current
/// directory, or the current directory: what a session's, and a host
/// program's, configuration resolves against.
pub(crate) fn chosen_repo_root(root: Option<&Path>) -> Result<PathBuf, OutrigError> {
    match root {
        // Before anything keys on it: a relative root names another
        // repository from another directory, under the same approval.
        Some(root) => std::path::absolute(root).path_ctx("resolve", root),
        None => std::env::current_dir().path_ctx("read", "."),
    }
}

/// The container a session runs in, for the `[images.<image>]` block of
/// `config`: its workspace, mounts, security and network as `config` declares
/// them, with repository-relative paths against `repo_root` and its logs in
/// `log_dir`. With `pinned`, the image is that already-ensured tag, keeping the
/// block's security, which [`LaunchSpec::from_config`] finds local; otherwise
/// it is whatever the block names, which `from_config` pulls or builds under
/// the block's name.
///
/// **The launch starts no MCP server and no sidecar**, including servers an
/// image declares in its own `org.outrig.mcp` label: the spec carries what the
/// block declares, each sidecar's image built, pulled or found, and a session
/// given it starts those servers through its MCP table, as services (see
/// [`SessionBuilder::mcp`]). There a server that holds a credential -- a
/// `${VAR}` in its `env` -- is never placed in the primary or a sidecar that
/// shares its view: beside the interpreter as the same user, its resolved
/// secrets would be readable from Python wherever `/proc` allows, so such a
/// server does not start, and the session says why. The servers the launch
/// leaves out are named.
pub async fn container_spec(
    config: &Config,
    image: &str,
    pinned: Option<&ImageTag>,
    repo_root: &Path,
    log_dir: PathBuf,
) -> Result<ContainerSpec, OutrigError> {
    // For the session's MCP table, read before the block is cleared or
    // replaced below.
    let configured = crate::outrig_::ConfiguredMcp::from_config(config, image, repo_root).await;
    // Sidecars go before lowering rather than after: lowering would start
    // them with the launch, and the session's MCP table starts them itself.
    let mut config = config.clone();
    config.sidecars.clear();
    let mut left_out = Vec::new();
    if let Some(block) = config.images.get_mut(image) {
        left_out = block.mcp().keys().cloned().collect();
        match pinned {
            Some(tag) => {
                let security = std::mem::take(block.security_mut());
                *block = ImageConfig::from_image_name(tag.as_str());
                *block.security_mut() = security;
            }
            None => block.mcp_mut().clear(),
        }
    }
    let mut spec = LaunchSpec::from_config(&config, image, repo_root, log_dir)
        .await?
        .with_embedded_mcp_policy(EmbeddedMcpPolicy::Ignore);
    spec.configured_mcp = Some(configured);
    Ok(ContainerSpec { spec, left_out })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::config::{MountAccess, Workspace};
    use crate::{MountSpec, WorkspaceSpec};

    /// A container path that resolves on the host to the directory mounted
    /// there names it, as the host path does: a repository's mount is bound
    /// from where its declared path led (#604) and appears at the declared
    /// path. A container path that names another directory is refused, and so
    /// is one with a `..` after a symlink that the host resolves to the
    /// mounted directory: the container resolves `..` textually.
    #[test]
    fn a_container_path_resolving_to_the_mounted_directory_names_it() {
        let tmp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        let [real, other, link] = ["real", "other", "link"].map(|name| root.join(name));
        std::fs::create_dir(&real).unwrap();
        std::fs::create_dir(&other).unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let spec = |mounted: &Path| {
            LaunchSpec::from_image("img:latest", BTreeMap::new(), root.join("logs"))
                .with_workspace(WorkspaceSpec::new(&real, "/workspace"))
                .with_mount(MountSpec::new(&real, mounted, MountAccess::ReadOnly))
        };
        let declaring = |container: &Path| {
            let mut config = Config::default();
            config.workspace = Workspace::new(&real, container);
            config
        };

        let mut linked = spec(&link);
        same_paths(&declaring(&link), &mut linked).unwrap();
        assert_eq!(linked.workspace.expect("a workspace").container, link);
        let mut undeclared = spec(&link);
        same_paths(&Config::default(), &mut undeclared).unwrap();
        assert_eq!(undeclared.workspace.expect("a workspace").container, real);

        let key = |result: Result<(), OutrigError>| match result {
            Err(OutrigError::ConfigValidation(ConfigValidationError::BindingContainerPath {
                key,
                ..
            })) => key,
            other => panic!("{other:?}"),
        };
        let refused = same_paths(&declaring(&other), &mut spec(&link));
        assert_eq!(key(refused), "workspace.container-path");
        let mount = format!("the container path of the mount of {}", real.display());
        assert_eq!(
            key(same_paths(&Config::default(), &mut spec(&other))),
            mount
        );
        std::fs::create_dir(root.join("sub")).unwrap();
        std::fs::create_dir(root.join("inner")).unwrap();
        std::os::unix::fs::symlink(root.join("inner"), root.join("sub/up")).unwrap();
        let around = root.join("sub/up/../real");
        assert_eq!(
            key(same_paths(&Config::default(), &mut spec(&around))),
            mount
        );
    }
}
