//! Shared bootstrap for `outrig run` and (future) `outrig mcp`. Lifts the
//! sequence of "load + merge config -> resolve container -> ensure image ->
//! start + bootstrap container -> session row + log dir" out of [`run`]
//! so both subcommands hit the same code path.
//!
//! Three pieces:
//!
//! - [`setup`] -- everything from config-load through "container started +
//!   bootstrapped + session row + log dir created", returning a populated
//!   [`SessionSetup`]. Stops *before* MCP children connect.
//! - [`connect_mcp_clients`] -- reads any image-embedded MCP
//!   config, applies repo-config overrides, and spawns one [`McpClient`] per
//!   merged backing MCP in `BTreeMap` (key-sorted, deterministic) iteration
//!   order. Adapter construction stays in the caller because only the REPL
//!   path consumes adapters.
//! - [`teardown`] -- mirror of the cleanup tail: graceful MCP shutdowns
//!   (drop adapters first so [`Arc::try_unwrap`] succeeds), then stop the
//!   container, then finalize the session row. Errors are logged and never
//!   override the caller's outcome.
//!
//! The MCP children are `podman exec` processes whose stdio rides through
//! the container; tearing the container down before shutting them down
//! races their pipes, so the order in [`teardown`] is load-bearing.
//!
//! [`run`]: crate::cli::run

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use crate::cli::env_arg::CliEnvEntries;
use crate::cli::volume_arg::CliVolume;
use crate::cli::watcher::SessionWatcher;
use crate::error::{CliError, OutrigError, Result};
use crate::llm;
use crate::paths::{default_session_root, repo_root_from_config_path};
use crate::session::{self, Session, SessionId, SessionStore};
use outrig::config::{
    Config, ImageConfig, McpServerSpec, MistralrsDeviceSpec, MountAccess, MountConfig, NetworkMode,
    SidecarOnFailure, SidecarStart,
};
use outrig::container::{
    Container, ContainerCapabilities, ContainerLaunchSpec, ContainerMount, ContainerWorkspace,
    LABEL_SESSION, LABEL_SIDECAR, embedded,
    sidecar::{self, Placement, SessionMcpPlan, SidecarPlan},
};
use outrig::image::{self, ImageTag};
use outrig::network::NetworkInterceptor;
use outrig::{McpClient, Transcript};

pub(crate) const STOP_GRACE: Duration = Duration::from_secs(2);

pub(crate) struct ProgressSpan {
    started: Instant,
}

impl ProgressSpan {
    pub(crate) fn start(label: impl Into<String>) -> Self {
        let label = label.into();
        eprintln!("[outrig] {label}");
        Self {
            started: Instant::now(),
        }
    }

    pub(crate) fn done(self, message: impl AsRef<str>) {
        eprintln!(
            "[outrig] {} ({})",
            message.as_ref(),
            format_elapsed(self.started.elapsed())
        );
    }
}

pub(crate) fn plural<'a>(count: usize, singular: &'a str, plural: &'a str) -> &'a str {
    if count == 1 { singular } else { plural }
}

fn format_elapsed(duration: Duration) -> String {
    let millis = duration.as_millis();
    if millis < 1_000 {
        return format!("{millis}ms");
    }
    let secs = duration.as_secs();
    if secs < 60 {
        return format!("{:.1}s", duration.as_secs_f64());
    }
    format!("{}m{:02}s", secs / 60, secs % 60)
}

fn resolve_workspace_host(repo_root: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        repo_root.join(path)
    }
}

/// Inputs to [`setup`]. Borrowed to keep the call site cheap; the lifetime
/// is the caller's stack frame.
pub struct SessionSetupArgs<'a> {
    pub repo_cfg_path: &'a Path,
    pub global_cfg_path: &'a Path,
    pub session_root_flag: Option<&'a Path>,
    pub image_flag: Option<&'a str>,
    /// Existing session id or podman container name to attach to instead of
    /// starting a fresh container. Used by `outrig mcp --attach`.
    pub attach_target: Option<&'a str>,
    /// Raw `--agent` flag. Read only when `require_agent = true`; ignored
    /// otherwise (and `outrig mcp` always passes `None`).
    pub agent_flag: Option<&'a str>,
    /// Raw `--model` flag. Read only when `require_agent = true`; ignored
    /// otherwise (and `outrig mcp` always passes `None`).
    pub model_override: Option<&'a str>,
    /// `true` for `outrig run`: [`setup`] resolves an agent from
    /// `agent_flag.or(cfg.default_agent)` (errors if neither) and lets
    /// `agent.image` participate in the container fallback.
    /// `false` for `outrig mcp`: no agent at all -- `llm::resolve_agent` is
    /// not called, `cfg.default_agent` is not consulted, the resulting
    /// [`Session::agent_name`] is `None`, and the container cascade is
    /// `image_flag -> default_image` only.
    pub require_agent: bool,
    pub explicit_session_dir: Option<&'a Path>,
    pub network_mode_override: Option<NetworkMode>,
    pub device_override: Option<MistralrsDeviceSpec>,
    /// Extra `--volume HOST:CONTAINER[:ro|rw]` mounts appended to the
    /// container's workspace mounts. Rejected with `--attach`.
    pub volumes: &'a [CliVolume],
    /// Start `start = "auto"` sidecars declared by the image config. `true`
    /// for `outrig run` and `outrig mcp` serving; `false` for
    /// `outrig mcp show-merged`, which plans placement (including sidecar
    /// label merges) without launching sidecar containers.
    pub start_sidecars: bool,
    /// CLI `--env` overlay entries. Consulted during setup only for
    /// entrypoint-stdio sidecars, whose env must be resolved at container
    /// create time (`podman start` carries no `--env`); exec-stdio servers
    /// keep resolving at connect time in [`connect_mcp_clients`].
    pub cli_env: &'a CliEnvEntries,
    pub verbose: u8,
}

/// Every container the session owns, sidecars keyed by config name.
/// `sidecars` is declared before `primary` so field-order `Drop` reaps
/// sidecars first, mirroring orderly teardown.
pub struct SessionContainers {
    pub sidecars: BTreeMap<String, Container>,
    pub primary: Container,
}

impl SessionContainers {
    /// The container hosting `placement`, or `None` for a sidecar that was
    /// skipped (warn'd, manual, or reaped).
    pub fn container_for(&self, placement: &Placement) -> Option<&Container> {
        match placement {
            Placement::Primary => Some(&self.primary),
            Placement::Sidecar(name) => self.sidecars.get(name),
        }
    }

    /// Started sidecar container names, the shape the session store records.
    pub fn sidecar_names(&self) -> Vec<String> {
        self.sidecars
            .values()
            .map(|container| container.name().to_string())
            .collect()
    }
}

/// The mutable session state that lives from setup to [`teardown`] and
/// that `/sidecar add` grows mid-session. Assembled by each command after
/// destructuring [`SessionSetup`] (`mcp_arcs` starts empty and fills as
/// clients connect). Field order mirrors teardown order (watcher disarm ->
/// MCP shutdown -> interceptor -> containers) so an implicit `Drop` on an
/// abort path stays orderly.
pub struct SessionRuntime {
    pub watcher: Option<SessionWatcher>,
    pub mcp_arcs: Vec<Arc<McpClient>>,
    pub network: Option<NetworkInterceptor>,
    pub containers: SessionContainers,
}

impl SessionRuntime {
    /// Assemble a runtime from freshly set-up session pieces; `mcp_arcs`
    /// starts empty and fills as clients connect.
    pub fn new(
        watcher: Option<SessionWatcher>,
        network: Option<NetworkInterceptor>,
        containers: SessionContainers,
    ) -> Self {
        Self {
            watcher,
            mcp_arcs: Vec::new(),
            network,
            containers,
        }
    }
}

/// Output of [`setup`]: every long-lived value the post-setup pipeline
/// needs (REPL build, MCP children, teardown). The containers are already
/// started + bootstrapped; the session row is already on disk.
pub struct SessionSetup {
    pub cfg: Config,
    pub image_cfg_name: String,
    pub image_cfg: ImageConfig,
    pub image_tag: ImageTag,
    pub containers: SessionContainers,
    pub sid: SessionId,
    pub session: Session,
    pub session_dir: PathBuf,
    pub log_dir: PathBuf,
    pub store: SessionStore,
    /// Directory the repo config was resolved against; mid-session sidecar
    /// starts resolve mount paths against it.
    pub repo_root: PathBuf,
    pub attached: bool,
    pub network: Option<NetworkInterceptor>,
    /// Full placement plan (config + label merges), including servers in
    /// skipped sidecars. `show-merged` renders from this;
    /// [`connect_mcp_clients`] connects the subset whose container runs.
    pub mcp_plan: SessionMcpPlan,
    /// Present only when sidecars started; orderly teardown disarms it
    /// before stopping containers.
    pub watcher: Option<SessionWatcher>,
}

#[derive(Debug)]
struct AttachResolution {
    container_name: String,
    image_cfg_name: String,
}

/// Run the shared bootstrap. Returns once the container is up, the runtime
/// user is bootstrapped, and the session directory + log dir exist.
pub async fn setup(args: SessionSetupArgs<'_>) -> Result<SessionSetup> {
    let repo_root = repo_root_from_config_path(args.repo_cfg_path);
    let span = ProgressSpan::start("loading config");
    let mut cfg = if args.require_agent {
        Config::load_for_run(
            &repo_root,
            Some(args.global_cfg_path),
            args.agent_flag,
            args.model_override,
        )?
    } else {
        Config::load(&repo_root, Some(args.global_cfg_path))?
    };
    span.done("config loaded");
    if !args.repo_cfg_path.exists() {
        eprintln!(
            "[outrig] no repo config found; using current directory as workspace ({})",
            repo_root.display()
        );
    }

    let session_root =
        session::resolve_session_root(args.session_root_flag, &cfg, &default_session_root());
    let store = SessionStore::new(session_root);
    let attach = match args.attach_target {
        Some(target) if args.require_agent => {
            return Err(OutrigError::Configuration(format!(
                "--attach {target:?} is only supported by `outrig mcp`"
            ))
            .into());
        }
        Some(target) => Some(resolve_attach_target(target, args.image_flag, &store)?),
        None => None,
    };
    let network_mode = args.network_mode_override.unwrap_or(cfg.network.mode);
    if attach.is_some() && matches!(network_mode, NetworkMode::Audit | NetworkMode::Filter) {
        return Err(OutrigError::Configuration(
            "`--network audit` and `--network filter` cannot be used with \
             `outrig mcp --attach`; start a fresh session to install network monitoring"
                .to_string(),
        )
        .into());
    }
    if network_mode == NetworkMode::Filter && !cfg.network.has_policy_entries() {
        return Err(OutrigError::Configuration(
            "network filter mode requires at least one global [network] allow or deny entry"
                .to_string(),
        )
        .into());
    }

    // Extra `--volume` mounts append to the workspace mounts and are validated
    // with the same rules as config `[workspace.mounts]`. A borrowed container
    // (`--attach`) has fixed mounts, so reject `--volume` there.
    if !args.volumes.is_empty() {
        if attach.is_some() {
            return Err(OutrigError::Configuration(
                "--volume cannot be combined with --attach; a borrowed container's \
                 mounts are fixed when it is created"
                    .to_string(),
            )
            .into());
        }
        for vol in args.volumes {
            cfg.workspace.mounts.push(MountConfig {
                host_path: vol.host.clone(),
                container_path: vol.container.clone(),
                access: vol.access,
            });
        }
        cfg.validate_workspace_mounts(Some(&repo_root))?;
    }

    // Agent presence is checked before any container work so the failure
    // mode is identical for `outrig run` regardless of which container
    // would have been picked. `outrig mcp` opts out via `require_agent =
    // false` -- it has no agent concept, so `agent_flag` and
    // `cfg.default_agent` are not consulted at all.
    let span = ProgressSpan::start("resolving agent and container");
    let (session_agent_name, agent_image) = if attach.is_some() {
        (None, None)
    } else if args.require_agent {
        let agent_name = args
            .agent_flag
            .or(cfg.default_agent.as_deref())
            .ok_or_else(|| {
                OutrigError::Configuration("no --agent and no default-agent configured".to_string())
            })?;
        let resolved = llm::resolve_agent_with_overrides(
            &cfg,
            agent_name,
            args.model_override,
            args.device_override,
        )?;
        (Some(resolved.agent_name.clone()), resolved.image.clone())
    } else {
        (None, None)
    };

    let (image_cfg_name, allow_raw_image) = match &attach {
        Some(attach) => (attach.image_cfg_name.clone(), true),
        None => match args.image_flag {
            Some(image) => (image.to_string(), true),
            None => {
                let image = agent_image
                    .as_deref()
                    .or(cfg.default_image.as_deref())
                    .ok_or_else(|| {
                        let msg = if args.require_agent {
                            "no --image, agent.image, or default-image configured"
                        } else {
                            "no --image or default-image configured"
                        };
                        OutrigError::Configuration(msg.to_string())
                    })?;
                (image.to_string(), false)
            }
        },
    };
    let (image_cfg, raw_local_image) =
        resolve_image_config(&cfg, &image_cfg_name, allow_raw_image)?;
    let declares_sidecars = !image_cfg.sidecars.is_empty()
        || image_cfg
            .mcp
            .values()
            .any(|spec| spec.sidecar().is_some() || spec.image().is_some());
    if attach.is_some() && declares_sidecars {
        return Err(OutrigError::Configuration(
            "sidecars cannot be used with `outrig mcp --attach`; an attached session \
             borrows its container and cannot own sidecar containers"
                .to_string(),
        )
        .into());
    }
    if let Some(attach) = &attach {
        span.done(format!(
            "attach target resolved: container {}, image-config {}",
            attach.container_name, image_cfg_name
        ));
    } else if let Some(agent) = &session_agent_name {
        span.done(format!(
            "agent/container resolved: agent {agent}, container {image_cfg_name}"
        ));
    } else {
        span.done(format!("container resolved: {image_cfg_name}"));
    }

    let image_tag = if let Some(attach) = &attach {
        let span = ProgressSpan::start(format!(
            "inspecting attached container {}",
            attach.container_name
        ));
        let inspect = Container::inspect_existing(&attach.container_name, None).await?;
        if !inspect.running {
            return Err(OutrigError::Configuration(format!(
                "attached container {:?} is not running",
                attach.container_name
            ))
            .into());
        }
        span.done(format!(
            "attached container ready: {}",
            attach.container_name
        ));
        inspect.image_tag
    } else {
        let span = ProgressSpan::start("computing image tag");
        let image_tag = if raw_local_image {
            ImageTag(image_cfg_name.clone())
        } else {
            image::compute_tag_for(&image_cfg_name, &image_cfg, &repo_root).await?
        };
        span.done(format!("image tag computed: {image_tag}"));
        image_tag
    };

    let sid = SessionId::new();
    let host_workspace = resolve_workspace_host(&repo_root, &cfg.workspace.host_path);
    let container_workspace = cfg.workspace.container_path.clone();
    let launch = ContainerLaunchSpec {
        workspace: Some(ContainerWorkspace {
            host: host_workspace.clone(),
            container: container_workspace.clone(),
            access: MountAccess::ReadWrite,
        }),
        mounts: cfg
            .workspace
            .mounts
            .iter()
            .map(|mount| ContainerMount {
                host: resolve_workspace_host(&repo_root, &mount.host_path),
                container: mount.container_path.clone(),
                access: mount.access,
            })
            .collect(),
        capabilities: ContainerCapabilities {
            profile: image_cfg.security.capability_profile,
            cap_drop: image_cfg.security.cap_drop.clone(),
            cap_add: image_cfg.security.cap_add.clone(),
        },
        devices: image_cfg.security.devices.clone(),
        no_new_privileges: image_cfg.security.no_new_privileges,
        labels: BTreeMap::from([(LABEL_SESSION.to_string(), sid.0.clone())]),
    };

    if let Some(p) = args.explicit_session_dir
        && !p.is_dir()
    {
        return Err(OutrigError::Configuration(format!(
            "--session-dir {} is not an existing directory (create it first or omit the flag)",
            p.display()
        ))
        .into());
    }

    let container_name = attach
        .as_ref()
        .map(|attach| attach.container_name.clone())
        .unwrap_or_else(|| format!("outrig-{sid}"));
    let mut session = Session {
        id: sid.clone(),
        started_at: SystemTime::now(),
        ended_at: None,
        container_name: container_name.clone(),
        sidecar_container_names: Vec::new(),
        image_tag: image_tag.to_string(),
        image_config_name: image_cfg_name.clone(),
        agent_name: session_agent_name,
        working_dir: repo_root.clone(),
        session_dir: PathBuf::new(), // set by `create` below
        exit_code: None,
        link_target: None,
    };
    let session_dir = store.create(&sid, args.explicit_session_dir, &mut session)?;
    let log_dir = session_dir.join("logs");
    if let Err(e) = tokio::fs::create_dir_all(&log_dir).await {
        let _ = store.finalize(&sid, SystemTime::now(), 1);
        return Err(e.into());
    }

    let transcript = if args.verbose > 0 {
        match Transcript::create(&log_dir.join("container.log"), true).await {
            Ok(t) => Some(t),
            Err(e) => {
                let _ = store.finalize(&sid, SystemTime::now(), 1);
                return Err(e.into());
            }
        }
    } else {
        None
    };

    let mut container = if let Some(attach) = &attach {
        let span = ProgressSpan::start(format!("attaching to container {}", attach.container_name));
        match Container::is_running(&attach.container_name).await {
            Ok(true) => {
                span.done(format!("attached to container: {}", attach.container_name));
                Container::attach(
                    attach.container_name.clone(),
                    image_tag.clone(),
                    Some((&host_workspace, &container_workspace)),
                    transcript.clone(),
                )
            }
            Ok(false) => {
                let _ = store.finalize(&sid, SystemTime::now(), 1);
                return Err(OutrigError::Configuration(format!(
                    "attached container {:?} is not running",
                    attach.container_name
                ))
                .into());
            }
            Err(e) => {
                let _ = store.finalize(&sid, SystemTime::now(), 1);
                return Err(e.into());
            }
        }
    } else {
        let span = ProgressSpan::start(format!("ensuring image {image_tag}"));
        let ensure = if raw_local_image {
            image::ensure_local_image(&image_tag, transcript.as_ref()).await
        } else {
            image::ensure_tagged_image_for(
                &image_cfg_name,
                &image_cfg,
                &repo_root,
                &image_tag,
                false,
                transcript.as_ref(),
            )
            .await
        };
        let image_outcome = match ensure {
            Ok(outcome) => outcome,
            Err(e) => {
                let _ = store.finalize(&sid, SystemTime::now(), 1);
                return Err(e.into());
            }
        };
        let cache_status = if raw_local_image {
            "local image"
        } else if image_outcome.cache_hit {
            "cache hit"
        } else {
            "built"
        };
        span.done(format!(
            "image ready: {} ({cache_status})",
            image_outcome.tag
        ));

        let span = ProgressSpan::start(format!("starting container {container_name}"));
        match Container::start_named(&image_tag, launch, container_name, transcript.clone()).await {
            Ok(container) => {
                span.done(format!("container ready: {}", container.name()));
                container
            }
            Err(e) => {
                let _ = store.finalize(&sid, SystemTime::now(), 1);
                return Err(e.into());
            }
        }
    };

    let span = ProgressSpan::start("bootstrapping container user");
    if let Err(e) = container.bootstrap_user().await {
        let _ = container.stop(STOP_GRACE).await;
        let _ = store.finalize(&sid, SystemTime::now(), 1);
        return Err(e.into());
    }
    span.done("container user ready");

    let mut containers = SessionContainers {
        sidecars: BTreeMap::new(),
        primary: container,
    };

    // Placement plan, sidecar starts, and network interception. Any error
    // propagated out of the phase aborts the whole container set.
    let phase = SidecarPhaseArgs {
        cfg: &cfg,
        repo_root: &repo_root,
        image_cfg: &image_cfg,
        image_tag: &image_tag,
        sid: &sid,
        host_workspace: &host_workspace,
        container_workspace: &container_workspace,
        log_dir: &log_dir,
        network_mode,
        start_sidecars: args.start_sidecars,
        cli_env: args.cli_env,
        transcript: transcript.as_ref(),
    };
    let (mcp_plan, network) = match setup_sidecars_and_network(phase, &mut containers).await {
        Ok(outcome) => outcome,
        Err(e) => {
            abort_containers(containers, &store, &sid).await;
            return Err(e);
        }
    };

    if !containers.sidecars.is_empty() {
        let names = containers.sidecar_names();
        if let Err(e) = store.set_sidecar_containers(&sid, &names) {
            abort_containers(containers, &store, &sid).await;
            return Err(e.into());
        }
        session.sidecar_container_names = names;
    }

    // The watcher exists to reap sidecars when the primary dies out from
    // under outrig; a session declaring no sidecars keeps today's behavior.
    // Declared-but-manual sidecars arm it too (possibly with an empty list)
    // so its primary-death token is already wired into the REPL select
    // before a `/sidecar add` starts anything.
    let watcher = if args.start_sidecars && !mcp_plan.sidecars.is_empty() {
        Some(SessionWatcher::spawn(
            containers.primary.name().to_string(),
            session.sidecar_container_names.clone(),
            sid.0.clone(),
            session.started_at,
        ))
    } else {
        None
    };

    Ok(SessionSetup {
        cfg,
        image_cfg_name,
        image_cfg,
        image_tag,
        containers,
        sid,
        session,
        session_dir,
        log_dir,
        store,
        repo_root,
        attached: attach.is_some(),
        network,
        mcp_plan,
        watcher,
    })
}

/// Borrowed inputs threaded from [`setup`] into
/// [`setup_sidecars_and_network`]; grouped so the phase reads as one call.
struct SidecarPhaseArgs<'a> {
    cfg: &'a Config,
    repo_root: &'a Path,
    image_cfg: &'a ImageConfig,
    image_tag: &'a ImageTag,
    sid: &'a SessionId,
    host_workspace: &'a Path,
    container_workspace: &'a Path,
    log_dir: &'a Path,
    network_mode: NetworkMode,
    start_sidecars: bool,
    cli_env: &'a CliEnvEntries,
    transcript: Option<&'a Transcript>,
}

impl<'a> SidecarPhaseArgs<'a> {
    fn start_ctx(&self) -> SidecarStartCtx<'a> {
        SidecarStartCtx {
            cfg: self.cfg,
            repo_root: self.repo_root,
            sid: self.sid.as_str(),
            host_workspace: self.host_workspace,
            container_workspace: self.container_workspace,
            transcript: self.transcript,
        }
    }
}

/// Inputs shared by session-start sidecar launches and mid-session
/// (`/sidecar add`) ones -- the subset of [`SidecarPhaseArgs`] the start
/// sequence actually reads.
pub(crate) struct SidecarStartCtx<'a> {
    pub cfg: &'a Config,
    pub repo_root: &'a Path,
    /// Container-name suffix: sidecars are named `outrig-<sid>-<sc>`.
    pub sid: &'a str,
    pub host_workspace: &'a Path,
    pub container_workspace: &'a Path,
    pub transcript: Option<&'a Transcript>,
}

/// Full start sequence for one config-declared exec-stdio sidecar: ensure
/// its image (`--image` semantics) -> start the container (labels, keep-id)
/// -> bootstrap the user when identity matters. Session start uses the same
/// pieces inline (it interleaves label merges between ensure and start);
/// `/sidecar add` calls this composition mid-session.
pub(crate) async fn launch_declared_sidecar(
    ctx: &SidecarStartCtx<'_>,
    plan: &SessionMcpPlan,
    sc: &SidecarPlan,
) -> Result<Container> {
    let tag = ensure_sidecar_image(ctx.cfg, ctx.repo_root, &sc.image, ctx.transcript).await?;
    start_one_sidecar(ctx, &tag, sc, plan.sidecar_needs_bootstrap(sc)).await
}

/// Build the placement plan (config + primary and sidecar label merges),
/// start `start = "auto"` sidecars, and attach the network interceptor to
/// every running container.
///
/// Failure routing: image-ensure / start / bootstrap / interceptor-attach
/// failures on a sidecar follow its `on-failure` (`warn` logs, drops the
/// sidecar, and continues; `abort` propagates). Label-merge collisions and
/// malformed labels are startup errors regardless of `on-failure`, as are
/// all primary-container failures.
async fn setup_sidecars_and_network(
    args: SidecarPhaseArgs<'_>,
    containers: &mut SessionContainers,
) -> Result<(SessionMcpPlan, Option<NetworkInterceptor>)> {
    let mut plan = sidecar::plan_from_config(args.image_cfg);
    let image_mcp = embedded::read_embedded_mcp(args.image_tag, args.transcript).await?;
    sidecar::merge_primary_labels(&mut plan, image_mcp);

    // Phase A -- ensure each distinct sidecar image (and read its labels when a
    // non-anonymous sidecar needs them) concurrently. Sidecars are independent,
    // so their podman-heavy ensure/inspect work overlaps; the results are
    // consumed below in deterministic name order.
    let resolutions =
        resolve_sidecar_images(&plan, args.cfg, args.repo_root, args.transcript).await;

    // Phase B -- merge sidecar labels and decide which sidecars start, serially
    // in name order. Keeping this ordered preserves the deterministic
    // label-collision errors that the concurrent Phase A must not perturb.
    let mut to_start: Vec<(String, ImageTag, SidecarPlan)> = Vec::new();
    for name in plan.sidecars.keys().cloned().collect::<Vec<_>>() {
        let sc = plan.sidecars[&name].clone();
        let resolution = &resolutions[&sc.image];

        let tag = match &resolution.tag {
            Ok(tag) => tag.clone(),
            Err(message) => {
                warn_or_bail(&plan, &sc, shared_image_error(message))?;
                continue;
            }
        };

        if !sc.anonymous {
            let label_mcp = match resolution
                .labels
                .as_ref()
                .expect("non-anonymous sidecar image is label-read in Phase A")
            {
                Ok(label_mcp) => label_mcp.clone(),
                Err(message) => return Err(shared_image_error(message)),
            };
            sidecar::merge_sidecar_labels(&mut plan, &name, label_mcp)?;
        }

        if !args.start_sidecars || sc.start != SidecarStart::Auto {
            if args.start_sidecars && plan.servers_in(&name).next().is_some() {
                eprintln!(
                    "[outrig] sidecar {name} is start = \"manual\"; skipping its MCP servers"
                );
            }
            continue;
        }

        to_start.push((name, tag, sc));
    }

    // Phase C -- start the auto sidecars concurrently, inserting in name order.
    start_auto_sidecars(&plan, &args, to_start, containers).await?;

    let network = match args.network_mode {
        NetworkMode::Default => None,
        NetworkMode::Audit => Some(attach_interceptor("audit", &plan, containers, &args).await?),
        NetworkMode::Filter => Some(attach_interceptor("filter", &plan, containers, &args).await?),
    };

    Ok((plan, network))
}

/// One distinct sidecar image resolved once and shared by every sidecar that
/// references it (Phase A of [`setup_sidecars_and_network`]). `tag` is the
/// image-ensure result; `labels` is the `org.outrig.mcp` read, present only
/// when at least one *non-anonymous* sidecar uses the image -- anonymous
/// sidecars never read labels, so an anonymous-only image is never inspected
/// (inspecting it could newly fail a session on a malformed label). A failure
/// is kept as its `Display` text -- one image feeds many sidecars but
/// [`CliError`] is not `Clone` -- and [`shared_image_error`] re-wraps it
/// verbatim per consumer.
struct ImageResolution {
    tag: std::result::Result<ImageTag, String>,
    labels: Option<std::result::Result<BTreeMap<String, McpServerSpec>, String>>,
}

/// Ensure every distinct sidecar image (and read its labels when needed)
/// concurrently, deduped by image ref so two sidecars sharing an image run the
/// tag-compute + ensure + label-inspect once. `join_all` runs the futures on
/// the current task, so the borrowed inputs need no `'static` and nothing is
/// cloned beyond the image key.
async fn resolve_sidecar_images(
    plan: &SessionMcpPlan,
    cfg: &Config,
    repo_root: &Path,
    transcript: Option<&Transcript>,
) -> HashMap<String, ImageResolution> {
    // Distinct images, each flagged for a label read iff a non-anonymous
    // sidecar uses it (anonymous-only images are never inspected).
    let mut images: BTreeMap<&str, bool> = BTreeMap::new();
    for sc in plan.sidecars.values() {
        *images.entry(sc.image.as_str()).or_default() |= !sc.anonymous;
    }

    futures_util::future::join_all(images.into_iter().map(|(image, read_labels)| async move {
        let resolution = match ensure_sidecar_image(cfg, repo_root, image, transcript).await {
            Ok(tag) => {
                let labels = if read_labels {
                    Some(
                        embedded::read_embedded_mcp(&tag, transcript)
                            .await
                            .map_err(|e| e.to_string()),
                    )
                } else {
                    None
                };
                ImageResolution {
                    tag: Ok(tag),
                    labels,
                }
            }
            Err(e) => ImageResolution {
                tag: Err(e.to_string()),
                labels: None,
            },
        };
        (image.to_string(), resolution)
    }))
    .await
    .into_iter()
    .collect()
}

/// Start the `start = "auto"` sidecars concurrently, then insert them into
/// `containers` in name order. The container starts (and any bootstrap) are
/// independent; only the ordered insert and `warn`/`abort` routing run after
/// the join, so a `warn` sidecar drops exactly as it did serially.
async fn start_auto_sidecars(
    plan: &SessionMcpPlan,
    args: &SidecarPhaseArgs<'_>,
    to_start: Vec<(String, ImageTag, SidecarPlan)>,
    containers: &mut SessionContainers,
) -> Result<()> {
    let started =
        futures_util::future::join_all(to_start.into_iter().map(|(name, tag, sc)| async move {
            let result = match plan.entrypoint_server_in(&sc) {
                Some((server_name, placed)) => {
                    create_one_entrypoint_sidecar(args, &tag, &sc, server_name, &placed.spec).await
                }
                None => {
                    let needs_bootstrap = plan.sidecar_needs_bootstrap(&sc);
                    start_one_sidecar(&args.start_ctx(), &tag, &sc, needs_bootstrap).await
                }
            };
            (name, sc, result)
        }))
        .await;

    for (name, sc, result) in started {
        match result {
            Ok(container) => {
                containers.sidecars.insert(name, container);
            }
            Err(e) => warn_or_bail(plan, &sc, e)?,
        }
    }
    Ok(())
}

/// Rebuild an owned error from an [`ImageResolution`]'s shared failure text.
/// Only the error's `Display` reaches the user (`app.rs` prints `error: {e}` and
/// always exits 1), so preserving the exact message -- not the original variant
/// -- is what keeps this observably identical to the serial path. `io::Error`
/// displays its payload verbatim (`OutrigError::Io` and `CliError::Outrig` are
/// both transparent); `OutrigError::Configuration` would instead prepend
/// "configuration: ". Revisit if exit codes ever become variant-dependent.
fn shared_image_error(message: &str) -> CliError {
    CliError::from(std::io::Error::other(message.to_string()))
}

/// Start the network interceptor on the primary and attach every running
/// sidecar. A sidecar attach failure follows its `on-failure`; `warn` stops
/// and drops that sidecar.
async fn attach_interceptor(
    mode_word: &str,
    plan: &SessionMcpPlan,
    containers: &mut SessionContainers,
    args: &SidecarPhaseArgs<'_>,
) -> Result<NetworkInterceptor> {
    let span = ProgressSpan::start(format!("starting network {mode_word} interceptor"));
    let mut interceptor = if mode_word == "audit" {
        NetworkInterceptor::start(&containers.primary, args.log_dir, args.sid.as_str()).await?
    } else {
        NetworkInterceptor::start_with_policy(
            &containers.primary,
            args.log_dir,
            args.sid.as_str(),
            args.cfg.network.policy(),
        )
        .await?
    };
    for name in containers.sidecars.keys().cloned().collect::<Vec<_>>() {
        // Entrypoint-stdio sidecars are created+initialized but not yet
        // started here; attaching now -- before `podman start` in
        // connect_mcp_clients -- is what puts policy ahead of the
        // entrypoint's first packet.
        if let Err(e) = interceptor.attach(&containers.sidecars[&name]).await {
            warn_or_bail(plan, &plan.sidecars[&name], e.into())?;
            if let Some(container) = containers.sidecars.remove(&name) {
                let _ = container.stop(STOP_GRACE).await;
            }
        }
    }
    span.done(format!("network {mode_word} interceptor ready"));
    Ok(interceptor)
}

/// `warn` on-failure: log the failure and let the caller drop the sidecar.
/// `abort`: propagate.
fn warn_or_bail(plan: &SessionMcpPlan, sc: &SidecarPlan, e: crate::error::CliError) -> Result<()> {
    if sc.on_failure != SidecarOnFailure::Warn {
        return Err(e);
    }
    let server_count = plan.servers_in(&sc.name).count();
    let server_word = plural(server_count, "server", "servers");
    eprintln!(
        "[outrig] warning: sidecar {}: {e}; skipping the sidecar and its \
         {server_count} MCP {server_word}",
        sc.name
    );
    Ok(())
}

/// Resolve and ensure a sidecar's image with `--image`-identical semantics:
/// an `[images.<name>]` config name builds through the content-hash cache; an
/// unmatched name is a raw podman ref that must be present locally.
async fn ensure_sidecar_image(
    cfg: &Config,
    repo_root: &Path,
    image_name: &str,
    transcript: Option<&Transcript>,
) -> Result<ImageTag> {
    let (image_cfg, raw_local) = resolve_image_config(cfg, image_name, true)?;
    if raw_local {
        let tag = ImageTag(image_name.to_string());
        image::ensure_local_image(&tag, transcript).await?;
        Ok(tag)
    } else {
        let tag = image::compute_tag_for(image_name, &image_cfg, repo_root).await?;
        image::ensure_tagged_image_for(image_name, &image_cfg, repo_root, &tag, false, transcript)
            .await?;
        Ok(tag)
    }
}

/// The launch inputs every sidecar container shares, whichever path starts
/// it: session + sidecar labels and the block's security policy -- capability
/// profile, device passthrough, and privilege escalation alike. Workspace
/// and mounts stay empty; `start_one_sidecar` fills them in (the entrypoint
/// form cannot declare either).
fn sidecar_launch_base(ctx: &SidecarStartCtx<'_>, sc: &SidecarPlan) -> ContainerLaunchSpec {
    ContainerLaunchSpec {
        workspace: None,
        mounts: Vec::new(),
        capabilities: ContainerCapabilities {
            profile: sc.security.capability_profile,
            cap_drop: sc.security.cap_drop.clone(),
            cap_add: sc.security.cap_add.clone(),
        },
        devices: sc.security.devices.clone(),
        no_new_privileges: sc.security.no_new_privileges,
        labels: BTreeMap::from([
            (LABEL_SESSION.to_string(), ctx.sid.to_string()),
            (LABEL_SIDECAR.to_string(), sc.name.clone()),
        ]),
    }
}

fn sidecar_container_name(ctx: &SidecarStartCtx<'_>, sc: &SidecarPlan) -> String {
    outrig::container::sidecar_container_name(ctx.sid, &sc.name)
}

/// Start one sidecar container (`outrig-<sid>-<sc>`, session + sidecar
/// labels, keep-id) and bootstrap its user when identity matters.
async fn start_one_sidecar(
    ctx: &SidecarStartCtx<'_>,
    tag: &ImageTag,
    sc: &SidecarPlan,
    needs_bootstrap: bool,
) -> Result<Container> {
    let mut launch = sidecar_launch_base(ctx, sc);
    launch.workspace = sc
        .workspace
        .mount_access()
        .map(|access| ContainerWorkspace {
            host: ctx.host_workspace.to_path_buf(),
            container: ctx.container_workspace.to_path_buf(),
            access,
        });
    launch.mounts = sc
        .mounts
        .iter()
        .map(|mount| ContainerMount {
            host: resolve_workspace_host(ctx.repo_root, &mount.host_path),
            container: mount.container_path.clone(),
            access: mount.access,
        })
        .collect();

    let container_name = sidecar_container_name(ctx, sc);
    let span = ProgressSpan::start(format!("starting sidecar {}", sc.name));
    let mut container =
        Container::start_named(tag, launch, container_name, ctx.transcript.cloned()).await?;
    if needs_bootstrap && let Err(e) = container.bootstrap_user().await {
        let _ = container.stop(STOP_GRACE).await;
        return Err(e.into());
    }
    span.done(format!("sidecar {} ready: {}", sc.name, container.name()));
    Ok(container)
}

/// Create + initialize one entrypoint-stdio sidecar without executing its
/// ENTRYPOINT: the interceptor attaches to the initialized netns first, and
/// `podman start --attach --interactive` runs the server later in
/// [`connect_mcp_clients`]. The server's env (config + CLI `--env` overlay)
/// is resolved here and baked in via `podman create --env`; when network
/// interception is on, the loopback resolver rides in via `--dns` because
/// the exec-based resolv.conf install needs a running container.
async fn create_one_entrypoint_sidecar(
    args: &SidecarPhaseArgs<'_>,
    tag: &ImageTag,
    sc: &SidecarPlan,
    server_name: &str,
    spec: &outrig::config::McpServerSpec,
) -> Result<Container> {
    let ctx = args.start_ctx();
    let launch = sidecar_launch_base(&ctx, sc);
    let (_, env_spec) = spec.normalize();
    let env =
        outrig::resolve_mcp_env(server_name, env_spec, &args.cli_env.for_server(server_name))?;
    let intercept_dns = args.network_mode != NetworkMode::Default;

    let container_name = sidecar_container_name(&ctx, sc);
    let span = ProgressSpan::start(format!("creating sidecar {} (entrypoint held)", sc.name));
    let container = Container::create_initialized(
        tag,
        launch,
        container_name,
        args.transcript.cloned(),
        &env,
        intercept_dns,
    )
    .await?;
    span.done(format!("sidecar {} created: {}", sc.name, container.name()));
    Ok(container)
}

/// Stop every session container (sidecars before the primary) and finalize
/// the session row with a failure exit. Setup's bail-out path.
async fn abort_containers(containers: SessionContainers, store: &SessionStore, sid: &SessionId) {
    let SessionContainers { sidecars, primary } = containers;
    for (_, container) in sidecars {
        let _ = container.stop(STOP_GRACE).await;
    }
    let _ = primary.stop(STOP_GRACE).await;
    let _ = store.finalize(sid, SystemTime::now(), 1);
}

/// Resolve an image-config name to its [`ImageConfig`] and whether it is a
/// *raw local image* -- a ref used verbatim that is never built or pulled.
///
/// A name matching a `[images.<name>]` block uses that config (not raw).
/// Otherwise, when `allow_raw_image` is set and the name is non-empty, a
/// minimal image-only config naming the ref itself is synthesized (raw).
/// Otherwise it is an error.
fn resolve_image_config(
    cfg: &Config,
    image_cfg_name: &str,
    allow_raw_image: bool,
) -> Result<(ImageConfig, bool)> {
    if let Some(image_cfg) = cfg.images.get(image_cfg_name) {
        return Ok((image_cfg.clone(), false));
    }

    if allow_raw_image && !image_cfg_name.trim().is_empty() {
        let image_cfg = ImageConfig {
            image_name: Some(image_cfg_name.to_string()),
            dockerfile: None,
            context: None,
            build_args: BTreeMap::new(),
            security: Default::default(),
            mcp: BTreeMap::new(),
            sidecars: BTreeMap::new(),
        };
        return Ok((image_cfg, true));
    }

    Err(OutrigError::Configuration(format!(
        "image-config {image_cfg_name:?} does not match any [images.<name>]"
    ))
    .into())
}

fn resolve_attach_target(
    raw: &str,
    image_flag: Option<&str>,
    store: &SessionStore,
) -> Result<AttachResolution> {
    let sid = SessionId::from(raw.to_string());
    let session_entry = store.symlink_path(&sid);
    match fs::symlink_metadata(&session_entry) {
        Ok(_) => {
            let (_, session) = store.get_by_id(&sid)?;
            Ok(AttachResolution {
                container_name: session.container_name,
                image_cfg_name: image_flag.unwrap_or(&session.image_config_name).to_string(),
            })
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let image_cfg_name = image_flag.ok_or_else(|| {
                OutrigError::Configuration(format!(
                    "--attach {raw:?} did not match a session; pass --image <name> \
                     to treat it as a podman container name"
                ))
            })?;
            Ok(AttachResolution {
                container_name: raw.to_string(),
                image_cfg_name: image_cfg_name.to_string(),
            })
        }
        Err(e) => Err(e.into()),
    }
}

/// Spawn one [`McpClient`] per plan server whose container is running, in
/// key-sorted (`BTreeMap`) iteration order, each against the container its
/// placement names. Servers in sidecars that never started (manual, warn'd,
/// or reaped) are skipped. `cli_env` provides any `--env` overlay entries to
/// merge per server. Adapter construction is the caller's job because only
/// the REPL path consumes adapters.
///
/// A connect failure on a primary-placed server -- or any server in an
/// `on-failure = "abort"` sidecar -- propagates. In a `warn` sidecar it
/// drops the whole sidecar: already-connected clients from that sidecar are
/// shut down, its container is stopped and removed from `containers`, and
/// its remaining servers are skipped.
pub async fn connect_mcp_clients(
    containers: &mut SessionContainers,
    mcp_plan: &SessionMcpPlan,
    log_dir: &Path,
    cli_env: &CliEnvEntries,
) -> Result<Vec<Arc<McpClient>>> {
    // Tagged with the hosting sidecar so a warn-path drop can find and shut
    // down the sidecar's already-connected clients.
    let mut connected: Vec<(Option<String>, Arc<McpClient>)> =
        Vec::with_capacity(mcp_plan.servers.len());

    for (mcp_name, placed) in &mcp_plan.servers {
        let sidecar_name = match &placed.placement {
            Placement::Primary => None,
            Placement::Sidecar(sc) => Some(sc.clone()),
        };
        let Some(container) = containers.container_for(&placed.placement) else {
            // The sidecar never started or was dropped earlier in this loop.
            continue;
        };

        let span = ProgressSpan::start(format!("MCP {mcp_name}: initializing"));
        let result = if placed.spec.is_entrypoint_stdio() {
            // entrypoint-stdio: the container was created+initialized with
            // env baked in and the interceptor already attached; `podman
            // start --attach` here is what finally runs the entrypoint.
            McpClient::connect_via_podman_start(container, mcp_name, placed.source, log_dir).await
        } else {
            let extra_env = cli_env.for_server(mcp_name);
            McpClient::connect_via_podman_exec_with_source(
                container,
                &placed.spec,
                mcp_name,
                placed.source,
                log_dir,
                &extra_env,
            )
            .await
        };
        match result {
            Ok(client) => {
                span.done(format!("MCP {mcp_name}: initialized"));
                connected.push((sidecar_name, Arc::new(client)));
            }
            Err(e) => {
                let warn = sidecar_name
                    .as_deref()
                    .and_then(|sc| mcp_plan.sidecars.get(sc))
                    .is_some_and(|sc| sc.on_failure == SidecarOnFailure::Warn);
                if !warn {
                    return Err(e.into());
                }
                let sc = sidecar_name.expect("warn on-failure implies a sidecar placement");
                eprintln!(
                    "[outrig] warning: sidecar {sc}: MCP server {mcp_name} failed to \
                     start: {e}; skipping the sidecar and its servers"
                );
                let mut kept = Vec::with_capacity(connected.len());
                for (tag, arc) in connected.drain(..) {
                    if tag.as_deref() == Some(sc.as_str()) {
                        if let Ok(client) = Arc::try_unwrap(arc) {
                            let _ = client.shutdown().await;
                        }
                    } else {
                        kept.push((tag, arc));
                    }
                }
                connected = kept;
                if let Some(container) = containers.sidecars.remove(&sc) {
                    let _ = container.stop(STOP_GRACE).await;
                }
            }
        }
    }
    Ok(connected.into_iter().map(|(_, arc)| arc).collect())
}

/// Cleanup tail. Order: watcher disarm (so outrig's own stops are never
/// mistaken for external death) -> MCP shutdowns (so their `podman exec`
/// pipes drain before the containers go away) -> network interceptor
/// shutdown (detaches every container) -> sidecar stops -> primary stop ->
/// session finalize. Each step's failure is logged but never propagated;
/// the caller's outcome owns the process exit code.
///
/// Callers must drop any `Arc<McpClient>` clones (e.g. tool adapters) and
/// the agent before invoking this -- otherwise [`Arc::try_unwrap`] returns
/// `Err` and the explicit `shutdown` is skipped in favor of `Drop`.
pub async fn teardown(
    runtime: SessionRuntime,
    store: &SessionStore,
    sid: &SessionId,
    final_exit: i32,
) {
    let SessionRuntime {
        watcher,
        mcp_arcs,
        network,
        containers,
    } = runtime;
    if let Some(watcher) = watcher {
        watcher.shutdown();
    }
    for arc in mcp_arcs {
        match Arc::try_unwrap(arc) {
            Ok(client) => {
                if let Err(e) = client.shutdown().await {
                    tracing::warn!(
                        target: "outrig::cli::session_setup",
                        "mcp shutdown failed: {e}"
                    );
                }
            }
            Err(_) => {
                tracing::warn!(
                    target: "outrig::cli::session_setup",
                    "mcp client still has outstanding refs at cleanup; relying on Drop"
                );
            }
        }
    }
    if let Some(network) = network {
        network.shutdown().await;
    }
    let SessionContainers { sidecars, primary } = containers;
    for (name, container) in sidecars {
        // A watcher-reaped sidecar is already gone; podman's "no such
        // container" lands here as a logged, non-fatal error.
        if let Err(e) = container.stop(STOP_GRACE).await {
            tracing::warn!(
                target: "outrig::cli::session_setup",
                "sidecar {name} stop failed: {e}"
            );
        }
    }
    if let Err(e) = primary.stop(STOP_GRACE).await {
        tracing::warn!(
            target: "outrig::cli::session_setup",
            "container stop failed: {e}"
        );
    }
    if let Err(e) = store.finalize(sid, SystemTime::now(), final_exit) {
        tracing::warn!(
            target: "outrig::cli::session_setup",
            "session finalize failed: {e}"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_image_error_preserves_message_without_a_configuration_prefix() {
        // The serial path surfaced an ensure/label failure verbatim; deduped
        // Phase A keeps only its `Display` text, so re-wrapping must not add a
        // variant prefix (`OutrigError::Configuration` would prepend
        // "configuration: ").
        let message = "process `buildah` exited with code 1";
        assert_eq!(shared_image_error(message).to_string(), message);
    }

    fn config_image(image_ref: &str) -> ImageConfig {
        ImageConfig {
            image_name: Some(image_ref.to_string()),
            dockerfile: None,
            context: None,
            build_args: BTreeMap::new(),
            security: Default::default(),
            mcp: BTreeMap::new(),
            sidecars: BTreeMap::new(),
        }
    }

    #[test]
    fn image_resolution_prefers_config_entry_over_raw_fallback() {
        let mut cfg = Config::default();
        cfg.images.insert(
            "outrig-standard:53e082e721df8ecc".to_string(),
            config_image("configured"),
        );

        let (image_cfg, raw_local) =
            resolve_image_config(&cfg, "outrig-standard:53e082e721df8ecc", true).unwrap();

        assert!(!raw_local);
        assert_eq!(image_cfg.image_name.as_deref(), Some("configured"));
    }

    #[test]
    fn image_resolution_allows_raw_fallback_for_explicit_values() {
        let cfg = Config::default();

        let (image_cfg, raw_local) =
            resolve_image_config(&cfg, "outrig-standard:53e082e721df8ecc", true).unwrap();

        assert!(raw_local);
        assert_eq!(
            image_cfg.image_name.as_deref(),
            Some("outrig-standard:53e082e721df8ecc")
        );
        assert!(image_cfg.mcp.is_empty());
    }

    #[test]
    fn image_resolution_rejects_config_only_missing_values() {
        let cfg = Config::default();

        let err = resolve_image_config(&cfg, "missing", false).unwrap_err();

        assert!(
            err.to_string()
                .contains("image-config \"missing\" does not match any [images.<name>]"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn image_resolution_rejects_empty_raw_value() {
        let cfg = Config::default();

        let err = resolve_image_config(&cfg, "", true).unwrap_err();

        assert!(
            err.to_string()
                .contains("image-config \"\" does not match any [images.<name>]"),
            "unexpected error: {err}"
        );
    }
}
