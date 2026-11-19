//! The MCP services a session places: each MCP server an owner or a
//! configuration declares, placed, started, listed and bound to the session as
//! a [`Service`] OutRig ships, with the lifecycle a Rust service does not have
//! -- startup with its classification and budget, the pause-or-stop control a
//! cut uses, and the close.
//!
//! A call is the service table's (`services.rs`): admitted, checked,
//! dispatched, bounded, cancelled and reported as every service's call is.
//! What is MCP's is kept here, in a table of its own: each declaration, the
//! client its server answers on, the container it runs in, and its state.
//! See `plan/phase/0003-python/mcp-presenter.md`.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use futures_util::future::join_all;
use serde::Serialize;
use serde_json::{Map, Value};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use super::event::{Payload, ServiceCallId, ServiceKind, ServiceReason};
use super::mcp_placement::{Launch, Placement, SHARES_THE_PRIMARY, Server};
use super::services::{
    CallContext, Hold, MethodDescription, Service, ServiceDescription, ServiceError,
    ServiceErrorCategory, ServiceFuture, Services,
};
use crate::config::{
    ApiKeyError, EnvValue, McpServerSpec, MountAccess, ResolvedEnvValue, Secrets, SidecarView,
    SidecarWorkspaceAccess,
};
use crate::container::embedded::McpDeclarationSource;
use crate::error::{McpFailureKind, OutrigError};
use crate::events::Events;
use crate::mcp::{McpClient, StartupClass};
use crate::mcp_content::{McpTool, result_to_rmcp, tool_to_rmcp};
use crate::outrig_::{ConfiguredMcp, SidecarServerSpec, SidecarSpec};

/// An MCP server a session places and binds as a service: where it runs, and
/// what the session tells agents of it. Given to
/// [`SessionBuilder::mcp`](super::SessionBuilder::mcp) under the name agents
/// reach it by.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct McpServiceSpec {
    /// Where the server runs, and what it is given.
    pub placement: McpPlacement,
    /// The longest one of its calls may run, in seconds, in place of the
    /// session's [`ServicePolicy::call_deadline`](super::ServicePolicy::call_deadline).
    pub call_timeout_secs: Option<u64>,
    /// What agents are told it is, in a line, in place of the first line of
    /// the server's own `instructions`.
    pub description: Option<String>,
}

impl McpServiceSpec {
    /// A server exec'd as `command` in the primary container, beside the
    /// interpreter, as the session's user.
    ///
    /// It may hold no credential: a `${VAR}` in its environment would be
    /// readable from agent Python through `/proc`, so such a declaration is
    /// [`McpServiceOutcome::Misconfigured`] and does not start. A server that
    /// needs one runs in a [`McpServiceSpec::sidecar`].
    pub fn command(command: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self::placed(McpPlacement::Command {
            command: command.into_iter().map(Into::into).collect(),
            env: BTreeMap::new(),
        })
    }

    /// A server in a sidecar of its own, which `spec` describes: its image,
    /// what it sees of the workspace, its mounts and security, and the one
    /// server it hosts, exec'd or its entrypoint. The container is named for
    /// the service, whatever `spec` names it. A `spec` hosting other than one
    /// server is misconfigured; one whose `view` is
    /// [`SidecarView::Primary`] is a primary view, under its rule.
    pub fn sidecar(spec: SidecarSpec) -> Self {
        Self::placed(McpPlacement::Sidecar(spec))
    }

    /// A server that is `image`'s entrypoint, run in a sidecar that sees the
    /// primary's whole filesystem: see
    /// [`Outrig::add_sidecar`](crate::Outrig::add_sidecar) for what joining the
    /// primary's mount namespace grants. As for a command, it may hold no
    /// credential.
    pub fn primary_view(image: impl Into<String>) -> Self {
        Self::placed(McpPlacement::PrimaryView {
            image: image.into(),
            args: Vec::new(),
            env: BTreeMap::new(),
        })
    }

    fn placed(placement: McpPlacement) -> Self {
        Self {
            placement,
            call_timeout_secs: None,
            description: None,
        }
    }

    /// The spec, with `key` set to `value` in the server's environment: a
    /// literal, or a `${VAR}` reference the session's
    /// [`Secrets`](super::Secrets) resolve once, at start. A sidecar's server
    /// gets it beside what its [`SidecarServerSpec`] already sets.
    #[must_use]
    pub fn with_env(mut self, key: impl Into<String>, value: EnvValue) -> Self {
        let key = key.into();
        match &mut self.placement {
            McpPlacement::Command { env, .. } | McpPlacement::PrimaryView { env, .. } => {
                env.insert(key, value);
            }
            McpPlacement::Sidecar(spec) => {
                for server in spec.servers.values_mut() {
                    let mut env = server.env().clone();
                    env.insert(key.clone(), value.clone());
                    *server = server.clone().with_env(env);
                }
            }
        }
        self
    }

    /// The spec, with `args` handed to a primary view's entrypoint. Nothing
    /// for another placement, whose command says its own arguments.
    #[must_use]
    pub fn with_args(mut self, args: impl IntoIterator<Item = impl Into<String>>) -> Self {
        if let McpPlacement::PrimaryView { args: slot, .. } = &mut self.placement {
            *slot = args.into_iter().map(Into::into).collect();
        }
        self
    }

    /// The spec, with `secs` as its calls' own deadline.
    #[must_use]
    pub fn with_call_timeout_secs(mut self, secs: u64) -> Self {
        self.call_timeout_secs = Some(secs);
        self
    }

    /// The spec, with `description` as what agents are told it is.
    #[must_use]
    pub fn with_description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }
}

/// Where an MCP service's server runs.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum McpPlacement {
    /// Exec'd in the primary container: see [`McpServiceSpec::command`].
    #[non_exhaustive]
    Command {
        /// The argv.
        command: Vec<String>,
        /// Its environment.
        env: BTreeMap<String, EnvValue>,
    },
    /// In a sidecar of its own: see [`McpServiceSpec::sidecar`].
    Sidecar(SidecarSpec),
    /// An entrypoint in a sidecar with the primary's filesystem: see
    /// [`McpServiceSpec::primary_view`].
    #[non_exhaustive]
    PrimaryView {
        /// The image whose entrypoint is the server.
        image: String,
        /// The entrypoint's arguments.
        args: Vec<String>,
        /// Its environment.
        env: BTreeMap<String, EnvValue>,
    },
}

/// How a session starts its MCP services. The deadline, result bound and
/// recording of their calls are the
/// [`ServicePolicy`](super::ServicePolicy)'s, shared with every service.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct McpPolicy {
    /// How long each server has to start -- the read of the primary image's
    /// label before it, every attempt, each bounded at two minutes, and every
    /// wait between them -- before it is [`McpServiceOutcome::Unavailable`]:
    /// two minutes, or the configuration's `mcp-connect-budget-secs`.
    pub connect_budget: Duration,
    /// How many times one service is connected again after its transport
    /// fails, in a session: three.
    pub reconnect_max: u32,
    /// The longest one attempt may take: `STARTUP_TIMEOUT`, which a test
    /// lowers.
    pub(crate) attempt: Duration,
}

impl Default for McpPolicy {
    fn default() -> Self {
        Self {
            connect_budget: Duration::from_secs(crate::config::DEFAULT_MCP_CONNECT_BUDGET_SECS),
            reconnect_max: 3,
            attempt: crate::mcp::STARTUP_TIMEOUT,
        }
    }
}

impl McpPolicy {
    /// The default policy, with `config`'s `mcp-connect-budget-secs` as the
    /// budget when it sets one.
    pub(crate) fn for_config(config: &crate::config::Config) -> Self {
        let policy = Self::default();
        match config.mcp_connect_budget_secs {
            Some(secs) => policy.with_connect_budget(Duration::from_secs(secs)),
            None => policy,
        }
    }

    /// The policy, with `budget` as each server's to start in.
    #[must_use]
    pub fn with_connect_budget(mut self, budget: Duration) -> Self {
        self.connect_budget = budget;
        self
    }

    /// The policy, with `max` reconnects per service.
    #[must_use]
    pub fn with_reconnect_max(mut self, max: u32) -> Self {
        self.reconnect_max = max;
        self
    }
}

/// Where an MCP service's server runs, as events and reports name it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum McpPlacementKind {
    /// Exec'd in the primary container.
    Command,
    /// In a sidecar with a filesystem of its own.
    Sidecar,
    /// In a sidecar that sees the primary's filesystem.
    PrimaryView,
}

/// Who declared an MCP service.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum McpDeclaredIn {
    /// The session's owner, through the builder or `outrig.Session`.
    Owner,
    /// The configuration's `[images.<name>.mcp]`.
    Config,
    /// An image's `org.outrig.mcp` label.
    ImageLabel,
}

/// Which way an MCP service's start ended, as events and reports name it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum McpOutcomeKind {
    /// See [`McpServiceOutcome::Ready`].
    Ready,
    /// See [`McpServiceOutcome::Unavailable`].
    Unavailable,
    /// See [`McpServiceOutcome::Refused`].
    Refused,
    /// See [`McpServiceOutcome::Misconfigured`].
    Misconfigured,
    /// See [`McpServiceOutcome::Unclassified`].
    Unclassified,
}

impl McpOutcomeKind {
    /// Its name, as events and reports spell it.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::Unavailable => "unavailable",
            Self::Refused => "refused",
            Self::Misconfigured => "misconfigured",
            Self::Unclassified => "unclassified",
        }
    }
}

/// Why an MCP service did not start, built from what OutRig knows of it and
/// holding no value a `${VAR}` resolved to: its environment is shown as
/// declared. The server's own stderr and words are its own, and OutRig
/// redacts neither.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct McpStartupCause {
    /// The service, by the name agents would have reached it by.
    pub service: String,
    /// The server's own name in the configuration, where it differs.
    pub server: Option<String>,
    /// Where it would have run.
    pub placement: McpPlacementKind,
    /// Who declared it.
    pub declared_in: McpDeclaredIn,
    /// What was run, its environment first as declared: `TOKEN=${GH_TOKEN}
    /// gh-mcp`. `None` for a server that is its image's entrypoint.
    pub command: Option<String>,
    /// How its transport ended, when one ran: `code 127`, `signal 9`, or
    /// `still running (wait timed out)`.
    pub exit: Option<String>,
    /// The end of what it wrote to stderr, when one ran.
    pub stderr_tail: Option<String>,
    /// What went wrong.
    pub message: String,
    /// The class of failure, when the server's handshake or listing failed.
    pub kind: Option<McpFailureKind>,
    /// How many times it was tried.
    pub attempts: u32,
}

/// How a declared MCP service's start ended. The session starts with those
/// that are [`Ready`](Self::Ready); the owner decides what the others mean.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
#[non_exhaustive]
pub enum McpServiceOutcome {
    /// Started and bound: agents reach it as `outrig.bindings[name]`.
    #[non_exhaustive]
    Ready {
        /// How many tools its first listing holds.
        tools: usize,
        /// That listing's generation: 1.
        listing: u64,
        /// How many times it was tried.
        attempts: u32,
    },
    /// Failed in a way that may pass -- its transport, a handshake that did
    /// not answer -- every attempt the connect budget allowed.
    Unavailable(McpStartupCause),
    /// The server answered `initialize` with an error.
    Refused(McpStartupCause),
    /// Cannot start as declared: a binary that is not there, a credential
    /// where the agent could read it, a `${VAR}` with no value, a configured
    /// sidecar's image that could not be had.
    Misconfigured(McpStartupCause),
    /// Failed in a way OutRig does not classify: named so, not guessed at.
    Unclassified(McpStartupCause),
}

impl McpServiceOutcome {
    /// Which way it ended.
    pub fn kind(&self) -> McpOutcomeKind {
        match self {
            Self::Ready { .. } => McpOutcomeKind::Ready,
            Self::Unavailable(_) => McpOutcomeKind::Unavailable,
            Self::Refused(_) => McpOutcomeKind::Refused,
            Self::Misconfigured(_) => McpOutcomeKind::Misconfigured,
            Self::Unclassified(_) => McpOutcomeKind::Unclassified,
        }
    }

    /// Why it did not start, unless it did.
    pub fn cause(&self) -> Option<&McpStartupCause> {
        match self {
            Self::Ready { .. } => None,
            Self::Unavailable(cause)
            | Self::Refused(cause)
            | Self::Misconfigured(cause)
            | Self::Unclassified(cause) => Some(cause),
        }
    }
}

/// What a transient startup failure was, as a retry's event says it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct McpRetryCause {
    /// The class of failure, when the handshake or listing failed.
    pub kind: Option<McpFailureKind>,
    /// What went wrong.
    pub message: String,
}

/// Which MCP services can change files, and which: what a cut quiesces.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct McpWriter {
    /// The service.
    pub service: String,
    /// Where it runs.
    pub placement: McpPlacementKind,
    /// What it can write.
    pub writes: McpWrites,
}

impl McpWriter {
    /// Whether it can write anything the session mounts.
    pub fn is_writer(&self) -> bool {
        self.writes != McpWrites::Nothing
    }
}

/// What an MCP service can write, read from its declaration alone.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum McpWrites {
    /// Nothing the session mounts.
    Nothing,
    /// The primary's filesystem, the workspace with it: a command placement,
    /// and a sidecar that sees the primary's view, whatever its own mounts.
    PrimaryFilesystem,
    /// These host paths: the workspace a sidecar mounts read-write, and its
    /// read-write mounts.
    Paths(Vec<PathBuf>),
}

/// How [`McpServices::quiesce`] quiesces a service.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Quiesce {
    /// Freeze its container; a call made meanwhile is refused, reason
    /// `paused`, so a model learns that it is paused rather than waiting.
    Pause,
    /// Freeze its container for a cut: a call made meanwhile waits for
    /// [`McpServices::resume`], and is refused, reason `paused`, if `until`
    /// comes first.
    #[non_exhaustive]
    PauseHolding {
        /// The cut's deadline.
        until: std::time::Instant,
    },
    /// Tell each of its calls to stop, close its transport and stop its
    /// container. A stopped service is not started again.
    Stop,
}

impl Quiesce {
    /// [`Quiesce::PauseHolding`] until `until`.
    pub fn pause_holding(until: std::time::Instant) -> Self {
        Self::PauseHolding { until }
    }
}

/// What [`McpServices::quiesce`] did. It never says quiesced before the
/// freeze or the stop returned.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum QuiesceOutcome {
    /// Frozen, or stopped, as `how` asked: the service and every service its
    /// container also hosts, in `services`.
    #[non_exhaustive]
    Quiesced {
        /// What was asked.
        how: Quiesce,
        /// Every service quiesced with it.
        services: Vec<String>,
    },
    /// Calls of `services` had not ended when the wait ran out, so nothing
    /// was frozen; their new calls are held until [`McpServices::resume`].
    #[non_exhaustive]
    InFlight {
        /// The calls still running, by id.
        calls: Vec<ServiceCallId>,
        /// The services held.
        services: Vec<String>,
    },
    /// The service cannot be quiesced: it runs in the primary, it is not
    /// running, or its container would not freeze or stop -- `why` says
    /// which. A freeze that failed leaves its new calls held until
    /// [`McpServices::resume`].
    #[non_exhaustive]
    Uncontrolled {
        /// Why.
        why: String,
    },
}

/// How an MCP service's server was stopped, in the report.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum McpStopHow {
    /// Its transport was closed and its container stopped: by the close, or
    /// earlier by [`Quiesce::Stop`].
    Stopped,
    /// Its transport was closed; it ends with the primary container.
    WithPrimary,
    /// It never started, so there was nothing to stop.
    NotStarted,
}

/// How a pause holds a service's new calls, in its event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum McpPauseHow {
    /// Refused, reason `paused`.
    Refuse,
    /// Held until the resume, or the cut's deadline.
    Hold,
}

/// One MCP service at the session's close, and how it was stopped.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct McpServiceStop {
    /// The service.
    pub service: String,
    /// Where it ran.
    pub placement: McpPlacementKind,
    /// How its start ended.
    pub outcome: McpOutcomeKind,
    /// How it was stopped.
    pub how: McpStopHow,
    /// Whether that stop is proven: a container that would not stop makes it
    /// `false`, and the session's report not proven stopped.
    pub proven: bool,
}

/// The waits between attempts to start a server: half a second, doubling to
/// eight, with no jitter -- a session's servers start once, so nothing herds.
const BACKOFF_FIRST: Duration = Duration::from_millis(500);
const BACKOFF_CAP: Duration = Duration::from_secs(8);

/// How long a stop waits, past the service policy's grace, for its calls to
/// be abandoned.
const STOP_MARGIN: Duration = Duration::from_secs(1);

/// What an attempt holds back from its connect when the budget bounds it:
/// the moment a failure its handshake has classified -- a refusal -- takes
/// to be reported as itself, before the attempt's own bound ends it as out
/// of time.
const REPORT_MARGIN: Duration = Duration::from_millis(250);

/// The host key of the primary container: a name no sidecar can have, so a
/// configured block named `primary` is a sidecar like any other.
pub(crate) const PRIMARY: &str = "<primary>";

/// One MCP service as planned: named, placed and checked, not yet started.
#[derive(Debug, Clone)]
pub(crate) struct Planned {
    pub(crate) name: String,
    /// The server's own name, where it differs from `name`.
    pub(crate) server: Option<String>,
    pub(crate) declared_in: McpDeclaredIn,
    pub(crate) placement: McpPlacementKind,
    /// The key of the container it runs in: [`PRIMARY`], or a sidecar's
    /// container name.
    pub(crate) host: String,
    pub(crate) launch: Launch,
    pub(crate) env: BTreeMap<String, EnvValue>,
    pub(crate) call_deadline: Option<Duration>,
    pub(crate) description: Option<String>,
    pub(crate) writes: McpWrites,
    /// Why it cannot start, found before anything did.
    pub(crate) misconfigured: Option<String>,
}

impl Planned {
    /// The service `name`, declared in `declared_in`, running in the `host`
    /// container as `launch` with `env`: no deadline or description of its
    /// own, and nothing found wrong yet.
    fn new(
        name: &str,
        declared_in: McpDeclaredIn,
        placement: McpPlacementKind,
        host: &str,
        (launch, env): (Launch, BTreeMap<String, EnvValue>),
        writes: McpWrites,
    ) -> Self {
        Self {
            name: name.to_string(),
            server: None,
            declared_in,
            placement,
            host: host.to_string(),
            launch,
            env,
            call_deadline: None,
            description: None,
            writes,
            misconfigured: None,
        }
    }

    /// Why it did not start, built from what OutRig knows of it: `message`,
    /// and its command with its environment as declared.
    fn cause(&self, message: String) -> McpStartupCause {
        McpStartupCause {
            service: self.name.clone(),
            server: self.server.clone(),
            placement: self.placement,
            declared_in: self.declared_in,
            command: shown_command(&self.launch, &self.env),
            exit: None,
            stderr_tail: None,
            message,
            kind: None,
            attempts: 0,
        }
    }
}

/// An image's `org.outrig.mcp` label entries, and the image.
#[derive(Debug, Clone, Default)]
pub(crate) struct Labels {
    /// The image, as messages name it.
    pub(crate) image: String,
    /// Its entries, by server name.
    pub(crate) servers: BTreeMap<String, McpServerSpec>,
}

/// What a planned service runs in.
#[derive(Debug, Clone)]
pub(crate) enum HostPlan {
    /// The primary container.
    Primary,
    /// A sidecar, as its spec, named for its container, describes it.
    Sidecar(Box<SidecarSpec>),
}

/// A session's MCP services, planned.
#[derive(Debug, Clone, Default)]
pub(crate) struct Plan {
    pub(crate) services: Vec<Planned>,
    pub(crate) hosts: BTreeMap<String, HostPlan>,
    /// The sidecars the launch starts, whose names no block of the
    /// configuration's can take.
    launched: BTreeSet<String>,
}

impl Plan {
    /// The services `owner` declared, in order, then what `configured`
    /// declares -- its primary servers, the primary image's label entries
    /// `labels` holds that it does not name, its sidecars' servers -- each
    /// checked. A configured server whose name `taken` says is taken, or that
    /// cannot be bound, is misconfigured; an owner's name was checked before.
    /// `workspace` is the host directory a sidecar's workspace access mounts,
    /// and `launched` names the sidecars the launch starts.
    pub(crate) fn new(
        owner: &[(String, McpServiceSpec)],
        configured: Option<&ConfiguredMcp>,
        labels: Labels,
        taken: impl Fn(&str) -> bool,
        workspace: Option<PathBuf>,
        launched: BTreeSet<String>,
    ) -> Self {
        let mut plan = Self {
            launched,
            ..Self::default()
        };
        plan.hosts.insert(PRIMARY.to_string(), HostPlan::Primary);
        for (name, spec) in owner {
            plan.owner(name, spec, workspace.as_deref());
        }
        if let Some(configured) = configured {
            plan.configured(configured, labels, &taken, workspace.as_deref());
        }
        plan
    }

    fn owner(&mut self, name: &str, spec: &McpServiceSpec, workspace: Option<&std::path::Path>) {
        let container = owner_sidecar(name);
        let owner = |placement, host: &str, launch, writes| {
            Planned::new(name, McpDeclaredIn::Owner, placement, host, launch, writes)
        };
        let mut one = match &spec.placement {
            McpPlacement::Command { command, env } => owner(
                McpPlacementKind::Command,
                PRIMARY,
                (Launch::Exec(command.clone()), env.clone()),
                McpWrites::PrimaryFilesystem,
            ),
            McpPlacement::PrimaryView { image, args, env } => {
                let sidecar =
                    SidecarSpec::from_image(&container, image).with_view(SidecarView::Primary);
                let mut one = owner(
                    McpPlacementKind::PrimaryView,
                    &container,
                    (Launch::Entrypoint(args.clone()), env.clone()),
                    McpWrites::PrimaryFilesystem,
                );
                one.misconfigured =
                    check_sidecar(&sidecar.clone().with_entrypoint_server(name, args));
                self.sidecar(sidecar);
                one
            }
            McpPlacement::Sidecar(declared) => {
                let mut sidecar = declared.clone();
                sidecar.name = container.clone();
                // Checked as declared, before an entrypoint server is set aside.
                let why = match sidecar.servers.len() {
                    1 => check_sidecar(&sidecar),
                    n => Some(format!(
                        "its sidecar hosts {n} servers, and a service is one server"
                    )),
                };
                let hosted = sidecar.servers.values().next().cloned();
                let mut one = owner(
                    placement_of(&sidecar),
                    &container,
                    hosted
                        .as_ref()
                        .map_or_else(|| (Launch::Exec(Vec::new()), BTreeMap::new()), launch_of),
                    writes_of(&sidecar, workspace),
                );
                one.call_deadline = hosted
                    .and_then(|hosted| hosted.call_timeout_secs())
                    .map(Duration::from_secs);
                one.misconfigured = why;
                // An entrypoint host's server is added at each attempt.
                sidecar.servers.retain(|_, hosted| !hosted.is_entrypoint());
                self.sidecar(sidecar);
                one
            }
        };
        one.description = spec.description.clone();
        if let Some(secs) = spec.call_timeout_secs {
            one.call_deadline = Some(Duration::from_secs(secs));
            one.misconfigured = one
                .misconfigured
                .or_else(|| call_timeout_misconfigured(secs));
        }
        self.push(one);
    }

    fn configured(
        &mut self,
        configured: &ConfiguredMcp,
        labels: Labels,
        taken: &impl Fn(&str) -> bool,
        workspace: Option<&std::path::Path>,
    ) {
        let session_default = configured.call_timeout_secs;
        let deadline = |own: Option<u64>| {
            Some(crate::config::effective_mcp_call_timeout(
                own,
                session_default,
            ))
        };
        let primary = configured
            .primary
            .iter()
            .map(|(server, spec)| (server.clone(), spec.clone(), McpDeclaredIn::Config))
            .chain(
                labels
                    .servers
                    .into_iter()
                    .filter(|(server, _)| !configured.names.contains(server))
                    .map(|(server, spec)| (server, spec, McpDeclaredIn::ImageLabel)),
            );
        for (server, spec, declared_in) in primary {
            let (command, env) = spec.normalize();
            let mut one = Planned::new(
                &server,
                declared_in,
                McpPlacementKind::Command,
                PRIMARY,
                (Launch::Exec(command), env),
                McpWrites::PrimaryFilesystem,
            );
            one.call_deadline = deadline(spec.call_timeout_secs());
            if declared_in == McpDeclaredIn::ImageLabel {
                one.misconfigured =
                    label_env_misconfigured(&labels.image, &server, &spec, &configured.env_allow);
            }
            self.configured_name(one, &server, taken);
        }
        for sidecar in &configured.sidecars {
            let mut spec = sidecar.spec.clone();
            // Never in place of an owner's container: the owner's server would
            // run, with its values, in what the configuration describes.
            let collides =
                self.hosts.contains_key(&spec.name) || self.launched.contains(&spec.name);
            // Checked as declared, before its entrypoint server is set aside to
            // be added at each attempt.
            let why = collides
                .then(|| {
                    format!(
                        "its sidecar block {:?} has the name of a container the session runs \
                         already -- an owner's MCP service's, or a sidecar its launch starts -- \
                         so it is not placed; rename the block",
                        spec.name
                    )
                })
                .or_else(|| sidecar.failed.clone())
                .or_else(|| check_sidecar(&spec));
            let servers = spec.servers.clone();
            spec.servers.retain(|_, hosted| !hosted.is_entrypoint());
            for (server, hosted) in &servers {
                let declared_in = match configured.from_labels.contains(server) {
                    true => McpDeclaredIn::ImageLabel,
                    false => McpDeclaredIn::Config,
                };
                let mut one = Planned::new(
                    server,
                    declared_in,
                    placement_of(&spec),
                    &spec.name,
                    launch_of(hosted),
                    writes_of(&spec, workspace),
                );
                one.call_deadline = deadline(hosted.call_timeout_secs());
                one.misconfigured = why.clone().or_else(|| {
                    (declared_in == McpDeclaredIn::ImageLabel).then(|| {
                        let declared = McpServerSpec::exec(Vec::<String>::new())
                            .with_env(hosted.env().clone());
                        label_env_misconfigured(
                            &spec.image,
                            server,
                            &declared,
                            &configured.env_allow,
                        )
                    })?
                });
                self.configured_name(one, server, taken);
            }
            if !collides {
                self.sidecar(spec);
            }
        }
        for (server, sidecar) in &configured.manual {
            let mut one = Planned::new(
                server,
                McpDeclaredIn::Config,
                McpPlacementKind::Sidecar,
                sidecar,
                (Launch::Exec(Vec::new()), BTreeMap::new()),
                McpWrites::Nothing,
            );
            one.misconfigured = Some(format!(
                "its sidecar {sidecar:?} is `start = \"manual\"`, and a session starts every \
                 server it places"
            ));
            self.configured_name(one, server, taken);
        }
    }

    /// `one`, a configured server named `server`, under the name it is bound
    /// as: its own when that is free and a binding could take it; else, for
    /// a name holding `-`, the name with `_` in its place where that is; else
    /// misconfigured, saying why.
    fn configured_name(&mut self, mut one: Planned, server: &str, taken: &impl Fn(&str) -> bool) {
        let free = |name: &str| {
            !taken(name)
                && !self.services.iter().any(|earlier| earlier.name == name)
                && crate::config::check_binding_name(name, None).is_ok()
        };
        let bound = if free(server) {
            Some(server.to_string())
        } else {
            let underscored = server.replace('-', "_");
            (underscored != server && free(&underscored)).then_some(underscored)
        };
        match bound {
            Some(bound) => {
                if bound != server {
                    one.server = Some(server.to_string());
                }
                one.name = bound;
            }
            None => {
                one.misconfigured.get_or_insert_with(|| {
                    "its name cannot be bound in this session: a service is reached as \
                     `outrig.bindings[name]` and a global, so it needs a Python identifier that \
                     no binding, service, channel or other server takes"
                        .to_string()
                });
                // Kept apart from a service of that name, which is listed under it.
                one.server = Some(server.to_string());
                one.name = format!("{server} (unbound)");
            }
        }
        self.push(one);
    }

    /// The sidecar `spec` describes, as one of the containers services run in;
    /// never in place of one planned before it.
    fn sidecar(&mut self, spec: SidecarSpec) {
        self.hosts
            .entry(spec.name.clone())
            .or_insert_with(|| HostPlan::Sidecar(Box::new(spec)));
    }

    /// `one`, checked against what holds no credential, and added.
    fn push(&mut self, mut one: Planned) {
        if one.misconfigured.is_none() {
            one.misconfigured = env_misconfigured(&one);
        }
        self.services.push(one);
    }
}

/// Why a server placed as `one` is with `one.env` cannot start, if it
/// cannot: a `${VAR}` in a placement that runs where the agent reads its
/// environment, or one under a key podman reads itself, which would put the
/// value on a command line.
fn env_misconfigured(one: &Planned) -> Option<String> {
    let references: Vec<&str> = one
        .env
        .iter()
        .filter_map(|(key, value)| match value {
            EnvValue::EnvRef(_) => Some(key.as_str()),
            EnvValue::Literal(_) => None,
        })
        .collect();
    let place = match one.placement {
        McpPlacementKind::Command => Some("the primary container, beside the interpreter"),
        McpPlacementKind::PrimaryView => Some("a sidecar that joins the primary's namespaces"),
        McpPlacementKind::Sidecar => None,
    };
    if let (false, Some(place)) = (references.is_empty(), place) {
        return Some(format!(
            "it holds a credential -- a `${{VAR}}` in its env ({}) -- and would run in {place}, \
             where agent code can read a process's environment; place it in a sidecar of its \
             own",
            references.join(", ")
        ));
    }
    let on_line: Vec<&str> = one
        .env
        .iter()
        .filter(|(key, value)| crate::engine_env::reference_on_command_line(key, value))
        .map(|(key, _)| key.as_str())
        .collect();
    (!on_line.is_empty()).then(|| {
        format!(
            "its env sets {} to a `${{VAR}}`, a key podman reads itself, so the value would \
             reach podman's command line rather than its environment",
            on_line.join(", ")
        )
    })
}

/// Why `image`'s label entry `spec` named `server` cannot start, if it
/// references a variable `[repo] env-allow` does not list.
fn label_env_misconfigured(
    image: &str,
    server: &str,
    spec: &McpServerSpec,
    env_allow: &[String],
) -> Option<String> {
    crate::container::embedded::check_label_server_env(image, server, spec, env_allow)
        .err()
        .map(|e| e.to_string())
}

/// Why a call deadline of `secs` cannot be a service's, if it cannot.
fn call_timeout_misconfigured(secs: u64) -> Option<String> {
    crate::config::check_mcp_call_timeout_secs("its call_timeout_secs", secs)
        .err()
        .map(|e| e.to_string())
}

/// Why the sidecar `spec` cannot start, if it cannot: the checks
/// `add_sidecar` makes of a spec.
fn check_sidecar(spec: &SidecarSpec) -> Option<String> {
    crate::outrig_::validate_sidecar(spec)
        .err()
        .map(|e| e.to_string())
}

/// How `hosted` runs, and its environment.
fn launch_of(hosted: &SidecarServerSpec) -> (Launch, BTreeMap<String, EnvValue>) {
    let launch = match hosted.command() {
        Some(command) => Launch::Exec(command.to_vec()),
        None => Launch::Entrypoint(hosted.args().to_vec()),
    };
    (launch, hosted.env().clone())
}

/// The placement a sidecar `spec` is: a primary view when its effective view
/// is the primary's.
fn placement_of(spec: &SidecarSpec) -> McpPlacementKind {
    match spec.view {
        SidecarView::Primary => McpPlacementKind::PrimaryView,
        _ => McpPlacementKind::Sidecar,
    }
}

/// What a sidecar `spec` can write: the primary's filesystem for a primary
/// view, whatever else it says; else the workspace, at `workspace`, when it
/// mounts it read-write, and its read-write mounts.
fn writes_of(spec: &SidecarSpec, workspace: Option<&std::path::Path>) -> McpWrites {
    if spec.view == SidecarView::Primary {
        return McpWrites::PrimaryFilesystem;
    }
    let mut paths = Vec::new();
    if spec.workspace == SidecarWorkspaceAccess::Rw
        && let Some(workspace) = workspace
    {
        paths.push(workspace.to_path_buf());
    }
    paths.extend(
        spec.mounts
            .iter()
            .filter(|mount| mount.access == MountAccess::ReadWrite)
            .map(|mount| mount.host.clone()),
    );
    match paths.is_empty() {
        true => McpWrites::Nothing,
        false => McpWrites::Paths(paths),
    }
}

/// `env`, shown as declared, before `command`: what a cause says was run.
fn shown_command(launch: &Launch, env: &BTreeMap<String, EnvValue>) -> Option<String> {
    let Launch::Exec(command) = launch else {
        return None;
    };
    let env: Vec<String> = env
        .iter()
        .map(|(key, value)| format!("{key}={} ", value.to_raw()))
        .collect();
    Some(format!(
        "{}{}",
        env.concat(),
        crate::mcp::render_command(command)
    ))
}

/// A session's MCP services: their outcomes, and the control a cut quiesces
/// them with. Cheap to clone. See
/// [`SessionBuilder::mcp`](super::SessionBuilder::mcp).
#[derive(Clone)]
pub struct McpServices(Arc<Table>);

impl std::fmt::Debug for McpServices {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpServices")
            .field("services", &self.0.entries.len())
            .finish_non_exhaustive()
    }
}

struct Table {
    events: Events,
    entries: Vec<Arc<Entry>>,
    hosts: BTreeMap<String, Host>,
    /// The service table each ready service is bound into, once it is.
    services: OnceLock<Services>,
    /// Held for the URL services an owner declares, and never read, logged
    /// or shown.
    #[expect(dead_code, reason = "the URL placement (`0003-57`) stamps them")]
    identity: Vec<(String, String)>,
}

/// One container, and what it runs.
struct Host {
    placement: Arc<dyn Placement>,
    /// How its servers end at a stop: [`McpStopHow::WithPrimary`] for the
    /// primary's, [`McpStopHow::Stopped`] for a sidecar's.
    stops: McpStopHow,
    /// Shared, so that a control step spawned to run to its end holds it from
    /// before it was spawned: see `Table::locked`.
    state: Arc<tokio::sync::Mutex<HostState>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HostState {
    Running,
    /// Its services' new calls are held, and it is not frozen: their calls
    /// were still running, or the freeze failed.
    Held,
    Paused,
    /// Stopped, and whether that was proven.
    Stopped(bool),
}

/// One MCP service, as its start left it.
struct Entry {
    planned: Planned,
    outcome: McpServiceOutcome,
    /// Its first listing.
    tools: Vec<McpTool>,
    /// The first line of what its server said of itself.
    instructions: Option<String>,
    /// How it was started, its values resolved once: what a reconnect starts
    /// it with again. `None` when it could not be, and so never ran.
    server: Option<Server>,
    /// Its client, until its transport is closed.
    client: Mutex<Option<Arc<McpClient>>>,
    /// What its attempts ran is not proven stopped: it ran in the primary,
    /// which runs on, or its sidecar would not stop once the start ended.
    unstopped: bool,
}

impl Entry {
    fn name(&self) -> &str {
        &self.planned.name
    }

    /// Whether it started: placed, handshaken and listed.
    fn started(&self) -> bool {
        matches!(self.outcome, McpServiceOutcome::Ready { .. })
    }

    /// Whether anything of it may run: it started, or what its start left
    /// was not proven stopped.
    fn may_run(&self) -> bool {
        self.started() || self.unstopped
    }

    fn client(&self) -> Option<Arc<McpClient>> {
        self.client.lock().expect("entry lock").clone()
    }

    /// Its client, taken: no call made from now on reaches it.
    fn take_client(&self) -> Option<Arc<McpClient>> {
        self.client.lock().expect("entry lock").take()
    }
}

/// What a session gives its MCP table to start with, besides its plan.
pub(crate) struct Start {
    pub(crate) policy: McpPolicy,
    pub(crate) identity: Vec<(String, String)>,
    pub(crate) secrets: Arc<dyn Secrets>,
    pub(crate) events: Events,
}

/// How one service's start ended.
struct Started {
    outcome: McpServiceOutcome,
    client: Option<McpClient>,
    tools: Vec<McpTool>,
    instructions: Option<String>,
}

impl Started {
    fn failed(outcome: McpServiceOutcome) -> Self {
        Self {
            outcome,
            client: None,
            tools: Vec::new(),
            instructions: None,
        }
    }
}

impl McpServices {
    /// Start every service `plan` holds, each in the placement `place` makes
    /// for its container, concurrently: its values resolved once through the
    /// session's secrets, then placed, handshaken and listed, a transient
    /// failure tried again until `by`, where the connect budget ends, and
    /// until `cancel` gives up on what has not started -- the session's start
    /// failed. What each came to is its outcome; [`McpServices::ready`] serves
    /// those that started.
    pub(crate) async fn start(
        start: Start,
        plan: Plan,
        place: impl Fn(&HostPlan) -> Arc<dyn Placement>,
        by: Instant,
        cancel: CancellationToken,
    ) -> Self {
        let Start {
            policy,
            identity,
            secrets,
            events,
        } = start;
        let secrets = Once::new(&*secrets);
        let launches: Vec<Result<Server, String>> = plan
            .services
            .iter()
            .map(|planned| match &planned.misconfigured {
                Some(why) => Err(why.clone()),
                None => resolve(planned, &secrets),
            })
            .collect();
        let hosts: BTreeMap<String, Host> = plan
            .hosts
            .iter()
            .filter(|(key, _)| {
                plan.services
                    .iter()
                    .zip(&launches)
                    .any(|(planned, launch)| launch.is_ok() && planned.host == **key)
            })
            .map(|(key, host)| {
                let stops = match host {
                    HostPlan::Primary => McpStopHow::WithPrimary,
                    HostPlan::Sidecar(_) => McpStopHow::Stopped,
                };
                let host = Host {
                    placement: place(host),
                    stops,
                    state: Arc::new(tokio::sync::Mutex::new(HostState::Running)),
                };
                (key.clone(), host)
            })
            .collect();
        // A sidecar that runs one server, which starts again with it.
        let alone = |key: &str| {
            hosts
                .get(key)
                .is_some_and(|host| host.stops == McpStopHow::Stopped)
                && plan
                    .services
                    .iter()
                    .zip(&launches)
                    .filter(|(planned, launch)| launch.is_ok() && planned.host == key)
                    .count()
                    == 1
        };
        let started = join_all(
            plan.services
                .iter()
                .zip(&launches)
                .map(|(planned, launch)| {
                    let placement = hosts.get(&planned.host).map(|host| &*host.placement);
                    let alone = alone(&planned.host);
                    start_one(
                        &events, planned, launch, placement, alone, &policy, by, &cancel,
                    )
                }),
        )
        .await;
        // A sidecar none of whose servers started serves nothing, and nothing
        // pauses it: stopped now, with whatever its attempts left running
        // there, rather than at the close. One that would not stop is kept
        // among the writers, as a server the primary ran is.
        let idle = hosts.iter().filter(|(key, host)| {
            host.stops == McpStopHow::Stopped
                && !plan
                    .services
                    .iter()
                    .zip(&started)
                    .any(|(planned, started)| {
                        planned.host == **key
                            && matches!(started.outcome, McpServiceOutcome::Ready { .. })
                    })
        });
        let unstopped: BTreeSet<String> = join_all(idle.map(|(key, host)| async move {
            let stopped = host.placement.stop().await;
            if let Err(why) = &stopped {
                tracing::warn!(target: "outrig::mcp", "{why}");
            }
            *host.state.lock().await = HostState::Stopped(stopped.is_ok());
            stopped.is_err().then(|| key.clone())
        }))
        .await
        .into_iter()
        .flatten()
        .collect();
        let entries = plan
            .services
            .into_iter()
            .zip(launches)
            .zip(started)
            .map(|((planned, launch), started)| {
                Arc::new(Entry {
                    unstopped: launch.is_ok()
                        && (planned.host == PRIMARY || unstopped.contains(&planned.host)),
                    planned,
                    outcome: started.outcome,
                    tools: started.tools,
                    instructions: started.instructions,
                    server: launch.ok(),
                    client: Mutex::new(started.client.map(Arc::new)),
                })
            })
            .collect();
        Self(Arc::new(Table {
            events,
            entries,
            hosts,
            services: OnceLock::new(),
            identity,
        }))
    }

    /// The services that started, to serve, each with its name.
    pub(crate) fn ready(&self) -> Vec<(String, Arc<dyn Service>)> {
        self.0
            .entries
            .iter()
            .filter(|entry| entry.started())
            .map(|entry| {
                let service: Arc<dyn Service> = Arc::new(McpService(Arc::clone(entry)));
                (entry.name().to_string(), service)
            })
            .collect()
    }

    /// The service table the ready services were bound into, for the control
    /// to hold and stop their calls in.
    pub(crate) fn bind(&self, services: &Services) {
        let _ = self.0.services.set(services.clone());
    }

    /// Each declared service, in the order declared, and how its start
    /// ended.
    pub fn outcomes(&self) -> Vec<(String, McpServiceOutcome)> {
        self.0
            .entries
            .iter()
            .map(|entry| (entry.name().to_string(), entry.outcome.clone()))
            .collect()
    }

    /// Each service that started, and what it can write, from its declaration
    /// alone: what a cut decides which to quiesce by. A service that never
    /// started is listed only when what its attempts ran may still write --
    /// it ran in the primary, or its sidecar would not stop once the start
    /// ended -- and quiescing it says why it cannot be.
    pub fn writers(&self) -> Vec<McpWriter> {
        self.0
            .entries
            .iter()
            .filter(|entry| entry.may_run())
            .map(|entry| McpWriter {
                service: entry.name().to_string(),
                placement: entry.planned.placement,
                writes: entry.planned.writes.clone(),
            })
            .collect()
    }

    /// Quiesce `service` as `how` says, and with it every service its
    /// container hosts -- a configuration's sidecar block shares one container
    /// among its servers, and a freeze or a stop reaches them all.
    ///
    /// A pause holds the services' new calls, waits up to `wait` for their
    /// calls in flight to end, and freezes the container (`podman pause`): a
    /// call still running when the wait runs out is
    /// [`QuiesceOutcome::InFlight`], with nothing frozen. A stop tells each of
    /// their calls to stop, reason `stopped` -- each ends `unknown`, its
    /// server sent the cancel notice -- closes their transports and stops the
    /// container. A service in the primary cannot be frozen apart from the
    /// interpreter, and is [`QuiesceOutcome::Uncontrolled`].
    ///
    /// The quiesce runs to its end whether or not anything waits on it: one
    /// dropped part-way -- an owner's timeout -- leaves its holds and the
    /// container as it finished them, for a later [`McpServices::resume`] or
    /// the close, which wait for it, to undo. One dropped while another
    /// control step still held the container does nothing.
    pub async fn quiesce(&self, service: &str, how: Quiesce, wait: Duration) -> QuiesceOutcome {
        let state = match self.0.locked(service).await {
            Ok(state) => state,
            Err(why) => return QuiesceOutcome::Uncontrolled { why },
        };
        let (this, service) = (self.clone(), service.to_string());
        let quiesced =
            tokio::spawn(async move { this.quiesce_now(&service, how, wait, state).await });
        finished(quiesced.await, "quiesce")
            .unwrap_or_else(|why| QuiesceOutcome::Uncontrolled { why })
    }

    async fn quiesce_now(
        &self,
        service: &str,
        how: Quiesce,
        wait: Duration,
        mut state: HostLock,
    ) -> QuiesceOutcome {
        let uncontrolled = |why: String| QuiesceOutcome::Uncontrolled { why };
        let Control {
            entry,
            host,
            siblings,
        } = match self.0.control(service) {
            Ok(control) => control,
            Err(why) => return uncontrolled(why),
        };
        if entry.planned.placement == McpPlacementKind::Command {
            return uncontrolled(format!("service {service:?}: {SHARES_THE_PRIMARY}"));
        }
        let Some(services) = self.0.services.get() else {
            return uncontrolled(format!("service {service:?} is not bound"));
        };
        let quiesced = QuiesceOutcome::Quiesced {
            how,
            services: names(&siblings),
        };
        let (hold, pause_how) = match (*state, how) {
            (HostState::Stopped(false), _) => {
                return uncontrolled(format!(
                    "service {service:?}'s container was stopped, and the stop is not proven"
                ));
            }
            (HostState::Stopped(true), Quiesce::Stop) => return quiesced,
            (HostState::Stopped(true), _) => {
                return uncontrolled(format!(
                    "service {service:?} was stopped, so there is nothing to pause"
                ));
            }
            (_, Quiesce::Stop) => {
                return match self
                    .0
                    .stop_host(host, &mut state, services, &siblings)
                    .await
                {
                    Ok(()) => quiesced,
                    Err(why) => uncontrolled(why),
                };
            }
            (_, Quiesce::PauseHolding { until }) => (
                Hold::Wait {
                    until: Instant::from_std(until),
                },
                McpPauseHow::Hold,
            ),
            (_, Quiesce::Pause) => (Hold::Refuse, McpPauseHow::Refuse),
        };
        let held = names(&siblings);
        // Set however it was held before: a call waiting on an earlier hold
        // reads this one at once.
        services.hold(&held, hold);
        let paused = |proven, calls: &[ServiceCallId]| {
            self.0
                .emit_each(&siblings, |service| Payload::McpServicePaused {
                    service,
                    how: pause_how,
                    proven,
                    calls: calls.to_vec(),
                });
        };
        if *state == HostState::Paused {
            paused(true, &[]);
            return quiesced;
        }
        let settled = tokio::time::timeout(wait, services.settled(&held)).await;
        if settled.is_err() {
            let calls = services.dispatched(&held);
            *state = HostState::Held;
            paused(false, &calls);
            return QuiesceOutcome::InFlight {
                calls,
                services: held,
            };
        }
        match host.placement.freeze().await {
            Ok(()) => {
                *state = HostState::Paused;
                paused(true, &[]);
                quiesced
            }
            Err(why) => {
                *state = HostState::Held;
                paused(false, &[]);
                uncontrolled(format!(
                    "service {service:?}'s container did not freeze: {why}"
                ))
            }
        }
    }

    /// Thaw what [`McpServices::quiesce`] paused and let the calls it held
    /// go on: every service `service`'s container hosts. Those services, or
    /// why they could not be resumed -- a stopped one is not started again.
    /// It runs to its end whether or not anything waits on it, as
    /// [`McpServices::quiesce`] does.
    pub async fn resume(&self, service: &str) -> Result<Vec<String>, String> {
        let state = self.0.locked(service).await?;
        let (this, service) = (self.clone(), service.to_string());
        let resumed = tokio::spawn(async move { this.resume_now(&service, state).await });
        finished(resumed.await, "resume")?
    }

    async fn resume_now(&self, service: &str, mut state: HostLock) -> Result<Vec<String>, String> {
        let Control { host, siblings, .. } = self.0.control(service)?;
        let resumed = names(&siblings);
        let thawed = match *state {
            HostState::Running => return Ok(resumed),
            HostState::Stopped(_) => {
                return Err(format!(
                    "service {service:?} was stopped, and is not started again"
                ));
            }
            HostState::Held => Ok(()),
            HostState::Paused => host.placement.thaw().await,
        };
        if let Err(why) = thawed {
            return Err(format!(
                "service {service:?}'s container did not thaw: {why}"
            ));
        }
        *state = HostState::Running;
        if let Some(services) = self.0.services.get() {
            services.release(&resumed);
        }
        self.0
            .emit_each(&siblings, |service| Payload::McpServiceResumed {
                service,
                proven: true,
            });
        Ok(resumed)
    }

    /// Close every service at the session's close, once its calls have
    /// drained -- every container at once: a paused one thawed, each
    /// transport closed, each sidecar stopped, one whose earlier stop was not
    /// proven stopped again. A server in the primary ends with it, and
    /// [`McpServices::closed`] is told whether it did. Why each container
    /// that may not have stopped did not prove it.
    pub(crate) async fn close(&self) -> Vec<String> {
        let stops = self.0.hosts.iter().map(|(key, host)| async move {
            let mut state = host.state.lock().await;
            let siblings = self.0.siblings(key);
            if host.stops == McpStopHow::WithPrimary {
                join_all(siblings.iter().map(|entry| shut_down(entry))).await;
                return None;
            }
            if *state == HostState::Stopped(true) {
                return None;
            }
            // What an earlier stop could not stop, it kept for this one.
            self.0
                .stop_container(host, &mut state, &siblings)
                .await
                .err()
        });
        join_all(stops).await.into_iter().flatten().collect()
    }

    /// Each service and how it was stopped, once the session's container has
    /// been: `primary` says whether that stop was proven, which a server
    /// running in it is stopped with -- announced now, and not before.
    pub(crate) async fn closed(&self, primary: bool) -> Vec<McpServiceStop> {
        let mut rows = Vec::new();
        for entry in &self.0.entries {
            let (how, proven) = match self.0.hosts.get(&entry.planned.host) {
                // Never placed, so nothing ran.
                None => (McpStopHow::NotStarted, true),
                // A server that did not start ran nothing when it was never
                // launched; what its attempts ran may have outlived them,
                // and ended with the primary.
                Some(host) if host.stops == McpStopHow::WithPrimary && !entry.started() => {
                    (McpStopHow::NotStarted, entry.server.is_none() || primary)
                }
                Some(host) if host.stops == McpStopHow::WithPrimary => {
                    self.0.events.emit(Payload::McpServiceStopped {
                        service: entry.name().to_string(),
                        how: host.stops,
                        proven: primary,
                    });
                    (host.stops, primary)
                }
                // Its container may have run though its server never
                // started: the row is as proven as that container's stop.
                Some(host) => (
                    match entry.started() {
                        true => host.stops,
                        false => McpStopHow::NotStarted,
                    },
                    matches!(*host.state.lock().await, HostState::Stopped(true)),
                ),
            };
            rows.push(McpServiceStop {
                service: entry.name().to_string(),
                placement: entry.planned.placement,
                outcome: entry.outcome.kind(),
                how,
                proven,
            });
        }
        rows
    }
}

/// What a control step spawned to run to its end came to: a panic raised
/// again, and a task the runtime shut down said so.
fn finished<T>(joined: Result<T, tokio::task::JoinError>, step: &str) -> Result<T, String> {
    joined.map_err(|e| match e.try_into_panic() {
        Ok(panic) => std::panic::resume_unwind(panic),
        Err(e) => format!("the {step} did not finish: {e}"),
    })
}

/// A container's state, locked for one control step.
type HostLock = tokio::sync::OwnedMutexGuard<HostState>;

/// What the control acts on for one service: see `Table::control`.
struct Control<'a> {
    entry: &'a Arc<Entry>,
    host: &'a Host,
    /// Every started service its container runs, itself included.
    siblings: Vec<&'a Arc<Entry>>,
}

/// The names of `entries`.
fn names(entries: &[&Arc<Entry>]) -> Vec<String> {
    entries
        .iter()
        .map(|entry| entry.name().to_string())
        .collect()
}

impl Table {
    fn entry(&self, name: &str) -> Option<&Arc<Entry>> {
        self.entries.iter().find(|entry| entry.name() == name)
    }

    /// The started services `host` runs, in the order declared.
    fn siblings(&self, host: &str) -> Vec<&Arc<Entry>> {
        self.entries
            .iter()
            .filter(|entry| entry.planned.host == host && entry.started())
            .collect()
    }

    /// What the control acts on for `service`: it, the container it runs in,
    /// and every started service that container runs; or why there is none.
    fn control(&self, service: &str) -> Result<Control<'_>, String> {
        let entry = self
            .entry(service)
            .ok_or_else(|| format!("the session has no MCP service {service:?}"))?;
        let host = self
            .hosts
            .get(&entry.planned.host)
            .filter(|_| entry.may_run())
            .ok_or_else(|| format!("service {service:?} never started"))?;
        Ok(Control {
            entry,
            host,
            siblings: self.siblings(&entry.planned.host),
        })
    }

    /// `service`'s container, locked before a control step on it is spawned:
    /// a step asked for after it, once it is given up on, cannot overtake it.
    async fn locked(&self, service: &str) -> Result<HostLock, String> {
        let host = self.control(service)?.host;
        Ok(Arc::clone(&host.state).lock_owned().await)
    }

    /// Publish what `payload` makes of each of `entries`' names.
    fn emit_each(&self, entries: &[&Arc<Entry>], payload: impl Fn(String) -> Payload) {
        for entry in entries {
            self.events.emit(payload(entry.name().to_string()));
        }
    }

    /// Stop `host`, at a [`Quiesce::Stop`]: hold its services' new calls, tell
    /// those in flight to stop, wait for them to be abandoned, and stop it.
    async fn stop_host(
        &self,
        host: &Host,
        state: &mut HostState,
        services: &Services,
        siblings: &[&Arc<Entry>],
    ) -> Result<(), String> {
        let held = names(siblings);
        services.hold(&held, Hold::Stopped);
        services.stop_calls(&held, ServiceReason::Stopped);
        let _ =
            tokio::time::timeout(services.grace() + STOP_MARGIN, services.drained_of(&held)).await;
        self.stop_container(host, state, siblings).await
    }

    /// Stop `host`, which runs `siblings`: thawed first when paused, each
    /// transport closed, then what it runs stopped.
    async fn stop_container(
        &self,
        host: &Host,
        state: &mut HostState,
        siblings: &[&Arc<Entry>],
    ) -> Result<(), String> {
        if *state == HostState::Paused
            && let Err(why) = host.placement.thaw().await
        {
            tracing::warn!(target: "outrig::mcp", "a paused sidecar did not thaw: {why}");
        }
        join_all(siblings.iter().map(|entry| shut_down(entry))).await;
        let stopped = host.placement.stop().await;
        *state = HostState::Stopped(stopped.is_ok());
        self.emit_each(siblings, |service| Payload::McpServiceStopped {
            service,
            how: host.stops,
            proven: stopped.is_ok(),
        });
        stopped
    }
}

/// Start `planned`, as `launch` says, in `placement` -- `alone` when that is
/// a sidecar running it and nothing else -- by `deadline`, where `policy`'s
/// connect budget ends: what it came to.
#[allow(clippy::too_many_arguments)]
async fn start_one(
    events: &Events,
    planned: &Planned,
    launch: &Result<Server, String>,
    placement: Option<&dyn Placement>,
    alone: bool,
    policy: &McpPolicy,
    deadline: Instant,
    cancel: &CancellationToken,
) -> Started {
    let (Ok(server), Some(placement)) = (launch, placement) else {
        let why = launch.as_ref().err().cloned().unwrap_or_default();
        let cause = planned.cause(why);
        events.emit(Payload::McpServiceFailed {
            service: planned.name.clone(),
            outcome: McpOutcomeKind::Misconfigured,
            cause: cause.clone(),
            attempts: 0,
            duration: Duration::ZERO,
        });
        return Started::failed(McpServiceOutcome::Misconfigured(cause));
    };
    let since = Instant::now();
    let mut delay = BACKOFF_FIRST;
    let mut attempts = 0;
    let failure = loop {
        attempts += 1;
        let left = deadline.saturating_duration_since(Instant::now());
        // A listing's failure, kept before its transport is closed: a server
        // that ignores its closed stdin can hold the close past the attempt's
        // end, which does not make the attempt one that timed out.
        let mut unlisted = None;
        let attempt = async {
            let within = policy.attempt.min(left.saturating_sub(REPORT_MARGIN));
            let client = placement.connect(server, within).await?;
            let left = deadline.saturating_duration_since(Instant::now());
            match client.list_tools_within(policy.attempt.min(left)).await {
                Ok(tools) => Ok((client, tools)),
                Err(e) => {
                    unlisted = Some(Failed::of(&e));
                    let _ = client.shutdown().await;
                    Err(e)
                }
            }
        };
        // The whole attempt, its container's preparation included, within
        // what is left of the budget: one that has not finished then is
        // dropped, which removes a container it created and undoes its
        // attachment, and nothing starts after the budget.
        let attempted = tokio::select! {
            attempted = tokio::time::timeout(left, attempt) => attempted,
            () = cancel.cancelled() => break Failed::abandoned(),
        };
        let failed = match attempted {
            Ok(Ok((client, tools))) => {
                let started = Started {
                    outcome: McpServiceOutcome::Ready {
                        tools: tools.len(),
                        listing: 1,
                        attempts,
                    },
                    instructions: client.instructions(),
                    // The service table bounds each call -- its deadline, then
                    // the grace a call told to stop has, then dropping it,
                    // which sends the server the cancel notice -- and says how
                    // it ended. A deadline of the client's own would end it
                    // first, inside the grace, as a failure.
                    client: Some(client.with_call_timeout(Duration::MAX)),
                    tools,
                };
                events.emit(Payload::McpServiceStarted {
                    service: planned.name.clone(),
                    server: planned.server.clone(),
                    placement: planned.placement,
                    declared_in: planned.declared_in,
                    tools: started.tools.len(),
                    listing: 1,
                    attempts,
                    duration: since.elapsed(),
                });
                return started;
            }
            Ok(Err(e)) => Failed::of(&e),
            Err(_) => unlisted.unwrap_or_else(|| Failed::timed_out(left)),
        };
        if failed.class != StartupClass::Transient {
            break failed;
        }
        // Its container starts again with it: what the failed attempt left
        // running -- a server that ignored its closed stdin -- does not run
        // beside the next.
        if alone {
            let stopped = tokio::select! {
                stopped = placement.stop() => stopped,
                () = cancel.cancelled() => break Failed::abandoned(),
            };
            if let Err(why) = stopped {
                let message = format!(
                    "{} -- and its container did not stop, so it is not started again: {why}",
                    failed.message
                );
                break Failed { message, ..failed };
            }
        }
        // Once the stop has, so a stop that ran past the budget starts
        // nothing more.
        let after = Instant::now() + delay;
        if after >= deadline {
            break Failed {
                message: format!(
                    "{} -- and the {:?} connect budget is spent",
                    failed.message, policy.connect_budget
                ),
                ..failed
            };
        }
        events.emit(Payload::McpServiceRetry {
            service: planned.name.clone(),
            attempt: attempts,
            delay,
            cause: McpRetryCause {
                kind: failed.kind,
                message: failed.message.clone(),
            },
        });
        tokio::select! {
            () = tokio::time::sleep_until(after) => {}
            () = cancel.cancelled() => break Failed::abandoned(),
        }
        delay = (delay * 2).min(BACKOFF_CAP);
    };
    let cause = McpStartupCause {
        exit: failure.exit,
        stderr_tail: failure.stderr_tail,
        kind: failure.kind,
        attempts,
        ..planned.cause(failure.message)
    };
    let outcome = match failure.class {
        StartupClass::Transient => McpServiceOutcome::Unavailable(cause.clone()),
        StartupClass::Refused => McpServiceOutcome::Refused(cause.clone()),
        StartupClass::Misconfigured => McpServiceOutcome::Misconfigured(cause.clone()),
        StartupClass::Unclassified => McpServiceOutcome::Unclassified(cause.clone()),
    };
    events.emit(Payload::McpServiceFailed {
        service: planned.name.clone(),
        outcome: outcome.kind(),
        cause,
        attempts,
        duration: since.elapsed(),
    });
    Started::failed(outcome)
}

/// Close `entry`'s transport, once nothing calls it: a client a call still
/// holds is dropped instead, which ends its transport all the same.
async fn shut_down(entry: &Entry) {
    let Some(client) = entry.take_client() else {
        return;
    };
    match Arc::try_unwrap(client) {
        Ok(client) => {
            if let Err(e) = client.shutdown().await {
                tracing::warn!(target: "outrig::mcp", "closing {:?}'s transport: {e}", entry.name());
            }
        }
        Err(_) => tracing::warn!(
            target: "outrig::mcp",
            "service {:?}'s client is still held at its stop; relying on Drop",
            entry.name()
        ),
    }
}

/// One failed attempt, classified.
struct Failed {
    class: StartupClass,
    kind: Option<McpFailureKind>,
    message: String,
    exit: Option<String>,
    stderr_tail: Option<String>,
}

impl Failed {
    /// What `error`, from placing, handshaking or listing a server, means.
    fn of(error: &OutrigError) -> Self {
        let plain = |class, message: String| Self {
            class,
            kind: None,
            message,
            exit: None,
            stderr_tail: None,
        };
        match error {
            OutrigError::McpStartupFailed(failure) => Self {
                class: failure.class,
                kind: Some(failure.kind),
                message: failure.classified.message.clone(),
                exit: Some(failure.exit.clone()),
                stderr_tail: Some(failure.stderr_tail.clone()),
            },
            OutrigError::McpToolsListFailed {
                source,
                stderr_tail,
                ..
            } => Self {
                class: match source.kind {
                    McpFailureKind::Transport | McpFailureKind::Timeout => StartupClass::Transient,
                    _ => StartupClass::Unclassified,
                },
                kind: Some(source.kind),
                message: format!("tools/list failed: {}", source.message),
                exit: None,
                stderr_tail: Some(stderr_tail.clone()),
            },
            OutrigError::McpEnvResolveFailed { .. }
            | OutrigError::Configuration(_)
            | OutrigError::ConfigValidation(_)
            | OutrigError::FilesystemHelperUnavailable => {
                plain(StartupClass::Misconfigured, error.to_string())
            }
            _ => plain(StartupClass::Unclassified, error.to_string()),
        }
    }

    /// The attempt had not finished when the `left` of the connect budget ran
    /// out: its container's preparation, or its server's handshake.
    fn timed_out(left: Duration) -> Self {
        Self {
            class: StartupClass::Transient,
            kind: Some(McpFailureKind::Timeout),
            message: format!("the attempt had not finished within the {left:?} left"),
            exit: None,
            stderr_tail: None,
        }
    }

    /// The session's start failed while this one's went on, and was given up.
    fn abandoned() -> Self {
        Self {
            class: StartupClass::Transient,
            kind: None,
            message: "the session's start failed before it started, so it was given up".into(),
            exit: None,
            stderr_tail: None,
        }
    }
}

/// How `planned` starts, its environment resolved through `secrets`; or why
/// a value has none.
fn resolve(planned: &Planned, secrets: &Once<'_>) -> Result<Server, String> {
    let mut env = BTreeMap::new();
    for (key, value) in &planned.env {
        let resolved = ResolvedEnvValue::resolve_with(value.clone(), secrets)
            .map_err(|e| format!("its env {key} = {}: {e}", value.to_raw()))?;
        env.insert(key.clone(), resolved);
    }
    Ok(Server {
        name: planned
            .server
            .clone()
            .unwrap_or_else(|| planned.name.clone()),
        declared: Some(match planned.declared_in {
            McpDeclaredIn::Owner => "the session's owner",
            McpDeclaredIn::Config => McpDeclarationSource::ConfigToml.description(),
            McpDeclaredIn::ImageLabel => McpDeclarationSource::ImageLabel.description(),
        }),
        launch: planned.launch.clone(),
        env,
    })
}

/// The session's secrets, each variable asked once however many services
/// name it.
struct Once<'a> {
    secrets: &'a dyn Secrets,
    asked: Mutex<HashMap<String, Result<String, Missing>>>,
}

/// Why the secrets had no value for a variable, kept to say so again.
#[derive(Debug, Clone, Copy)]
enum Missing {
    NotPresent,
    NotUnicode,
    Unresolved,
}

impl<'a> Once<'a> {
    fn new(secrets: &'a dyn Secrets) -> Self {
        Self {
            secrets,
            asked: Mutex::new(HashMap::new()),
        }
    }
}

impl Secrets for Once<'_> {
    fn resolve(&self, var: &str) -> std::result::Result<String, ApiKeyError> {
        let mut asked = self.asked.lock().expect("secrets lock");
        let answer = asked.entry(var.to_string()).or_insert_with(|| {
            self.secrets.resolve(var).map_err(|e| match e {
                ApiKeyError::NotPresent { .. } => Missing::NotPresent,
                ApiKeyError::NotUnicode { .. } => Missing::NotUnicode,
                _ => Missing::Unresolved,
            })
        });
        let var = var.to_string();
        answer.clone().map_err(|missing| match missing {
            Missing::NotPresent => ApiKeyError::NotPresent { var },
            Missing::NotUnicode => ApiKeyError::NotUnicode { var },
            Missing::Unresolved => ApiKeyError::Unresolved { var },
        })
    }
}

/// An MCP service, as the service table calls it.
struct McpService(Arc<Entry>);

impl Service for McpService {
    fn describe(&self) -> ServiceDescription {
        let entry = &self.0;
        let line = entry
            .planned
            .description
            .clone()
            .or_else(|| entry.instructions.clone())
            .unwrap_or_else(|| format!("The MCP server {:?}.", entry.name()));
        let mut description = ServiceDescription::new(line).of_kind(ServiceKind::Mcp);
        description.call_deadline = entry.planned.call_deadline;
        for tool in &entry.tools {
            let mut method = MethodDescription::new(
                tool.name.clone(),
                tool.description.clone().unwrap_or_default(),
                tool.input_schema.clone(),
            );
            method.result = tool.output_schema.clone();
            method.extras = extras(tool);
            description = description.with_method(method);
        }
        description
    }

    fn call(&self, method: &str, args: Value, _cx: CallContext) -> ServiceFuture<'_> {
        // The token is not watched here: the table abandons a call told to
        // stop once its grace is out, and dropping the call is what sends the
        // server the cancel notice.
        let client = self.0.client();
        let method = method.to_string();
        Box::pin(async move {
            let Some(client) = client else {
                return Err(ServiceError::new(
                    ServiceErrorCategory::Unavailable,
                    "the server is not connected",
                ));
            };
            match client.call_tool(&method, args).await {
                Ok(result) => serde_json::to_value(result_to_rmcp(result))
                    .map_err(|e| ServiceError::new(ServiceErrorCategory::Failed, e.to_string())),
                Err(OutrigError::McpCallToolFailed { source, .. }) => {
                    let category = match source.kind {
                        McpFailureKind::Transport | McpFailureKind::Timeout => {
                            ServiceErrorCategory::Unavailable
                        }
                        McpFailureKind::Canceled => ServiceErrorCategory::Cancelled,
                        McpFailureKind::Protocol => ServiceErrorCategory::Failed,
                        _ => ServiceErrorCategory::Other,
                    };
                    Err(ServiceError::new(category, source.message))
                }
                Err(e) => Err(ServiceError::new(
                    ServiceErrorCategory::Failed,
                    e.to_string(),
                )),
            }
        })
    }
}

/// What a tool's descriptor holds beyond its name, description and schemas,
/// as the protocol spells it: `title`, `annotations`, `icons` and `_meta`.
fn extras(tool: &McpTool) -> Map<String, Value> {
    let rmcp = tool_to_rmcp(tool.name.clone(), tool, Arc::new(Map::new()));
    let Ok(Value::Object(all)) = serde_json::to_value(rmcp) else {
        return Map::new();
    };
    all.into_iter()
        .filter(|(key, _)| matches!(key.as_str(), "title" | "annotations" | "icons" | "_meta"))
        .collect()
}

/// Whether a declaration's `headers` name one of the owner's identity
/// headers, which no declaration may: they are the owner's to stamp, on the
/// URL services the owner declares (`0003-57`). Header names compare without
/// regard to case.
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "the URL placement (`0003-57`) has headers")
)]
pub(crate) fn names_identity_header<'a>(
    headers: impl IntoIterator<Item = &'a String>,
    identity: &[(String, String)],
) -> Option<String> {
    headers.into_iter().find_map(|header| {
        identity
            .iter()
            .any(|(name, _)| name.eq_ignore_ascii_case(header))
            .then(|| header.clone())
    })
}

/// Refuse an MCP service name an owner gives that the session cannot give:
/// one a binding could not take, one a binding `declared` or a service or
/// channel `taken` takes, or one given twice.
/// The sidecar an owner's MCP service `name` runs in, when it runs in one.
fn owner_sidecar(name: &str) -> String {
    format!("mcp-{name}")
}

/// Whether each owner's MCP service in `mcp` that runs in a sidecar can: the
/// launch's `sidecars` would take its container's name, and only one
/// container has a name.
pub(crate) fn check_launch_sidecars(
    mcp: &[(String, McpServiceSpec)],
    sidecars: &[SidecarSpec],
) -> crate::error::Result<()> {
    let in_sidecar = mcp.iter().filter(|(_, spec)| {
        matches!(
            spec.placement,
            McpPlacement::Sidecar(_) | McpPlacement::PrimaryView { .. }
        )
    });
    for (name, _) in in_sidecar {
        let container = owner_sidecar(name);
        if sidecars.iter().any(|sidecar| sidecar.name == container) {
            return Err(OutrigError::Configuration(format!(
                "the MCP service `{name}` runs in a sidecar named `{container}`, which a sidecar \
                 the launch starts is named already; rename one of them"
            )));
        }
    }
    Ok(())
}

/// Whether `budget` can be a connect budget: from a second to an hour, as
/// the configuration's `mcp-connect-budget-secs` is.
pub(crate) fn check_connect_budget(budget: Duration) -> std::result::Result<(), String> {
    let max = crate::config::MCP_CALL_TIMEOUT_SECS_CEILING;
    match (Duration::from_secs(1)..=Duration::from_secs(max)).contains(&budget) {
        true => Ok(()),
        false => Err(format!(
            "an MCP connect budget is from 1 to {max} seconds, as `mcp-connect-budget-secs` is, \
             not {budget:?}"
        )),
    }
}

pub(crate) fn check_names(
    declared: &[super::bindings::Declared],
    taken: &[&str],
    mcp: &[(String, McpServiceSpec)],
) -> crate::error::Result<()> {
    for (index, (name, _)) in mcp.iter().enumerate() {
        let earlier = mcp[..index].iter().map(|(earlier, _)| earlier.as_str());
        let why = super::services::name_taken(name, "mcp", declared, earlier).or_else(|| {
            taken
                .contains(&name.as_str())
                .then(|| "is a service's or a channel's name already".to_string())
        });
        if let Some(why) = why {
            return Err(OutrigError::Configuration(format!(
                "the MCP service `{name}` given through `mcp` {why}: a service's name is a global \
                 in every kernel, so it names one service"
            )));
        }
    }
    Ok(())
}
