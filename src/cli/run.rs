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

use clap::Parser;
use rig::completion::Message;

use crate::cli::session_setup::{self, SessionSetup, SessionSetupArgs};
use crate::config::{Config, ContainerConfig};
use crate::container::Container;
use crate::error::Result;
use crate::image::ImageTag;
use crate::llm;
use crate::mcp::McpClient;
use crate::repl::Repl;
use crate::repo;
use crate::rig_tool::McpToolAdapter;

#[derive(Debug, Parser)]
pub struct RunArgs {
    /// Pick an `[agents.<name>]` block. Defaults to `default-agent` from config.
    #[arg(long, value_name = "NAME")]
    pub agent: Option<String>,

    /// Pick a `[containers.<name>]` block. Overrides the agent's `container`
    /// and the top-level `default-container`.
    #[arg(long, value_name = "NAME")]
    pub container: Option<String>,

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
    let setup = session_setup::setup(SessionSetupArgs {
        repo_cfg_path,
        global_cfg_path,
        session_root_flag,
        container_flag: args.container.as_deref(),
        agent_flag: args.agent.as_deref(),
        require_agent: true,
        explicit_session_dir: args.session_dir.as_deref(),
    })
    .await?;

    let agent_name = setup
        .session
        .agent_name
        .clone()
        .expect("outrig run always resolves an agent in setup");
    let SessionSetup {
        cfg,
        container_cfg_name,
        container_cfg,
        image_tag,
        container,
        sid,
        log_dir,
        store,
        session: _,
        session_dir: _,
    } = setup;
    let cache_root = repo::model_cache_root(cfg.model_cache_root.as_deref());

    let mut mcp_arcs: Vec<Arc<McpClient>> = Vec::new();
    let outcome: Result<i32> = run_inner(
        &cfg,
        &agent_name,
        &container_cfg_name,
        &container_cfg,
        &image_tag,
        &container,
        &log_dir,
        sid.as_str(),
        &cache_root,
        &mut mcp_arcs,
    )
    .await;

    let final_exit = outcome.as_ref().copied().unwrap_or(1);
    session_setup::teardown(mcp_arcs, container, &store, &sid, final_exit).await;
    outcome
}

#[allow(clippy::too_many_arguments)]
async fn run_inner(
    cfg: &Config,
    agent_name: &str,
    container_cfg_name: &str,
    container_cfg: &ContainerConfig,
    image_tag: &ImageTag,
    container: &Container,
    log_dir: &Path,
    session_id: &str,
    cache_root: &Path,
    mcp_arcs: &mut Vec<Arc<McpClient>>,
) -> Result<i32> {
    // `setup` already validated presence and used the resolved `.container`
    // for the container fallback. We re-resolve here for `build_agent` +
    // banner; cheap (config table lookups, no I/O).
    let resolved = llm::resolve_agent(cfg, agent_name)?;

    let connected = session_setup::connect_mcp_clients(container, container_cfg, log_dir).await?;
    mcp_arcs.extend(connected);

    let mut all_tools: Vec<McpToolAdapter> = Vec::new();
    let mut per_server_counts: Vec<(String, usize)> = Vec::new();
    for arc in mcp_arcs.iter() {
        let adapters = McpToolAdapter::from_client_tools(arc.clone()).await?;
        per_server_counts.push((arc.name().to_string(), adapters.len()));
        all_tools.extend(adapters);
    }

    #[cfg(feature = "mistralrs")]
    let registry = llm::LlmRegistry::new();

    let agent = llm::build_agent(
        &resolved,
        all_tools.clone(),
        cache_root,
        #[cfg(feature = "mistralrs")]
        &registry,
    )
    .await?;

    print_banner(
        &resolved,
        container_cfg_name,
        image_tag,
        &container.name,
        &per_server_counts,
        &all_tools,
        session_id,
    );

    let tools_summary = build_tools_summary(&all_tools);
    let result = run_repl(&agent, tools_summary).await;

    // Drop adapters and the agent before returning so teardown's
    // `Arc::try_unwrap` on each `mcp_arcs` entry succeeds.
    drop(all_tools);
    drop(agent);

    result
}

async fn run_repl(agent: &llm::RigAgent, tools_summary: String) -> Result<i32> {
    // Single-task REPL: callbacks run sequentially. RefCell over the shared
    // history avoids needing Send bounds via Arc<Mutex<_>>; the binary's
    // tokio runtime is current-thread.
    let history: Rc<RefCell<Vec<Message>>> = Rc::new(RefCell::new(Vec::new()));

    let history_for_prompt = history.clone();
    let on_prompt = move |line: String| {
        let history = history_for_prompt.clone();
        async move {
            // Move the vec out so the RefCell isn't borrowed across the
            // await; restore it on completion. Cancellation drops `h`,
            // losing this turn's partial history -- acceptable for v0.
            let mut h = std::mem::take(&mut *history.borrow_mut());
            let result = agent.run_turn(&line, &mut h).await;
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
    image_tag: &ImageTag,
    container_pod_name: &str,
    per_server_counts: &[(String, usize)],
    all_tools: &[McpToolAdapter],
    session_id: &str,
) {
    let provider_label = match &resolved.provider {
        llm::ResolvedProvider::OpenAi { .. } => "openai",
        llm::ResolvedProvider::Mistralrs => "mistralrs",
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
