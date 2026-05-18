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

use crate::cli::env_arg::CliEnvEntries;
use crate::cli::session_setup::{self, ProgressSpan, SessionSetup, SessionSetupArgs, plural};
use crate::error::{OutrigError, Result};
use crate::llm;
use crate::paths::model_cache_root;
use crate::repl::Repl;
use crate::rig_tool::McpToolAdapter;
use outrig::McpClient;
use outrig::config::{
    Config, MAX_TOOL_CALL_CAP, MAX_TOOL_RESULT_CAP_BYTES, MIN_TOOL_RESULT_CAP_BYTES,
    MistralrsDeviceSpec, NetworkMode,
};
use outrig::container::Container;
use outrig::image::ImageTag;

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

    /// Override the per-turn tool-call cap for this run.
    #[arg(long = "max-tool-calls", value_name = "N", value_parser = parse_tool_call_cap)]
    pub max_tool_calls: Option<u32>,

    /// Override the per-result truncation cap for this run.
    #[arg(long = "max-tool-result-bytes", value_name = "N", value_parser = parse_tool_result_cap)]
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
        container_flag: args.container.as_deref(),
        attach_target: None,
        agent_flag: args.agent.as_deref(),
        require_agent: true,
        explicit_session_dir: args.session_dir.as_deref(),
        network_mode_override: args.network,
        device_override: args.device,
        verbose,
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
        network,
        attached: _,
        session: _,
        session_dir: _,
    } = setup;
    let cache_root = model_cache_root(cfg.model_cache_root.as_deref());

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
    let outcome: Result<i32> = run_inner(
        &cfg,
        &agent_name,
        &container_cfg_name,
        &image_tag,
        &container,
        &log_dir,
        sid.as_str(),
        &cache_root,
        &mut mcp_arcs,
        args.max_tool_calls,
        args.max_tool_result_bytes,
        args.device,
        &mcp,
        &cli_env,
    )
    .await;

    let final_exit = outcome.as_ref().copied().unwrap_or(1);
    session_setup::teardown(mcp_arcs, network, container, &store, &sid, final_exit).await;
    outcome
}

fn parse_network_mode(s: &str) -> std::result::Result<NetworkMode, String> {
    s.parse()
}

fn parse_mistralrs_device(s: &str) -> std::result::Result<MistralrsDeviceSpec, String> {
    s.parse::<MistralrsDeviceSpec>().map_err(|e| e.to_string())
}

#[allow(clippy::too_many_arguments)]
async fn run_inner(
    cfg: &Config,
    agent_name: &str,
    container_cfg_name: &str,
    image_tag: &ImageTag,
    container: &Container,
    log_dir: &Path,
    session_id: &str,
    cache_root: &Path,
    mcp_arcs: &mut Vec<Arc<McpClient>>,
    max_tool_calls: Option<u32>,
    max_tool_result_bytes: Option<u32>,
    device_override: Option<MistralrsDeviceSpec>,
    mcp: &std::collections::BTreeMap<String, outrig::config::McpServerSpec>,
    cli_env: &CliEnvEntries,
) -> Result<i32> {
    // `setup` already validated presence and used the resolved `.container`
    // for the container fallback. We re-resolve here for `build_agent` +
    // banner; cheap (config table lookups, no I/O).
    let mut resolved = llm::resolve_agent_with_device_override(cfg, agent_name, device_override)?;
    apply_tool_call_cap_override(&mut resolved, max_tool_calls);
    apply_tool_result_cap_override(&mut resolved, max_tool_result_bytes);

    let connected = session_setup::connect_mcp_clients(container, mcp, log_dir, cli_env).await?;
    mcp_arcs.extend(connected);

    let mut all_tools: Vec<McpToolAdapter> = Vec::new();
    let mut per_server_counts: Vec<(String, usize)> = Vec::new();
    for arc in mcp_arcs.iter() {
        let span = ProgressSpan::start(format!("MCP {}: listing tools", arc.name()));
        let adapters =
            McpToolAdapter::from_client_tools(arc.clone(), resolved.tool_result_cap_bytes).await?;
        let tool_count = adapters.len();
        let tool_word = plural(tool_count, "tool", "tools");
        span.done(format!(
            "MCP {}: tools ready: {tool_count} {tool_word}",
            arc.name()
        ));
        per_server_counts.push((arc.name().to_string(), tool_count));
        all_tools.extend(adapters);
    }

    #[cfg(feature = "local-llm")]
    let registry = llm::LlmRegistry::new();

    let span = ProgressSpan::start("building agent");
    let agent = llm::build_agent(
        &resolved,
        all_tools.clone(),
        cache_root,
        #[cfg(feature = "local-llm")]
        &registry,
    )
    .await?;
    span.done("agent ready");

    print_banner(
        &resolved,
        container_cfg_name,
        image_tag,
        container.name(),
        &per_server_counts,
        &all_tools,
        session_id,
    );

    let tools_summary = build_tools_summary(&all_tools);
    eprintln!("[outrig] entering REPL");
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
            // await; restore it on completion. Prompt cancellation may add
            // partial history to `h`, so it must always be written back.
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
    let _ = writeln!(
        buf,
        "[outrig] tool-call cap:     {}",
        resolved.tool_call_cap
    );
    let _ = writeln!(
        buf,
        "[outrig] tool-result cap:   {} bytes",
        resolved.tool_result_cap_bytes
    );
    if let Some(weights) = &resolved.model_weights {
        let _ = writeln!(buf, "[outrig] model device:      {}", weights.device);
    }
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

fn apply_tool_call_cap_override(resolved: &mut llm::ResolvedAgent, max_tool_calls: Option<u32>) {
    if let Some(max_tool_calls) = max_tool_calls {
        resolved.tool_call_cap = max_tool_calls as usize;
    }
}

fn apply_tool_result_cap_override(
    resolved: &mut llm::ResolvedAgent,
    max_tool_result_bytes: Option<u32>,
) {
    if let Some(max_tool_result_bytes) = max_tool_result_bytes {
        resolved.tool_result_cap_bytes = max_tool_result_bytes as usize;
    }
}

fn parse_tool_call_cap(s: &str) -> std::result::Result<u32, String> {
    let value = s
        .parse::<u32>()
        .map_err(|_| format!("must be an integer between 1 and {MAX_TOOL_CALL_CAP}"))?;
    if !(1..=MAX_TOOL_CALL_CAP).contains(&value) {
        return Err(format!(
            "must be between 1 and {MAX_TOOL_CALL_CAP}; got {value}"
        ));
    }
    Ok(value)
}

fn parse_tool_result_cap(s: &str) -> std::result::Result<u32, String> {
    let value = s.parse::<u32>().map_err(|_| {
        format!(
            "must be an integer between {MIN_TOOL_RESULT_CAP_BYTES} and \
             {MAX_TOOL_RESULT_CAP_BYTES}"
        )
    })?;
    if !(MIN_TOOL_RESULT_CAP_BYTES..=MAX_TOOL_RESULT_CAP_BYTES).contains(&value) {
        return Err(format!(
            "must be between {MIN_TOOL_RESULT_CAP_BYTES} and \
             {MAX_TOOL_RESULT_CAP_BYTES}; got {value}"
        ));
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn cli_override_replaces_resolved_tool_call_cap() {
        let mut resolved = llm::ResolvedAgent {
            agent_name: "coding".to_string(),
            model_name: "fast".to_string(),
            model_identifier: "gpt-4o-mini".to_string(),
            provider_name: "local".to_string(),
            provider: llm::ResolvedProvider::Mistralrs,
            model_weights: None,
            preamble: "test".to_string(),
            temperature: None,
            max_tokens: None,
            tool_call_cap: 100,
            tool_result_cap_bytes: llm::DEFAULT_TOOL_RESULT_CAP_BYTES,
            container: None,
        };

        apply_tool_call_cap_override(&mut resolved, Some(50));

        assert_eq!(resolved.tool_call_cap, 50);
    }

    #[test]
    fn cli_override_replaces_resolved_tool_result_cap() {
        let mut resolved = llm::ResolvedAgent {
            agent_name: "coding".to_string(),
            model_name: "fast".to_string(),
            model_identifier: "gpt-4o-mini".to_string(),
            provider_name: "local".to_string(),
            provider: llm::ResolvedProvider::Mistralrs,
            model_weights: None,
            preamble: "test".to_string(),
            temperature: None,
            max_tokens: None,
            tool_call_cap: 100,
            tool_result_cap_bytes: 262_144,
            container: None,
        };

        apply_tool_result_cap_override(&mut resolved, Some(65_536));

        assert_eq!(resolved.tool_result_cap_bytes, 65_536);
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
}
