use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use clap::Parser;
use directories::BaseDirs;
use outrig_cli::llm::{build_agent, resolve_agent_with_overrides, RigAgent};
use outrig_rune_harness_prototype::{
    channels, ActivationRequest, AgentDriver, Decision, EventBridge, ExternalEvent, Invocation,
    ModelBackend,
};
use std::path::{Path, PathBuf};
use tokio::io::{AsyncBufReadExt, BufReader};

#[derive(Parser, Debug)]
#[command(about = "Event-driven, host-scoped Rune agent prototype")]
struct Args {
    #[arg(long, default_value = ".")]
    repo: PathBuf,
    #[arg(long)]
    agent: Option<String>,
    #[arg(long)]
    model: Option<String>,
    /// Override the user-level OutRig configuration file.
    #[arg(long)]
    global_config: Option<PathBuf>,
}

fn global_config_path_with(
    override_path: Option<&Path>,
    xdg_config_home: Option<&Path>,
    home: &Path,
) -> PathBuf {
    if let Some(path) = override_path {
        return path.to_path_buf();
    }
    if let Some(xdg) = xdg_config_home {
        return xdg.join("outrig").join("config.toml");
    }
    home.join(".outrig").join("config.toml")
}

fn resolve_global_config_with(
    override_path: Option<&Path>,
    xdg_config_home: Option<&Path>,
    home: &Path,
    is_file: impl FnOnce(&Path) -> bool,
) -> Result<Option<PathBuf>> {
    let path = global_config_path_with(override_path, xdg_config_home, home);
    if is_file(&path) {
        Ok(Some(path))
    } else if override_path.is_some() {
        bail!(
            "--global-config does not exist or is not a file: {}",
            path.display()
        )
    } else {
        Ok(None)
    }
}

fn resolve_global_config(override_path: Option<&Path>) -> Result<Option<PathBuf>> {
    let xdg = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from);
    let home = BaseDirs::new()
        .map(|dirs| dirs.home_dir().to_path_buf())
        .unwrap_or_default();
    resolve_global_config_with(override_path, xdg.as_deref(), &home, Path::is_file)
}

struct RealModel {
    agent: RigAgent,
}
#[async_trait(?Send)]
impl ModelBackend for RealModel {
    async fn activate(&mut self, request: ActivationRequest) -> Result<Decision> {
        let schema = r#"Reply with JSON only, exactly one of:
{"decision":"execute_rune","source":"Rune snippet body"}
{"decision":"emit","text":"user-facing answer"}
Do not add markdown fences."#;
        let prompt = format!(
            "{schema}\n\nActivation request (there are no prior provider messages):\n{}",
            serde_json::to_string_pretty(&request)?
        );
        let text = self
            .agent
            .activate_text_once(&prompt)
            .await
            .context("history-free model completion")?;
        parse_decision(&text)
    }
}

fn parse_decision(text: &str) -> Result<Decision> {
    let trimmed = text.trim();
    if let Ok(value) = serde_json::from_str(trimmed) {
        return Ok(value);
    }
    let body = trimmed
        .strip_prefix("```json")
        .or_else(|| trimmed.strip_prefix("```"))
        .and_then(|s| s.strip_suffix("```"))
        .map(str::trim);
    match body {
        Some(body) => serde_json::from_str(body).context("parse fenced Decision JSON"),
        None => bail!("model did not return Decision JSON: {trimmed}"),
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let args = Args::parse();
    let repo = args
        .repo
        .canonicalize()
        .with_context(|| format!("canonicalize --repo {}", args.repo.display()))?;
    let global_config = resolve_global_config(args.global_config.as_deref())?;
    let (config, config_root) = outrig::load_project(&repo, global_config.as_deref())
        .context("load project and global OutRig configuration")?;
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
    let stable = "You are an event-driven Rune agent. Durable state is the host binding inventory, not chat history. Discover capabilities with doc(fs), then choose files yourself through the persistent fs.read(relative_path). Use events::next().await only when the same Rune run must wait for the next terminal event. Never assume a previous provider message exists.";
    resolved.preamble = Some(match resolved.preamble.take() {
        Some(p) if !p.is_empty() => format!("{p}\n\n{stable}"),
        _ => stable.into(),
    });
    let cache = std::env::temp_dir().join("outrig-rune-event-cache");
    let agent = build_agent(&resolved, vec![], &cache)
        .await
        .context("build configured tool-free Rig agent")?;
    let bridge = EventBridge::new();
    let invocation =
        Invocation::new(&repo, bridge).context("create host-owned Rune invocation scope")?;
    let (input_tx, input_rx, output_tx, mut output_rx) = channels();

    // Stdin is an independent producer; it never waits for model or Rune work.
    let reader = tokio::spawn(async move {
        let mut lines = BufReader::new(tokio::io::stdin()).lines();
        let mut id = 0;
        while let Some(text) = lines.next_line().await? {
            id += 1;
            if input_tx
                .send(ExternalEvent::UserInput { id, text })
                .is_err()
            {
                return Ok::<_, std::io::Error>(());
            }
        }
        let _ = input_tx.send(ExternalEvent::Eof);
        Ok::<_, std::io::Error>(())
    });
    let printer = tokio::spawn(async move {
        while let Some(text) = output_rx.recv().await {
            println!("{text}");
        }
    });
    eprintln!(
        "[outrig-harness] event-driven repo={} agent={} model={}",
        repo.display(),
        selected_agent.unwrap_or("(agentless)"),
        resolved.model_name()
    );
    let driver = AgentDriver::new(RealModel { agent }, invocation, input_rx, output_tx);
    let _driver = driver.run().await?;
    reader.await.context("stdin task join")??;
    printer.await.context("output task join")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn global_config_override_wins() {
        let explicit = Path::new("/explicit/config.toml");
        assert_eq!(
            global_config_path_with(
                Some(explicit),
                Some(Path::new("/xdg")),
                Path::new("/home/alice")
            ),
            explicit
        );
    }

    #[test]
    fn global_config_uses_xdg_before_home() {
        assert_eq!(
            global_config_path_with(None, Some(Path::new("/xdg")), Path::new("/home/alice")),
            Path::new("/xdg/outrig/config.toml")
        );
    }

    #[test]
    fn global_config_falls_back_to_outrig_home() {
        assert_eq!(
            global_config_path_with(None, None, Path::new("/home/alice")),
            Path::new("/home/alice/.outrig/config.toml")
        );
    }

    #[test]
    fn missing_explicit_global_config_is_an_error_but_missing_default_is_optional() {
        let explicit = Path::new("/missing/config.toml");
        let error = resolve_global_config_with(
            Some(explicit),
            Some(Path::new("/xdg")),
            Path::new("/home/alice"),
            |_| false,
        )
        .unwrap_err();
        assert!(error.to_string().contains("--global-config"));
        assert!(error.to_string().contains("/missing/config.toml"));
        assert_eq!(
            resolve_global_config_with(
                None,
                Some(Path::new("/xdg")),
                Path::new("/home/alice"),
                |_| false
            )
            .unwrap(),
            None
        );
    }

    #[test]
    fn existing_default_global_config_is_passed_to_loader() {
        let expected = Path::new("/xdg/outrig/config.toml");
        assert_eq!(
            resolve_global_config_with(
                None,
                Some(Path::new("/xdg")),
                Path::new("/home/alice"),
                |path| path == expected
            )
            .unwrap()
            .as_deref(),
            Some(expected)
        );
    }

    #[test]
    fn parses_plain_and_fenced_decisions() {
        assert!(matches!(
            parse_decision(r#"{"decision":"emit","text":"ok"}"#).unwrap(),
            Decision::Emit { .. }
        ));
        assert!(
            parse_decision("```json\n{\"decision\":\"execute_rune\",\"source\":\"1\"}\n```")
                .is_ok()
        );
    }
}
