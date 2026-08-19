use anyhow::{bail, Context, Result};
use clap::Parser;
use outrig_cli::llm::{build_agent, resolve_agent_with_overrides};
use outrig_cli::session_tool::SessionTool;
use outrig_rune_harness_prototype::{ExecuteArgs, Invocation};
use rig::completion::Message;
use rig::tool::{ToolDyn, ToolError};
use rig::wasm_compat::WasmBoxedFuture;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, BufReader};

#[derive(Parser, Debug)]
#[command(about = "Scenario-specific real-model Rune harness prototype")]
struct Args {
    #[arg(long, default_value = ".")]
    repo: PathBuf,
    #[arg(long)]
    file: PathBuf,
    #[arg(long)]
    agent: Option<String>,
    #[arg(long)]
    model: Option<String>,
}

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
struct RuneToolError(String);

#[derive(Clone)]
struct ExecuteRune {
    invocation: Arc<Mutex<Invocation>>,
}
impl ToolDyn for ExecuteRune {
    fn name(&self) -> String {
        "execute_rune".to_string()
    }
    fn description(&self) -> String {
        "Execute one supported Rune source form in the persistent scenario scope. Use doc(fs), read the selected path once into retained source, or print a numeric source slice.".to_string()
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": { "source": { "type": "string", "description": "One documented supported Rune source form." } },
            "required": ["source"],
            "additionalProperties": false
        })
    }
    fn call<'a>(&'a self, args: String) -> WasmBoxedFuture<'a, Result<String, ToolError>> {
        Box::pin(async move {
            let args: ExecuteArgs = serde_json::from_str(&args).map_err(ToolError::JsonError)?;
            self.invocation
                .lock()
                .expect("Rune invocation mutex")
                .execute(&args.source)
                .map_err(|error| {
                    ToolError::ToolCallError(Box::new(RuneToolError(error.to_string())))
                })
        })
    }
}

fn selected_file(repo: &Path, file: &Path) -> Result<(PathBuf, PathBuf)> {
    let repo = repo
        .canonicalize()
        .with_context(|| format!("canonicalize repo {}", repo.display()))?;
    let candidate = if file.is_absolute() {
        file.to_path_buf()
    } else {
        repo.join(file)
    };
    let file = candidate
        .canonicalize()
        .with_context(|| format!("canonicalize selected file {}", candidate.display()))?;
    if !file.starts_with(&repo) {
        bail!(
            "selected file {} is outside repo root {}",
            file.display(),
            repo.display()
        );
    }
    if !file.is_file() {
        bail!("selected path is not a regular file: {}", file.display());
    }
    Ok((repo, file))
}

fn harness_preamble(existing: Option<String>, selected: &Path) -> String {
    let instructions = format!(
        r#"You have exactly one provider tool: execute_rune. It runs a persistent Rune invocation scope with globals fs, path (the selected file {:?}), and a retained source binding after a read. Call println!(\"{{}}\", doc(fs)); to inspect the canonical FileSystem capability. Supported source shapes are exactly: println!(\"{{}}\", doc(fs)); ; let source = fs.read(path).await?; println!(\"{{}}\", source); ; println!(\"{{}}\", source[START..END]); with numeric ranges. Output is bounded to 16 KiB and reports attempted bytes, truncation, and retained bindings. Reuse retained source instead of reading again. Unsupported programs return a recoverable tool error. Use execute_rune when observation is needed, then answer the user's terminal message as ordinary final assistant text; do not call a finish tool."#,
        selected
    );
    match existing {
        Some(mut preamble) if !preamble.is_empty() => {
            preamble.push_str("\n\n");
            preamble.push_str(&instructions);
            preamble
        }
        _ => instructions,
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let args = Args::parse();
    let (repo, file) = selected_file(&args.repo, &args.file)?;
    let (config, config_root) =
        outrig::load_project(&repo, None).context("load .agents/outrig/config.toml")?;
    if config_root.canonicalize()? != repo {
        bail!(
            "--repo must be the configured repository root (config resolved at {})",
            config_root.display()
        );
    }
    let selected_agent = args.agent.as_deref().or(config.default_agent.as_deref());
    let mut resolved =
        resolve_agent_with_overrides(&config, selected_agent, args.model.as_deref(), None)
            .context("resolve configured agent/model/provider")?;
    resolved.preamble = Some(harness_preamble(resolved.preamble.take(), &file));

    let invocation = Invocation::new(file.clone()).context("create persistent Rune scope")?;
    let tools = vec![SessionTool::new(ExecuteRune {
        invocation: Arc::new(Mutex::new(invocation)),
    })];
    let cache_root = std::env::temp_dir().join("outrig-rune-harness-cache");
    let agent = build_agent(&resolved, tools, &cache_root)
        .await
        .context("build configured Rig agent")?;

    eprintln!(
        "[outrig-harness] repo={} file={} agent={} model={}",
        repo.display(),
        file.display(),
        selected_agent.unwrap_or("(agentless)"),
        resolved.model_name()
    );
    let mut history: Vec<Message> = Vec::new();
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    while let Some(line) = lines.next_line().await.context("read stdin")? {
        if line.trim().is_empty() {
            continue;
        }
        let end = agent
            .run_turn(&line, &mut history)
            .await
            .context("model turn")?;
        if !end.reply.is_empty() {
            println!("{}", end.reply);
        }
        if end.is_silent() {
            println!("{}", end.silent_report());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn path_boundary_rejects_escape() -> Result<()> {
        let base = std::env::temp_dir().join(format!("outrig-boundary-{}", std::process::id()));
        let repo = base.join("repo");
        std::fs::create_dir_all(&repo)?;
        std::fs::write(base.join("outside"), "no")?;
        assert!(selected_file(&repo, Path::new("../outside")).is_err());
        Ok(())
    }
    #[test]
    fn preamble_preserves_agent_text_first() {
        let text = harness_preamble(Some("configured words".into()), Path::new("/repo/file"));
        assert!(text.starts_with("configured words\n\n"));
        assert!(text.contains("execute_rune"));
    }
}
