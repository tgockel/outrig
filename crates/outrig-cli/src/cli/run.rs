//! `outrig run` orchestrator: wire every subsystem into a working REPL.
//!
//! [`execute`] walks through the order documented in `doc/usage/run.md`:
//! delegate to [`session_setup::setup`] for config-load through container
//! bootstrap + session row + log dir, then resolve the agent, connect every
//! MCP client, build adapters, build the Rig agent, print the banner, and
//! hand off to the REPL. On exit (clean or error)
//! [`session_setup::teardown`] runs MCP shutdowns *before* stopping the
//! container -- the MCP children are `podman exec` processes whose pipes
//! ride through the container; tearing the container down first races
//! them.

use std::cell::RefCell;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use clap::{ArgAction, Parser};
use rig::completion::Message;

use crate::builtin_tool;
use crate::cli::env_arg::CliEnvEntries;
use crate::cli::session_setup::{
    self, ProgressSpan, STOP_GRACE, SessionRuntime, SessionSetup, SessionSetupArgs,
    SidecarStartCtx, plural,
};
use crate::cli::volume_arg::{CliVolume, parse_volume};
use crate::error::{OutrigError, Result};
use crate::llm;
use crate::paths::model_cache_root;
use crate::repl::{HelpEntry, Repl};
use crate::rig_tool::McpToolAdapter;
use crate::session::{SessionId, SessionStore};
use crate::session_tool::{self, SessionTool};
use crate::subagent::{SubagentContext, SubagentRegistry};
use outrig::McpClient;
use outrig::config::{
    Config, MistralrsDeviceSpec, NetworkMode, SidecarStart, TOOL_CALL_MAX_LIMIT,
    TOOL_RESULT_MAX_CEILING_BYTES, TOOL_RESULT_MAX_FLOOR_BYTES,
};
use outrig::container::Container;
use outrig::container::sidecar::{SessionMcpPlan, SidecarPlan};
use outrig::image::ImageTag;
use rig::tool::ToolDyn;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Parser)]
pub struct RunArgs {
    /// Pick an `[agents.<name>]` block. Defaults to `default-agent` from
    /// config; with neither, the session runs with no agent -- no preamble,
    /// and the image comes from `--image` or `default-image`.
    #[arg(long, value_name = "NAME")]
    pub agent: Option<String>,

    /// Pick a `[models.<name>]` block for this run. Overrides the agent's
    /// `model` and the top-level `default-model`.
    #[arg(long, value_name = "NAME")]
    pub model: Option<String>,

    /// Pick a `[images.<name>]` block. Overrides the agent's `image` and the
    /// top-level `default-image`. An explicit value that doesn't match config
    /// is used as a local Podman image ref, run without pulling.
    #[arg(long, value_name = "NAME-OR-LOCAL-REF")]
    pub image: Option<String>,

    /// Write the session into an explicit, already-existing directory. The
    /// session root gets a symlink at `<root>/<sid>` pointing at this path.
    #[arg(long = "session-dir", value_name = "PATH")]
    pub session_dir: Option<PathBuf>,

    /// Override the per-turn tool-call max for this run.
    #[arg(long = "max-tool-calls", value_name = "N", value_parser = parse_tool_call_max)]
    pub max_tool_calls: Option<u32>,

    /// Override the per-result truncation max for this run.
    #[arg(long = "max-tool-result-bytes", value_name = "N", value_parser = parse_tool_result_max)]
    pub max_tool_result_bytes: Option<u32>,

    /// Add or override env vars for MCP servers. Repeatable.
    /// `KEY=VALUE` applies to every server; `SERVER:KEY=VALUE` targets one.
    #[arg(long = "env", value_name = "KEY=VALUE", action = ArgAction::Append)]
    pub env: Vec<String>,

    /// Override network monitoring for this session.
    #[arg(long = "network", value_name = "MODE", value_parser = parse_network_mode)]
    pub network: Option<NetworkMode>,

    /// Override the mistralrs model device for this run.
    #[arg(long = "device", value_name = "DEVICE", value_parser = parse_mistralrs_device)]
    pub device: Option<MistralrsDeviceSpec>,

    /// Mount an extra host directory into the container. Repeatable. Format
    /// `HOST:CONTAINER[:ro|rw]` (default read-only; the host dir must exist).
    #[arg(long = "volume", value_name = "HOST:CONTAINER[:ro|rw]", action = ArgAction::Append, value_parser = parse_volume)]
    pub volume: Vec<CliVolume>,
}

/// Run one `outrig run` invocation end-to-end. Returns the process exit code.
pub async fn execute(
    repo_cfg_path: &Path,
    global_cfg_path: &Path,
    session_root_flag: Option<&Path>,
    args: &RunArgs,
    verbose: u8,
) -> Result<i32> {
    let cli_env =
        CliEnvEntries::parse(&args.env).map_err(|e| OutrigError::Configuration(e.to_string()))?;

    let setup = session_setup::setup(SessionSetupArgs {
        repo_cfg_path,
        global_cfg_path,
        session_root_flag,
        image_flag: args.image.as_deref(),
        attach_target: None,
        agent_flag: args.agent.as_deref(),
        model_override: args.model.as_deref(),
        llm_session: true,
        explicit_session_dir: args.session_dir.as_deref(),
        network_mode_override: args.network,
        device_override: args.device,
        volumes: &args.volume,
        start_sidecars: true,
        cli_env: &cli_env,
        verbose,
    })
    .await?;

    // `None` when neither `--agent` nor `default-agent` named one: the session
    // runs with no preamble and no agent-level knobs.
    let agent_name = setup.session.agent_name.clone();
    let SessionSetup {
        cfg,
        image_cfg_name,
        image_tag,
        containers,
        sid,
        log_dir,
        store,
        repo_root,
        network,
        mcp_plan,
        watcher,
        attached: _,
        session: _,
    } = setup;
    let mut runtime = SessionRuntime::new(watcher, network, containers);
    // Shared from here on: the subagent registry keeps a handle so a launch can
    // re-resolve the agent against another `[models.<name>]`, against the same
    // merged config the session resolved from.
    let cfg = Arc::new(cfg);
    let cache_root = model_cache_root(cfg.model_cache_root.as_deref());

    // Validate per-server env entries against the full merged plan (a
    // skipped sidecar's servers are still declared names).
    for name in cli_env.per_server_names() {
        if !mcp_plan.servers.contains_key(name) {
            return Err(OutrigError::Configuration(format!(
                "--env {name}:...: image '{}' has no MCP server '{name}'",
                image_cfg_name
            ))
            .into());
        }
    }

    let outcome: Result<i32> = run_inner(RunInnerArgs {
        cfg: Arc::clone(&cfg),
        agent_name: agent_name.as_deref(),
        image_cfg_name: &image_cfg_name,
        image_tag: &image_tag,
        log_dir: &log_dir,
        sid: &sid,
        repo_root: &repo_root,
        cache_root: &cache_root,
        max_tool_calls: args.max_tool_calls,
        max_tool_result_bytes: args.max_tool_result_bytes,
        model_override: args.model.as_deref(),
        device_override: args.device,
        mcp_plan: &mcp_plan,
        cli_env: &cli_env,
        runtime: &mut runtime,
        store: &store,
    })
    .await;

    let final_exit = outcome.as_ref().copied().unwrap_or(1);
    session_setup::teardown(runtime, &store, &sid, final_exit).await;
    crate::cli::watcher::exit_if_monitor_stopped(&outcome, final_exit);
    outcome
}

fn parse_network_mode(s: &str) -> std::result::Result<NetworkMode, String> {
    s.parse()
}

fn parse_mistralrs_device(s: &str) -> std::result::Result<MistralrsDeviceSpec, String> {
    s.parse::<MistralrsDeviceSpec>().map_err(|e| e.to_string())
}

/// Inputs to [`run_inner`], grouped like [`SessionSetupArgs`]: the resolved
/// session pieces plus the [`SessionRuntime`] that `/sidecar add` grows
/// mid-session.
struct RunInnerArgs<'a> {
    /// Shared rather than borrowed: the subagent registry keeps a handle so a
    /// launch can re-resolve the agent against another `[models.<name>]`.
    cfg: Arc<Config>,
    /// `None` for an agentless session; see [`SessionSetupArgs::llm_session`].
    agent_name: Option<&'a str>,
    image_cfg_name: &'a str,
    image_tag: &'a ImageTag,
    log_dir: &'a Path,
    sid: &'a SessionId,
    repo_root: &'a Path,
    cache_root: &'a Path,
    max_tool_calls: Option<u32>,
    max_tool_result_bytes: Option<u32>,
    model_override: Option<&'a str>,
    device_override: Option<MistralrsDeviceSpec>,
    mcp_plan: &'a SessionMcpPlan,
    cli_env: &'a CliEnvEntries,
    runtime: &'a mut SessionRuntime,
    store: &'a SessionStore,
}

async fn run_inner(args: RunInnerArgs<'_>) -> Result<i32> {
    let RunInnerArgs {
        cfg,
        agent_name,
        image_cfg_name,
        image_tag,
        log_dir,
        sid,
        repo_root,
        cache_root,
        max_tool_calls,
        max_tool_result_bytes,
        model_override,
        device_override,
        mcp_plan,
        cli_env,
        runtime,
        store,
    } = args;

    // Grab the death token before the watcher disappears into the REPL's
    // shared state; it pairs with the REPL select below.
    let primary_died: Option<CancellationToken> =
        runtime.watcher.as_ref().map(|w| w.primary_died());

    // `setup` already validated presence and used the resolved `.image`
    // for the image fallback. We re-resolve here for `build_agent` +
    // banner; cheap (config table lookups, no I/O).
    // Wrapped by the caller so the subagent registry can hold it and
    // re-resolve a launch against a different model, against the same merged
    // config the session resolved from.
    let mut resolved =
        llm::resolve_agent_with_overrides(&cfg, agent_name, model_override, device_override)?;
    apply_tool_call_max_override(&mut resolved, max_tool_calls);
    apply_tool_result_max_override(&mut resolved, max_tool_result_bytes);

    let connected =
        session_setup::connect_mcp_clients(&mut runtime.containers, mcp_plan, log_dir, cli_env)
            .await?;
    runtime.mcp_arcs.extend(connected);

    let mut all_tools: Vec<SessionTool> = Vec::new();
    let mut per_server_counts: Vec<(String, usize)> = Vec::new();
    for arc in runtime.mcp_arcs.iter() {
        let span = ProgressSpan::start(format!("MCP {}: listing tools", arc.name()));
        let adapters =
            McpToolAdapter::from_client_tools(arc.clone(), resolved.tool_result_max_bytes).await?;
        let tool_count = adapters.len();
        let tool_word = plural(tool_count, "tool", "tools");
        span.done(format!(
            "MCP {}: tools ready: {tool_count} {tool_word}",
            arc.name()
        ));
        per_server_counts.push((arc.name().to_string(), tool_count));
        all_tools.extend(session_tool::erase(adapters));
    }

    #[cfg(feature = "local-llm")]
    let registry = Arc::new(llm::LlmRegistry::new());

    // Subagents borrow the session's MCP tools as they stand now. A sidecar
    // added later grows the *primary* agent's tool list via `extend_tools`,
    // but not this snapshot, so subagents launched afterwards still see the
    // startup set.
    let subagents = Arc::new(SubagentRegistry::new(SubagentContext {
        resolved: resolved.clone(),
        cfg: cfg.clone(),
        mcp_tools: all_tools.clone(),
        cache_root: cache_root.to_path_buf(),
        log_dir: log_dir.to_path_buf(),
        // The primary is the root at depth 1, so its subagents live at depth 2.
        depth: 2,
        #[cfg(feature = "local-llm")]
        registry: registry.clone(),
    }));
    let mut agent_tools = all_tools;
    // The primary gets the launch tools when its agent opts in *and* the depth
    // limit leaves room for a first layer (root depth 1 < max). A
    // `subagent-depth-max` of 1 disables subagents for everyone.
    let subagents_enabled = agent_name
        .and_then(|name| cfg.agents.get(name))
        .is_none_or(outrig::config::Agent::subagents_enabled);
    if subagents_enabled && resolved.subagent_depth_max > 1 {
        agent_tools.extend(builtin_tool::parent_tools(
            subagents.clone(),
            resolved.tool_result_max_bytes,
        ));
    }

    let span = ProgressSpan::start("building agent");
    let agent = llm::build_agent(
        &resolved,
        agent_tools.clone(),
        cache_root,
        #[cfg(feature = "local-llm")]
        &registry,
    )
    .await?;
    span.done("agent ready");

    print_banner(
        &resolved,
        image_cfg_name,
        image_tag,
        runtime.containers.primary.name(),
        &per_server_counts,
        &agent_tools,
        sid.as_str(),
    );

    let primary_name = runtime.containers.primary.name().to_string();
    let agent = llm::RebuildingAgent::new(
        agent,
        agent_tools,
        resolved,
        cache_root.to_path_buf(),
        #[cfg(feature = "local-llm")]
        registry,
    );

    let session = ReplSession {
        runtime: RefCell::new(runtime),
        agent: &agent,
        store,
        sid,
        cfg: &cfg,
        repo_root,
        log_dir,
        cli_env,
        mcp_plan,
    };

    eprintln!("[outrig] entering REPL");
    // When a watcher is armed, external death of the primary ends the REPL
    // with an error instead of leaving the agent talking to dead tools.
    let result = match primary_died {
        Some(died) => {
            tokio::select! {
                result = run_repl(session) => result,
                _ = died.cancelled() => {
                    Err(crate::cli::watcher::primary_death_error(&primary_name))
                }
            }
        }
        None => run_repl(session).await,
    };

    // Stop every subagent *and wait for the tasks to end* before teardown.
    // An orphan would keep calling tools into a container being removed, and
    // until each task is reaped it still holds tool clones that teardown's
    // `Arc::try_unwrap` needs released to shut the MCP children down.
    subagents.shutdown().await;
    drop(subagents);

    // Drop the agent (which owns the tool adapters) before returning so
    // teardown's `Arc::try_unwrap` on each `mcp_arcs` entry succeeds.
    // `run_repl` borrowed it, so this is the last ref.
    drop(agent);

    result
}

/// Everything the REPL callbacks close over: the rebuildable agent plus the
/// session state the `/sidecar` command reads and grows. The runtime sits
/// in a `RefCell`: callbacks run strictly sequentially on the
/// current-thread runtime, so borrows never overlap.
struct ReplSession<'a> {
    runtime: RefCell<&'a mut SessionRuntime>,
    agent: &'a llm::RebuildingAgent,
    store: &'a SessionStore,
    sid: &'a SessionId,
    cfg: &'a Config,
    repo_root: &'a Path,
    log_dir: &'a Path,
    cli_env: &'a CliEnvEntries,
    mcp_plan: &'a SessionMcpPlan,
}

/// The `outrig run` REPL command set: each command's name and `/help` line
/// here, its semantics in the `on_command` dispatcher in [`run_repl`]
/// directly below. `/help` and `/quit` are the [`Repl`]'s own.
const REPL_COMMANDS: &[HelpEntry] = &[
    HelpEntry {
        syntax: "/tools",
        description: "list registered tools",
    },
    HelpEntry {
        syntax: "/reset",
        description: "clear conversation history",
    },
    HelpEntry {
        syntax: "/sidecar add <name>",
        description: "start a config-declared manual sidecar",
    },
    HelpEntry {
        syntax: "/sidecar list",
        description: "show declared sidecars and their status",
    },
];

async fn run_repl(session: ReplSession<'_>) -> Result<i32> {
    // Single-task REPL: callbacks run sequentially. RefCell over the shared
    // history avoids needing Send bounds via Arc<Mutex<_>>; the binary's
    // tokio runtime is current-thread.
    let history: Rc<RefCell<Vec<Message>>> = Rc::new(RefCell::new(Vec::new()));

    let agent = session.agent;

    let history_for_prompt = history.clone();
    let on_prompt = move |line: String| {
        let history = history_for_prompt.clone();
        async move {
            // Move the vec out so the RefCell isn't borrowed across the
            // await; restore it on completion. Prompt cancellation may add
            // partial history to `h`, so it must always be written back.
            let mut h = std::mem::take(&mut *history.borrow_mut());
            let result = agent.run_turn(&line, &mut h).await;
            *history.borrow_mut() = h;
            result
        }
    };

    let session = &session;
    let on_command = move |cmd: String, args: Vec<String>| {
        let history = history.clone();
        async move {
            match cmd.as_str() {
                // Zero-arg commands with args fall through to None so the
                // REPL reports e.g. `/tools foo` as unknown, as it always
                // has.
                "tools" if args.is_empty() => Some(build_tools_summary(&agent.tools())),
                "reset" if args.is_empty() => {
                    history.borrow_mut().clear();
                    Some("[outrig] history cleared".to_string())
                }
                "sidecar" => Some(handle_sidecar_command(session, &args).await),
                _ => None,
            }
        }
    };

    Repl::run("", REPL_COMMANDS, on_prompt, on_command).await?;
    Ok(0)
}

/// Dispatch for the `/sidecar` slash command. Always returns stderr text --
/// errors are reported to the user and never escape to the REPL loop, so a
/// failed add cannot end the session.
async fn handle_sidecar_command(state: &ReplSession<'_>, args: &[String]) -> String {
    match args {
        [sub, name] if sub == "add" => sidecar_add(state, name).await,
        [sub] if sub == "list" => sidecar_list(state).await,
        _ => "[outrig] usage: /sidecar add <name> | /sidecar list".to_string(),
    }
}

/// `/sidecar add <name>`: start a config-declared `start = "manual"`
/// sidecar mid-session. Declared-only by design (the library API is the
/// arbitrary-spec surface); unknown and already-running names are errors.
async fn sidecar_add(state: &ReplSession<'_>, name: &str) -> String {
    let Some(sc) = state.mcp_plan.sidecars.get(name) else {
        let manual: Vec<&str> = state
            .mcp_plan
            .sidecars
            .iter()
            .filter(|(_, sc)| sc.start == SidecarStart::Manual)
            .map(|(name, _)| name.as_str())
            .collect();
        return if manual.is_empty() {
            format!(
                "[outrig] no sidecar named {name:?}; the image config declares no manual sidecars"
            )
        } else {
            format!(
                "[outrig] no sidecar named {name:?}; manual sidecars: {}",
                manual.join(", ")
            )
        };
    };
    if state
        .runtime
        .borrow()
        .containers
        .sidecars
        .contains_key(name)
    {
        return format!("[outrig] sidecar {name:?} is already running");
    }
    if sc.start != SidecarStart::Manual {
        return format!(
            "[outrig] sidecar {name:?} is start = \"auto\" and is started by the \
             session; only start = \"manual\" sidecars can be added"
        );
    }
    match try_sidecar_add(state, name, sc).await {
        Ok(text) => text,
        Err(e) => format!("[outrig] sidecar add failed: {e}; session unaffected"),
    }
}

/// The fallible tail of [`sidecar_add`]: start -> attach interceptor ->
/// connect servers -> commit. Any failure unwinds everything this call
/// started (clients, interceptor attachment, container) and leaves the
/// session state untouched.
async fn try_sidecar_add(state: &ReplSession<'_>, name: &str, sc: &SidecarPlan) -> Result<String> {
    let (host_workspace, container_workspace, transcript) = {
        let runtime = state.runtime.borrow();
        let primary = &runtime.containers.primary;
        (
            primary.host_workspace().to_path_buf(),
            primary.container_workspace().to_path_buf(),
            primary.transcript(),
        )
    };
    let ctx = SidecarStartCtx {
        cfg: state.cfg,
        repo_root: state.repo_root,
        sid: state.sid.as_str(),
        host_workspace: &host_workspace,
        container_workspace: &container_workspace,
        transcript: transcript.as_ref(),
    };
    let container = session_setup::launch_declared_sidecar(&ctx, state.mcp_plan, sc).await?;

    // Take the interceptor out of the shared slot around the await so no
    // RefCell borrow is held across it; slash callbacks run sequentially,
    // so nothing observes the empty slot.
    let taken = state.runtime.borrow_mut().network.take();
    if let Some(mut interceptor) = taken {
        let attached = interceptor.attach(&container).await;
        state.runtime.borrow_mut().network = Some(interceptor);
        if let Err(e) = attached {
            let _ = container.stop(STOP_GRACE).await;
            return Err(e.into());
        }
    }

    let (new_arcs, new_adapters) =
        match connect_added_sidecar_servers(state, name, &container).await {
            Ok(connected) => connected,
            Err(e) => {
                let taken = state.runtime.borrow_mut().network.take();
                if let Some(mut interceptor) = taken {
                    let _ = interceptor.detach(container.name()).await;
                    state.runtime.borrow_mut().network = Some(interceptor);
                }
                let _ = container.stop(STOP_GRACE).await;
                return Err(e);
            }
        };

    let container_name = container.name().to_string();
    let server_names: Vec<String> = new_arcs.iter().map(|arc| arc.name().to_string()).collect();
    let tool_count = new_adapters.len();

    {
        let mut runtime = state.runtime.borrow_mut();
        runtime
            .containers
            .sidecars
            .insert(name.to_string(), container);
        runtime.mcp_arcs.extend(new_arcs);
        if let Some(watcher) = runtime.watcher.as_mut() {
            watcher.register_sidecar(container_name.clone());
        }
    }
    state.agent.extend_tools(new_adapters);

    let mut text = format!(
        "[outrig] sidecar {name} started: {container_name}\n\
         [outrig] {} MCP {} connected ({}); {tool_count} {} available on the next turn",
        server_names.len(),
        plural(server_names.len(), "server", "servers"),
        server_names.join(", "),
        plural(tool_count, "tool", "tools"),
    );
    let names = state.runtime.borrow().containers.sidecar_names();
    if let Err(e) = state.store.set_sidecar_containers(state.sid, &names) {
        let _ = write!(
            text,
            "\n[outrig] warning: failed to record the sidecar in the session store: {e}"
        );
    }
    Ok(text)
}

/// Connect every plan server hosted by `name` against the started sidecar
/// and build its tool adapters. On failure every client this call connected
/// is shut down before the error propagates; the container and interceptor
/// attachment are the caller's to unwind.
async fn connect_added_sidecar_servers(
    state: &ReplSession<'_>,
    name: &str,
    container: &Container,
) -> Result<(Vec<Arc<McpClient>>, Vec<SessionTool>)> {
    let mut arcs: Vec<Arc<McpClient>> = Vec::new();
    let mut adapters: Vec<McpToolAdapter> = Vec::new();
    let mut failure: Option<crate::error::CliError> = None;
    for (mcp_name, placed) in state.mcp_plan.servers_in(name) {
        let extra_env = state.cli_env.for_server(mcp_name);
        let client = match McpClient::connect_via_podman_exec_with_source(
            container,
            &placed.spec,
            mcp_name,
            placed.source,
            state.log_dir,
            &extra_env,
        )
        .await
        {
            Ok(client) => Arc::new(client),
            Err(e) => {
                failure = Some(e.into());
                break;
            }
        };
        match McpToolAdapter::from_client_tools(client.clone(), state.agent.tool_result_max_bytes())
            .await
        {
            Ok(new) => {
                adapters.extend(new);
                arcs.push(client);
            }
            Err(e) => {
                // Push the failing client too so the unwind reaches it.
                arcs.push(client);
                failure = Some(e);
                break;
            }
        }
    }
    match failure {
        None => Ok((arcs, session_tool::erase(adapters))),
        Some(e) => {
            // Adapters hold client Arc clones; drop them first so the
            // unwrap below reaches each client.
            drop(adapters);
            for arc in arcs {
                if let Ok(client) = Arc::try_unwrap(arc) {
                    let _ = client.shutdown().await;
                }
            }
            Err(e)
        }
    }
}

/// `/sidecar list`: every declared sidecar (auto and manual, named and
/// anonymous) with its status and hosted servers.
async fn sidecar_list(state: &ReplSession<'_>) -> String {
    if state.mcp_plan.sidecars.is_empty() {
        return "[outrig] no sidecars declared".to_string();
    }
    let pad = state
        .mcp_plan
        .sidecars
        .keys()
        .map(|name| name.len())
        .max()
        .unwrap_or(0);
    let mut buf = String::from("[outrig] sidecars:");
    for name in state.mcp_plan.sidecars.keys() {
        let container_name = state
            .runtime
            .borrow()
            .containers
            .sidecars
            .get(name)
            .map(|container| container.name().to_string());
        let status = match &container_name {
            None => "not started",
            Some(container_name) => match Container::is_running(container_name).await {
                Ok(true) => "running",
                Ok(false) => "exited",
                Err(_) => "unknown",
            },
        };
        let servers: Vec<&str> = state
            .mcp_plan
            .servers_in(name)
            .map(|(server, _)| server.as_str())
            .collect();
        let servers = if servers.is_empty() {
            "(none)".to_string()
        } else {
            servers.join(", ")
        };
        let _ = write!(buf, "\n  {name:<pad$}   {status:<11}   servers: {servers}");
    }
    buf
}

fn print_banner(
    resolved: &llm::ResolvedAgent,
    container_name: &str,
    image_tag: &ImageTag,
    container_pod_name: &str,
    per_server_counts: &[(String, usize)],
    all_tools: &[SessionTool],
    session_id: &str,
) {
    let provider_label = match &resolved.provider {
        llm::ResolvedProvider::OpenAi { .. } => "openai",
        llm::ResolvedProvider::Anthropic { .. } => "anthropic",
        llm::ResolvedProvider::Mistralrs => "mistralrs",
    };
    let mut buf = String::new();
    // An agentless session has no agent name to print, so the banner leads
    // with the model.
    let _ = match &resolved.agent_name {
        Some(agent) => writeln!(
            buf,
            "[outrig] agent:             {} (model: {} / provider: {} / {})",
            agent, resolved.model_name, provider_label, resolved.model_identifier
        ),
        None => writeln!(
            buf,
            "[outrig] model:             {} (provider: {} / {})",
            resolved.model_name, provider_label, resolved.model_identifier
        ),
    };
    let _ = writeln!(
        buf,
        "[outrig] tool-call max:     {}",
        resolved.tool_call_max
    );
    let _ = writeln!(
        buf,
        "[outrig] tool-result max:   {} bytes",
        resolved.tool_result_max_bytes
    );
    if let Some(weights) = &resolved.model_weights {
        let _ = writeln!(buf, "[outrig] model device:      {}", weights.device);
    }
    let _ = writeln!(buf, "[outrig] image-config:  {container_name}");
    let _ = writeln!(buf, "[outrig] image:             {image_tag}");
    let _ = writeln!(buf, "[outrig] container started: {container_pod_name}");
    for (name, count) in per_server_counts {
        let plural = if *count == 1 { "tool" } else { "tools" };
        let _ = writeln!(buf, "[outrig] mcp {name}: initialized ({count} {plural})");
    }
    let names: Vec<String> = all_tools.iter().map(ToolDyn::name).collect();
    let _ = writeln!(buf, "[outrig] tools available: {}", names.join(", "));
    let _ = writeln!(
        buf,
        "[outrig] session id: {session_id}   (Ctrl-D to exit, /help for slash commands)"
    );
    eprint!("{buf}");
}

fn build_tools_summary(tools: &[SessionTool]) -> String {
    let mut buf = String::new();
    let _ = writeln!(buf, "[outrig] tools available ({}):", tools.len());
    let rows: Vec<(String, String)> = tools
        .iter()
        .map(|t| (t.name(), truncate_description(&t.description(), 60)))
        .collect();
    let pad = rows.iter().map(|(name, _)| name.len()).max().unwrap_or(0);
    for (name, desc) in &rows {
        let _ = writeln!(buf, "  {name:<pad$}   {desc}");
    }
    buf
}

fn truncate_description(desc: &str, max: usize) -> String {
    let cleaned = desc.lines().next().unwrap_or("").trim();
    if cleaned.len() <= max {
        cleaned.to_string()
    } else {
        let cut = cleaned
            .char_indices()
            .nth(max)
            .map(|(i, _)| i)
            .unwrap_or(cleaned.len());
        format!("{}...", &cleaned[..cut])
    }
}

fn apply_tool_call_max_override(resolved: &mut llm::ResolvedAgent, max_tool_calls: Option<u32>) {
    if let Some(max_tool_calls) = max_tool_calls {
        resolved.tool_call_max = max_tool_calls as usize;
    }
}

fn apply_tool_result_max_override(
    resolved: &mut llm::ResolvedAgent,
    max_tool_result_bytes: Option<u32>,
) {
    if let Some(max_tool_result_bytes) = max_tool_result_bytes {
        resolved.tool_result_max_bytes = max_tool_result_bytes as usize;
    }
}

fn parse_tool_call_max(s: &str) -> std::result::Result<u32, String> {
    let value = s
        .parse::<u32>()
        .map_err(|_| format!("must be an integer between 1 and {TOOL_CALL_MAX_LIMIT}"))?;
    if !(1..=TOOL_CALL_MAX_LIMIT).contains(&value) {
        return Err(format!(
            "must be between 1 and {TOOL_CALL_MAX_LIMIT}; got {value}"
        ));
    }
    Ok(value)
}

fn parse_tool_result_max(s: &str) -> std::result::Result<u32, String> {
    let value = s.parse::<u32>().map_err(|_| {
        format!(
            "must be an integer between {TOOL_RESULT_MAX_FLOOR_BYTES} and \
             {TOOL_RESULT_MAX_CEILING_BYTES}"
        )
    })?;
    if !(TOOL_RESULT_MAX_FLOOR_BYTES..=TOOL_RESULT_MAX_CEILING_BYTES).contains(&value) {
        return Err(format!(
            "must be between {TOOL_RESULT_MAX_FLOOR_BYTES} and \
             {TOOL_RESULT_MAX_CEILING_BYTES}; got {value}"
        ));
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fully-populated [`llm::ResolvedAgent`]; tests override the fields
    /// they exercise via struct-update syntax.
    fn test_resolved_agent() -> llm::ResolvedAgent {
        llm::ResolvedAgent {
            agent_name: Some("coding".to_string()),
            model_name: "fast".to_string(),
            model_identifier: "gpt-4o-mini".to_string(),
            provider_name: "local".to_string(),
            provider: llm::ResolvedProvider::Mistralrs,
            model_weights: None,
            preamble: Some("test".to_string()),
            temperature: None,
            max_tokens: None,
            tool_call_max: 100,
            tool_result_max_bytes: llm::DEFAULT_TOOL_RESULT_MAX_BYTES,
            subagent_depth_max: outrig::config::DEFAULT_SUBAGENT_DEPTH_MAX,
            subagent_width_max: outrig::config::DEFAULT_SUBAGENT_WIDTH_MAX,
            image: None,
        }
    }

    /// Locks `/help` to the exact text the REPL printed when it owned the
    /// command list (pre-dispatcher `HELP_TEXT`).
    #[test]
    fn repl_help_is_byte_identical_to_the_pre_dispatcher_text() {
        assert_eq!(
            crate::repl::compose_help(REPL_COMMANDS),
            "\
[outrig] slash commands:
  /help                 show this help
  /tools                list registered tools
  /reset                clear conversation history
  /sidecar add <name>   start a config-declared manual sidecar
  /sidecar list         show declared sidecars and their status
  /quit                 exit the session
"
        );
    }

    mod sidecar_cmd {
        use super::*;
        use crate::cli::session_setup::SessionContainers;
        use outrig::container::sidecar::plan_from_config;

        /// Owned session state backing a [`ReplSession`]; no podman.
        struct Fixture {
            runtime: SessionRuntime,
            agent: llm::RebuildingAgent,
            store: SessionStore,
            sid: SessionId,
            cfg: Config,
            repo_root: PathBuf,
            log_dir: PathBuf,
            cli_env: CliEnvEntries,
            mcp_plan: SessionMcpPlan,
        }

        impl Fixture {
            /// Async because [`llm::build_agent`] is; the OpenAi arm does
            /// no I/O, so the dummy agent builds instantly.
            async fn new(config_toml: &str) -> Self {
                let cfg: Config = toml::from_str(config_toml).expect("config parses");
                let image_cfg = cfg.images["x"].clone();
                let primary = Container::attach(
                    "outrig-test",
                    ImageTag::new("img:latest"),
                    Some((Path::new("/host/ws"), Path::new("/workspace"))),
                    None,
                );
                let resolved = llm::ResolvedAgent {
                    provider: llm::ResolvedProvider::OpenAi {
                        base_url: "http://127.0.0.1:9".to_string(),
                        api_key: "test-key".to_string(),
                        request_timeout_secs: None,
                        // Retries off: the discard port refuses connections,
                        // which is transient by the retry loop's reckoning, and
                        // this test wants the failure now rather than after a
                        // budget's worth of backoff.
                        retry_budget_secs: Some(0),
                    },
                    tool_result_max_bytes: 1024,
                    ..test_resolved_agent()
                };
                #[cfg(feature = "local-llm")]
                let registry = Arc::new(llm::LlmRegistry::new());
                let rig_agent = llm::build_agent(
                    &resolved,
                    Vec::new(),
                    Path::new("."),
                    #[cfg(feature = "local-llm")]
                    &registry,
                )
                .await
                .expect("openai agent builds without I/O");
                Self {
                    runtime: SessionRuntime::new(
                        None,
                        None,
                        SessionContainers {
                            sidecars: std::collections::BTreeMap::new(),
                            primary,
                        },
                    ),
                    agent: llm::RebuildingAgent::new(
                        rig_agent,
                        Vec::new(),
                        resolved,
                        PathBuf::from("."),
                        #[cfg(feature = "local-llm")]
                        registry,
                    ),
                    store: SessionStore::new(std::env::temp_dir()),
                    sid: SessionId::from("test".to_string()),
                    cfg: cfg.clone(),
                    repo_root: PathBuf::from("."),
                    log_dir: PathBuf::from("logs"),
                    cli_env: CliEnvEntries::parse(&[]).expect("empty env parses"),
                    mcp_plan: plan_from_config(&cfg, &image_cfg),
                }
            }

            fn state(&mut self) -> ReplSession<'_> {
                ReplSession {
                    runtime: RefCell::new(&mut self.runtime),
                    agent: &self.agent,
                    store: &self.store,
                    sid: &self.sid,
                    cfg: &self.cfg,
                    repo_root: &self.repo_root,
                    log_dir: &self.log_dir,
                    cli_env: &self.cli_env,
                    mcp_plan: &self.mcp_plan,
                }
            }
        }

        const DECLARED: &str = r#"
[images.x]
dockerfile = "D"
context    = "."

[sidecars.tools]
image = "img-tools"
start = "manual"

[sidecars.autos]
image = "img-auto"

[images.x.mcp]
fs   = { command = ["mcp-fs"], sidecar = "tools" }
auto = { command = ["mcp-auto"], sidecar = "autos" }
"#;

        fn args(items: &[&str]) -> Vec<String> {
            items.iter().map(|s| s.to_string()).collect()
        }

        #[tokio::test]
        async fn unknown_name_is_an_error_listing_manual_sidecars() {
            let mut fixture = Fixture::new(DECLARED).await;
            let text = handle_sidecar_command(&fixture.state(), &args(&["add", "nope"])).await;
            assert!(text.contains("no sidecar named \"nope\""), "{text}");
            assert!(text.contains("manual sidecars: tools"), "{text}");
        }

        #[tokio::test]
        async fn auto_sidecar_cannot_be_added() {
            let mut fixture = Fixture::new(DECLARED).await;
            let text = handle_sidecar_command(&fixture.state(), &args(&["add", "autos"])).await;
            assert!(text.contains("started by the session"), "{text}");
        }

        #[tokio::test]
        async fn already_running_sidecar_is_an_error() {
            let mut fixture = Fixture::new(DECLARED).await;
            fixture.runtime.containers.sidecars.insert(
                "tools".to_string(),
                Container::attach(
                    "outrig-test-tools",
                    ImageTag::new("img-tools"),
                    None,
                    None,
                ),
            );
            let text = handle_sidecar_command(&fixture.state(), &args(&["add", "tools"])).await;
            assert!(text.contains("already running"), "{text}");
        }

        #[tokio::test]
        async fn bad_arguments_return_usage() {
            let mut fixture = Fixture::new(DECLARED).await;
            for bad in [
                args(&[]),
                args(&["add"]),
                args(&["add", "a", "b"]),
                args(&["bogus"]),
            ] {
                let text = handle_sidecar_command(&fixture.state(), &bad).await;
                assert!(text.contains("usage: /sidecar"), "args {bad:?}: {text}");
            }
        }

        #[tokio::test]
        async fn list_shows_declared_sidecars_with_status_and_servers() {
            let mut fixture = Fixture::new(DECLARED).await;
            let text = handle_sidecar_command(&fixture.state(), &args(&["list"])).await;
            assert!(text.contains("tools"), "{text}");
            assert!(text.contains("autos"), "{text}");
            assert!(text.contains("not started"), "{text}");
            assert!(text.contains("servers: fs"), "{text}");
        }

        #[tokio::test]
        async fn list_without_declared_sidecars_says_so() {
            let mut fixture =
                Fixture::new("[images.x]\ndockerfile = \"D\"\ncontext = \".\"\n").await;
            let text = handle_sidecar_command(&fixture.state(), &args(&["list"])).await;
            assert!(text.contains("no sidecars declared"), "{text}");
        }
    }

    #[test]
    fn max_tool_calls_arg_accepts_in_range_value() {
        let args = RunArgs::try_parse_from(["run", "--max-tool-calls", "200"]).expect("arg parses");
        assert_eq!(args.max_tool_calls, Some(200));
    }

    #[test]
    fn max_tool_calls_arg_rejects_out_of_range_value() {
        let err =
            RunArgs::try_parse_from(["run", "--max-tool-calls", "0"]).expect_err("zero is invalid");
        let msg = err.to_string();
        assert!(
            msg.contains("must be between 1 and 2000"),
            "unexpected clap error: {msg}",
        );
    }

    #[test]
    fn max_tool_result_bytes_arg_accepts_in_range_value() {
        let args = RunArgs::try_parse_from(["run", "--max-tool-result-bytes", "65536"])
            .expect("arg parses");
        assert_eq!(args.max_tool_result_bytes, Some(65536));
    }

    #[test]
    fn max_tool_result_bytes_arg_rejects_out_of_range_value() {
        let err = RunArgs::try_parse_from(["run", "--max-tool-result-bytes", "0"])
            .expect_err("zero is invalid");
        let msg = err.to_string();
        assert!(
            msg.contains("must be between 1024 and 16777216"),
            "unexpected clap error: {msg}",
        );
    }

    #[test]
    fn device_arg_accepts_mistralrs_device_forms() {
        let args = RunArgs::try_parse_from(["run", "--device", "cuda:2"]).expect("arg parses");
        assert_eq!(args.device, Some(MistralrsDeviceSpec::Cuda(2)));
    }

    #[test]
    fn device_arg_rejects_unknown_device() {
        let err = RunArgs::try_parse_from(["run", "--device", "gpu"])
            .expect_err("unknown device is invalid");
        let msg = err.to_string();
        assert!(
            msg.contains(MistralrsDeviceSpec::EXPECTED),
            "unexpected clap error: {msg}",
        );
    }

    #[test]
    fn model_arg_accepts_name() {
        let args = RunArgs::try_parse_from(["run", "--model", "smart"]).expect("arg parses");
        assert_eq!(args.model.as_deref(), Some("smart"));
    }

    #[test]
    fn cli_override_replaces_resolved_tool_call_max() {
        let mut resolved = test_resolved_agent();

        apply_tool_call_max_override(&mut resolved, Some(50));

        assert_eq!(resolved.tool_call_max, 50);
    }

    #[test]
    fn cli_override_replaces_resolved_tool_result_max() {
        let mut resolved = llm::ResolvedAgent {
            tool_result_max_bytes: 262_144,
            ..test_resolved_agent()
        };

        apply_tool_result_max_override(&mut resolved, Some(65_536));

        assert_eq!(resolved.tool_result_max_bytes, 65_536);
    }

    #[test]
    fn env_flag_collects_multiple_values() {
        let args = RunArgs::try_parse_from(["run", "--env", "FOO=bar", "--env", "BAZ=quux"])
            .expect("arg parses");
        assert_eq!(args.env, vec!["FOO=bar", "BAZ=quux"]);
    }

    #[test]
    fn env_flag_absent_yields_empty_vec() {
        let args = RunArgs::try_parse_from(["run"]).expect("arg parses");
        assert!(args.env.is_empty());
    }

    #[test]
    fn volume_flag_collects_multiple_values() {
        let args =
            RunArgs::try_parse_from(["run", "--volume", "/h1:/c1", "--volume", "/h2:/c2:rw"])
                .expect("arg parses");
        assert_eq!(args.volume.len(), 2);
        assert_eq!(args.volume[0].container, std::path::PathBuf::from("/c1"));
    }

    #[test]
    fn volume_flag_rejects_bad_value() {
        let err = RunArgs::try_parse_from(["run", "--volume", "/h:/c:bogus"])
            .expect_err("bad access should fail");
        assert!(
            err.to_string().contains("ro` or `rw"),
            "unexpected error: {err}"
        );
    }
}
