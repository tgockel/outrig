use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::process::ExitCode;
use tracing_subscriber::EnvFilter;

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

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Debug, Subcommand)]
enum Cmd {
    /// Start an interactive agent session.
    Run,
    /// Build (or cache-hit) one or more container-config images.
    Build,
    /// Interactively set up global + repo config.
    Init,
    /// Scaffold a new container-config.
    InitContainer,
    /// List sessions newest-first under the session root.
    Ls,
    /// Print or follow a session's MCP-server stderr.
    Logs,
    /// Delete a session's on-disk record.
    Discard,
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
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::from(1)
        }
    }
}

fn dispatch(cli: &Cli) -> Result<()> {
    match cli.cmd {
        Cmd::Run => {
            let cwd = std::env::current_dir()?;
            let _repo_config = repo::resolve_repo_config(cli.config.as_deref(), &cwd)?;
            Err(OutrigError::NotImplemented("run"))
        }
        Cmd::Build => Err(OutrigError::NotImplemented("build")),
        Cmd::Init => Err(OutrigError::NotImplemented("init")),
        Cmd::InitContainer => Err(OutrigError::NotImplemented("init-container")),
        Cmd::Ls => Err(OutrigError::NotImplemented("ls")),
        Cmd::Logs => Err(OutrigError::NotImplemented("logs")),
        Cmd::Discard => Err(OutrigError::NotImplemented("discard")),
    }
}
