use clap::{Parser, Subcommand};
use std::process::ExitCode;
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
#[command(
    name = "outrig",
    version,
    about = "Run LLM agents inside podman-managed containers."
)]
struct Cli {
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

    tracing::debug!("outrig starting");

    let cli = Cli::parse();
    let name: &'static str = match cli.cmd {
        Cmd::Run => "run",
        Cmd::Build => "build",
        Cmd::Init => "init",
        Cmd::InitContainer => "init-container",
        Cmd::Ls => "ls",
        Cmd::Logs => "logs",
        Cmd::Discard => "discard",
    };

    tracing::debug!("subcommand {name} not implemented");
    eprintln!("error: not implemented");
    ExitCode::from(1)
}
