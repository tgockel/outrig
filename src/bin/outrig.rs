use clap::{ArgAction, Args, Parser, Subcommand};
use std::path::PathBuf;
use std::process::ExitCode;
use tracing_subscriber::EnvFilter;

use outrig::cli::build::{self, BuildArgs};
use outrig::cli::discard::{self, DiscardArgs};
use outrig::cli::logs::{self, LogsArgs};
use outrig::cli::ls::{self, LsArgs};
use outrig::cli::mcp::{self, McpArgs};
use outrig::cli::mcp_self;
use outrig::cli::run::{self, RunArgs};
use outrig::config;
use outrig::error::Result;
use outrig::init;
use outrig::repo;

#[derive(Debug, Parser)]
#[command(
    name = "outrig",
    version,
    about = "Run LLM agents with podman-isolated MCP servers."
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

    /// Show buildah/podman transcripts. Repeat for trace-level outrig logs.
    #[arg(short = 'v', long = "verbose", global = true, action = ArgAction::Count)]
    verbose: u8,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Debug, Subcommand)]
enum Cmd {
    /// Start an interactive agent session.
    Run(RunArgs),
    /// Serve the configured backing MCPs as a single MCP server over stdio.
    Mcp(McpArgs),
    /// Build (or cache-hit) one or more container-config images.
    Build(BuildArgs),
    /// Read or write outrig's configuration files.
    Config(ConfigArgs),
    /// Interactively set up global + repo config.
    Init {
        /// Overwrite existing files. Propagates to `config init` and `container add`.
        #[arg(long)]
        force: bool,
    },
    /// Manage container-configs.
    Container(ContainerArgs),
    /// List sessions newest-first under the session root.
    Ls(LsArgs),
    /// Print or follow a session's MCP-server stderr.
    Logs(LogsArgs),
    /// Delete a session's on-disk record.
    Discard(DiscardArgs),
}

#[derive(Debug, Args)]
struct ConfigArgs {
    #[command(subcommand)]
    cmd: ConfigCmd,
}

#[derive(Debug, Subcommand)]
enum ConfigCmd {
    /// Interactively write the global config (`~/.outrig/config.toml`).
    Init {
        /// Overwrite an existing global config.
        #[arg(long)]
        force: bool,
    },
}

#[derive(Debug, Args)]
struct ContainerArgs {
    #[command(subcommand)]
    cmd: ContainerCmd,
}

#[derive(Debug, Subcommand)]
enum ContainerCmd {
    /// Scaffold a new container-config (Dockerfile + `[containers.<name>]`).
    Add {
        /// Container-config name. Prompted if omitted.
        name: Option<String>,
        /// Overwrite an existing Dockerfile / config block of this name.
        #[arg(long)]
        force: bool,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    init_tracing(cli.verbose);

    outrig::container::install_panic_hook();

    tracing::debug!("outrig starting");
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
            let (repo_config, global_config, runtime) = repo_cmd_ctx(cli)?;
            runtime.block_on(run::execute(
                &repo_config,
                &global_config,
                cli.session_root.as_deref(),
                args,
                cli.verbose,
            ))
        }
        Cmd::Mcp(args) => {
            if args.is_self_description() {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()?;
                return runtime.block_on(mcp_self::execute(args));
            }
            let (repo_config, global_config, runtime) = repo_cmd_ctx(cli)?;
            runtime.block_on(mcp::execute(
                &repo_config,
                &global_config,
                cli.session_root.as_deref(),
                args,
                cli.verbose,
            ))
        }
        Cmd::Build(args) => {
            let (repo_config, global_config, runtime) = repo_cmd_ctx(cli)?;
            runtime.block_on(build::execute(&repo_config, &global_config, args))
        }
        Cmd::Config(args) => match &args.cmd {
            ConfigCmd::Init { force } => {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()?;
                runtime.block_on(config::init::run(*force, cli.global_config.as_deref()))?;
                Ok(0)
            }
        },
        Cmd::Init { force } => {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            runtime.block_on(init::run(*force, cli.global_config.as_deref()))?;
            Ok(0)
        }
        Cmd::Container(args) => match &args.cmd {
            ContainerCmd::Add { name, force } => {
                let cwd = std::env::current_dir()?;
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()?;
                runtime.block_on(outrig::container::add::run(
                    &cwd,
                    cli.global_config.as_deref(),
                    name.clone(),
                    *force,
                ))?;
                Ok(0)
            }
        },
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

fn init_tracing(verbose: u8) {
    let mut filter =
        EnvFilter::try_from_env("OUTRIG_LOG").unwrap_or_else(|_| EnvFilter::new("info"));
    if verbose >= 2 {
        filter = filter.add_directive(
            "outrig=trace"
                .parse()
                .expect("hard-coded outrig trace directive must parse"),
        );
    }
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();
    if verbose >= 2 {
        tracing::trace!(target: "outrig", "verbose tracing enabled");
    }
}

/// Shared preamble for `ls`/`logs`/`discard`: cwd, the resolved global
/// config path, and a current-thread tokio runtime ready to drive the
/// async `execute` form of each subcommand. The repo config is resolved
/// inside each handler because session lookups can substring-match across
/// repos and shouldn't fail on a missing repo config.
fn session_cmd_ctx(cli: &Cli) -> Result<(PathBuf, PathBuf, tokio::runtime::Runtime)> {
    let cwd = std::env::current_dir()?;
    let global = repo::global_config_path(cli.global_config.as_deref());
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    Ok((cwd, global, runtime))
}

/// Shared preamble for `run`/`mcp`/`build`: the resolved repo config, the
/// resolved global config, and a current-thread tokio runtime. Errors if
/// the repo config can't be located (the user must be inside an outrig
/// repo for these to make sense).
fn repo_cmd_ctx(cli: &Cli) -> Result<(PathBuf, PathBuf, tokio::runtime::Runtime)> {
    let cwd = std::env::current_dir()?;
    let repo_config = repo::resolve_repo_config(cli.config.as_deref(), &cwd)?;
    let global_config = repo::global_config_path(cli.global_config.as_deref());
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    Ok((repo_config, global_config, runtime))
}
