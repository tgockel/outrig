//! `outrig run` orchestrator: wire every subsystem into a working REPL.
//!
//! [`execute`] walks through the order documented in `doc/usage/run.md`:
//! load + merge + validate config, resolve agent -> model -> provider, pick a
//! container-config, ensure the image, start + bootstrap the container,
//! connect every MCP server, build adapters, build the Rig agent, print the
//! banner, and hand off to the REPL. On exit (clean or error) we shutdown
//! every MCP child first, *then* stop the container -- the MCP children are
//! `podman exec` processes whose pipes go through the container; tearing
//! the container down first races them.

use std::cell::RefCell;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use clap::Parser;
use rig::completion::Message;

use crate::config::Config;
use crate::container::Container;
use crate::error::{OutrigError, Result};
use crate::image;
use crate::llm;
use crate::mcp::McpClient;
use crate::repl::Repl;
use crate::repo;
use crate::rig_tool::McpToolAdapter;
use crate::session::{self, Session, SessionId, SessionStore};

const STOP_GRACE: Duration = Duration::from_secs(2);

#[derive(Debug, Parser)]
pub struct RunArgs {
    /// Pick an `[agents.<name>]` block. Defaults to `default-agent` from config.
    #[arg(long, value_name = "NAME")]
    pub agent: Option<String>,

    /// Pick a `[containers.<name>]` block. Overrides the agent's `container`
    /// and the top-level `default-container`.
    #[arg(long = "container-config", value_name = "NAME")]
    pub container_config: Option<String>,

    /// Write the session into an explicit, already-existing directory. The
    /// session root gets a symlink at `<root>/<sid>` pointing at this path.
    #[arg(long = "session-dir", value_name = "PATH")]
    pub session_dir: Option<PathBuf>,
}

/// Run one `outrig run` invocation end-to-end. Returns the process exit code.
pub async fn execute(
    repo_cfg_path: &Path,
    global_cfg_path: &Path,
    session_root_flag: Option<&Path>,
    args: &RunArgs,
) -> Result<i32> {
    let repo_root = repo::repo_root_from_config_path(repo_cfg_path);
    let cfg = Config::load(&repo_root, Some(global_cfg_path))?;

    let agent_name = args
        .agent
        .as_deref()
        .or(cfg.default_agent.as_deref())
        .ok_or_else(|| {
            OutrigError::Configuration("no --agent and no default-agent configured".to_string())
        })?;
    let resolved = llm::resolve_agent(&cfg, agent_name)?;

    let container_name = args
        .container_config
        .as_deref()
        .or(resolved.container.as_deref())
        .or(cfg.default_container.as_deref())
        .ok_or_else(|| {
            OutrigError::Configuration(
                "no --container-config, agent.container, or default-container configured"
                    .to_string(),
            )
        })?;
    let container_cfg = cfg.containers.get(container_name).ok_or_else(|| {
        OutrigError::Configuration(format!(
            "container-config {container_name:?} does not match any [containers.<name>]"
        ))
    })?;

    let image_tag = image::ensure_image(container_cfg, &repo_root).await?;

    let host_workspace = if cfg.workspace.host_path.is_absolute() {
        cfg.workspace.host_path.clone()
    } else {
        repo_root.join(&cfg.workspace.host_path)
    };
    let container_workspace = cfg.workspace.container_path.clone();

    if let Some(p) = args.session_dir.as_deref()
        && !p.is_dir()
    {
        return Err(OutrigError::Configuration(format!(
            "--session-dir {} is not an existing directory (create it first or omit the flag)",
            p.display()
        )));
    }

    let mut container_slot =
        Some(Container::start(&image_tag, &host_workspace, &container_workspace).await?);
    if let Some(c) = container_slot.as_mut() {
        c.bootstrap_user().await?;
    }

    let container = container_slot
        .as_ref()
        .expect("container_slot just populated");
    let sid = SessionId(container.session_suffix().to_string());

    let session_root =
        session::resolve_session_root(session_root_flag, &cfg, &repo::default_session_root());
    let store = SessionStore::new(session_root);
    let mut session = Session {
        id: sid.clone(),
        started_at: SystemTime::now(),
        ended_at: None,
        container_name: container.name.clone(),
        image_tag: image_tag.to_string(),
        container_config_name: container_name.to_string(),
        agent_name: resolved.agent_name.clone(),
        working_dir: repo_root.clone(),
        session_dir: PathBuf::new(), // set by `create` below
        exit_code: None,
        link_target: None,
    };
    let session_dir = store.create(&sid, args.session_dir.as_deref(), &mut session)?;
    let log_dir = session_dir.join("logs");
    tokio::fs::create_dir_all(&log_dir).await?;

    let mut mcp_arcs: Vec<Arc<McpClient>> = Vec::new();
    let cache_root = repo::model_cache_root(cfg.model_cache_root.as_deref());

    let outcome: Result<i32> = run_repl(
        &resolved,
        container_name,
        &image_tag,
        container,
        container_cfg,
        &log_dir,
        &cache_root,
        sid.as_str(),
        &mut mcp_arcs,
    )
    .await;

    // Cleanup. MCPs first (drop the Arc clones the agent + adapters held;
    // adapter drops happen when `run_repl` returns), then the container.
    // Errors during cleanup are logged but never override `outcome`.
    for arc in mcp_arcs.drain(..) {
        match Arc::try_unwrap(arc) {
            Ok(client) => {
                if let Err(e) = client.shutdown().await {
                    tracing::warn!(target: "outrig::cli::run", "mcp shutdown failed: {e}");
                }
            }
            Err(_) => {
                tracing::warn!(
                    target: "outrig::cli::run",
                    "mcp client still has outstanding refs at cleanup; relying on Drop"
                );
            }
        }
    }
    if let Some(c) = container_slot.take()
        && let Err(e) = c.stop(STOP_GRACE).await
    {
        tracing::warn!(target: "outrig::cli::run", "container stop failed: {e}");
    }

    let final_exit = outcome.as_ref().copied().unwrap_or(1);
    if let Err(e) = store.finalize(&sid, SystemTime::now(), final_exit) {
        tracing::warn!(target: "outrig::cli::run", "session finalize failed: {e}");
    }

    outcome
}

#[allow(clippy::too_many_arguments)]
async fn run_repl(
    resolved: &llm::ResolvedAgent,
    container_name: &str,
    image_tag: &image::ImageTag,
    container: &Container,
    container_cfg: &crate::config::ContainerConfig,
    log_dir: &Path,
    cache_root: &Path,
    session_id: &str,
    mcp_arcs: &mut Vec<Arc<McpClient>>,
) -> Result<i32> {
    let mut per_server_counts: Vec<(String, usize)> = Vec::new();
    let mut all_tools: Vec<McpToolAdapter> = Vec::new();

    for (mcp_name, spec) in &container_cfg.mcp {
        let client = McpClient::connect_via_podman_exec(container, spec, mcp_name, log_dir).await?;
        let arc = Arc::new(client);
        let adapters = McpToolAdapter::from_client_tools(arc.clone()).await?;
        per_server_counts.push((arc.name().to_string(), adapters.len()));
        all_tools.extend(adapters);
        mcp_arcs.push(arc);
    }

    #[cfg(feature = "mistralrs")]
    let registry = llm::LlmRegistry::new();

    let agent = llm::build_agent(
        resolved,
        all_tools.clone(),
        cache_root,
        #[cfg(feature = "mistralrs")]
        &registry,
    )
    .await?;

    print_banner(
        resolved,
        container_name,
        image_tag,
        &container.name,
        &per_server_counts,
        &all_tools,
        session_id,
    );

    let tools_summary = build_tools_summary(&all_tools);

    // Single-task REPL: callbacks run sequentially. RefCell over the shared
    // history avoids needing Send bounds via Arc<Mutex<_>>; the binary's
    // tokio runtime is current-thread.
    let history: Rc<RefCell<Vec<Message>>> = Rc::new(RefCell::new(Vec::new()));

    let history_for_prompt = history.clone();
    let agent_ref = &agent;
    let on_prompt = move |line: String| {
        let history = history_for_prompt.clone();
        async move {
            // Move the vec out so the RefCell isn't borrowed across the
            // await; restore it on completion. Cancellation drops `h`,
            // losing this turn's partial history -- acceptable for v0.
            let mut h = std::mem::take(&mut *history.borrow_mut());
            let result = agent_ref.run_turn(&line, &mut h).await;
            *history.borrow_mut() = h;
            result
        }
    };

    let on_tools = move || {
        let summary = tools_summary.clone();
        async move { summary }
    };

    let history_for_reset = history.clone();
    let on_reset = move || {
        let history = history_for_reset.clone();
        async move {
            history.borrow_mut().clear();
            "[outrig] history cleared".to_string()
        }
    };

    Repl::run("", on_prompt, on_tools, on_reset).await?;
    Ok(0)
}

fn print_banner(
    resolved: &llm::ResolvedAgent,
    container_name: &str,
    image_tag: &image::ImageTag,
    container_pod_name: &str,
    per_server_counts: &[(String, usize)],
    all_tools: &[McpToolAdapter],
    session_id: &str,
) {
    let provider_label = match &resolved.provider {
        llm::ResolvedProvider::OpenAi { .. } => "openai",
        llm::ResolvedProvider::Mistralrs { .. } => "mistralrs",
    };
    let mut buf = String::new();
    let _ = writeln!(
        buf,
        "[outrig] agent:             {} (model: {} / provider: {} / {})",
        resolved.agent_name, resolved.model_name, provider_label, resolved.model_identifier
    );
    let _ = writeln!(buf, "[outrig] container-config:  {container_name}");
    let _ = writeln!(buf, "[outrig] image:             {image_tag}");
    let _ = writeln!(buf, "[outrig] container started: {container_pod_name}");
    for (name, count) in per_server_counts {
        let plural = if *count == 1 { "tool" } else { "tools" };
        let _ = writeln!(buf, "[outrig] mcp {name}: initialized ({count} {plural})");
    }
    let names: Vec<&str> = all_tools.iter().map(|t| t.openai_name.as_str()).collect();
    let _ = writeln!(buf, "[outrig] tools available: {}", names.join(", "));
    let _ = writeln!(
        buf,
        "[outrig] session id: {session_id}   (Ctrl-D to exit, /help for slash commands)"
    );
    eprint!("{buf}");
}

fn build_tools_summary(tools: &[McpToolAdapter]) -> String {
    let mut buf = String::new();
    let _ = writeln!(buf, "[outrig] tools available ({}):", tools.len());
    let pad = tools.iter().map(|t| t.openai_name.len()).max().unwrap_or(0);
    for t in tools {
        let desc = truncate_description(&t.description, 60);
        let _ = writeln!(buf, "  {:<pad$}   {}", t.openai_name, desc, pad = pad);
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
