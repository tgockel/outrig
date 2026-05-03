use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::process::ExitCode;
use tracing_subscriber::EnvFilter;

use outrig::cli::discard::{self, DiscardArgs};
use outrig::cli::logs::{self, LogsArgs};
use outrig::cli::ls::{self, LsArgs};
use outrig::cli::run::{self, RunArgs};
use outrig::error::{OutrigError, Result};
use outrig::repo;

#[derive(Debug, Parser)]
#[command(
    name = "outrig",
    version,
    about = "Run LLM agents inside podman-managed containers."
)]
struct Cli {
    /// Path to the repo `config.toml`. Defaults to walking up from cwd.
    #[arg(long, global = true, value_name = "PATH")]
    config: Option<PathBuf>,

    /// Path to the global config. Defaults to `~/.outrig/config.toml`.
    #[arg(long = "global-config", global = true, value_name = "PATH")]
    global_config: Option<PathBuf>,

    /// Override the session root for this invocation. Default cascade:
    /// flag > config's `session-root` > `<XDG_DATA_HOME>/outrig/sessions/`.
    #[arg(long = "session-root", global = true, value_name = "PATH")]
    session_root: Option<PathBuf>,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Debug, Subcommand)]
enum Cmd {
    /// Start an interactive agent session.
    Run(RunArgs),
    /// Build (or cache-hit) one or more container-config images.
    Build,
    /// Interactively set up global + repo config.
    Init,
    /// Scaffold a new container-config.
    InitContainer,
    /// List sessions newest-first under the session root.
    Ls(LsArgs),
    /// Print or follow a session's MCP-server stderr.
    Logs(LogsArgs),
    /// Delete a session's on-disk record.
    Discard(DiscardArgs),
}

fn main() -> ExitCode {
    let filter = EnvFilter::try_from_env("OUTRIG_LOG").unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();

    outrig::container::install_panic_hook();

    tracing::debug!("outrig starting");

    let cli = Cli::parse();
    match dispatch(&cli) {
        Ok(0) => ExitCode::SUCCESS,
        Ok(code) => ExitCode::from(code.clamp(0, 255) as u8),
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::from(1)
        }
    }
}

fn dispatch(cli: &Cli) -> Result<i32> {
    match &cli.cmd {
        Cmd::Run(args) => {
            let cwd = std::env::current_dir()?;
            let repo_config = repo::resolve_repo_config(cli.config.as_deref(), &cwd)?;
            let global_config = repo::global_config_path(cli.global_config.as_deref());
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            runtime.block_on(run::execute(
                &repo_config,
                &global_config,
                cli.session_root.as_deref(),
                args,
            ))
        }
        Cmd::Build => Err(OutrigError::NotImplemented("build")),
        Cmd::Init => Err(OutrigError::NotImplemented("init")),
        Cmd::InitContainer => Err(OutrigError::NotImplemented("init-container")),
        Cmd::Ls(args) => {
            let (cwd, global, runtime) = session_cmd_ctx(cli)?;
            let session_root = cli.session_root.as_deref();
            let repo_cfg = cli.config.as_deref();
            runtime.block_on(ls::execute(args, session_root, repo_cfg, &global, &cwd))
        }
        Cmd::Logs(args) => {
            let (cwd, global, runtime) = session_cmd_ctx(cli)?;
            let session_root = cli.session_root.as_deref();
            let repo_cfg = cli.config.as_deref();
            runtime.block_on(logs::execute(args, session_root, repo_cfg, &global, &cwd))
        }
        Cmd::Discard(args) => {
            let (cwd, global, runtime) = session_cmd_ctx(cli)?;
            let session_root = cli.session_root.as_deref();
            let repo_cfg = cli.config.as_deref();
            runtime.block_on(discard::execute(
                args,
                session_root,
                repo_cfg,
                &global,
                &cwd,
            ))
        }
    }
}

/// Shared preamble for `ls`/`logs`/`discard`: cwd, the resolved global
/// config path, and a current-thread tokio runtime ready to drive the
/// async `execute` form of each subcommand. `Cmd::Run` keeps its own
/// preamble because it also resolves the repo config eagerly.
fn session_cmd_ctx(cli: &Cli) -> Result<(PathBuf, PathBuf, tokio::runtime::Runtime)> {
    let cwd = std::env::current_dir()?;
    let global = repo::global_config_path(cli.global_config.as_deref());
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    Ok((cwd, global, runtime))
}
