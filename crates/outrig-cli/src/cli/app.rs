use clap::{ArgAction, Args, Parser, Subcommand};
use std::path::PathBuf;
use std::process::ExitCode;
use tracing_subscriber::EnvFilter;

use crate::cli::build::{self, BuildArgs};
use crate::cli::clean::{self, CleanArgs};
use crate::cli::design_prompt::{self, DesignArgs};
use crate::cli::discard::{self, DiscardArgs};
use crate::cli::logs::{self, LogsArgs};
use crate::cli::ls::{self, LsArgs};
use crate::cli::mcp::{self, McpArgs};
use crate::cli::mcp_self as mcp_self_cli;
use crate::cli::run::{self, RunArgs};
use crate::error::Result;
use crate::paths::{global_config_path, resolve_repo_config};
use crate::{config_init, image_setup, init};

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
    /// Generate prompts and setup snippets for AI-assisted design.
    Design(DesignArgs),
    /// Build (or cache-hit) one or more image-config images.
    Build(BuildArgs),
    /// Read or write outrig's configuration files.
    Config(ConfigArgs),
    /// Interactively set up global + repo config.
    Init {
        /// Overwrite existing files. Propagates to `config init` and `image add`.
        #[arg(long)]
        force: bool,
    },
    /// Manage image-configs.
    Image(ImageArgs),
    /// List sessions newest-first under the session root.
    Ls(LsArgs),
    /// Print or follow a session's MCP-server stderr.
    Logs(LogsArgs),
    /// Delete a session's on-disk record.
    Discard(DiscardArgs),
    /// Delete old stopped session records.
    Clean(CleanArgs),
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
struct ImageArgs {
    #[command(subcommand)]
    cmd: ImageCmd,
}

#[derive(Debug, Subcommand)]
enum ImageCmd {
    /// Scaffold a new image-config (Dockerfile + `[images.<name>]`).
    Add {
        /// Image-config name. Prompted if omitted.
        name: Option<String>,
        /// Overwrite an existing Dockerfile / config block of this name.
        #[arg(long)]
        force: bool,
    },
    /// Scaffold a standalone image project (Dockerfile + image.toml + README).
    Init {
        /// Project directory. Defaults to the current directory; its name
        /// becomes the image ref.
        dir: Option<PathBuf>,
        /// Overwrite the generated files if they already exist.
        #[arg(long)]
        force: bool,
    },
}

pub fn run() -> ExitCode {
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
                return runtime.block_on(mcp_self_cli::execute(args));
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
        Cmd::Design(args) => design_prompt::execute(args),
        Cmd::Build(args) => {
            let (repo_config, global_config, runtime) = repo_cmd_ctx(cli)?;
            runtime.block_on(build::execute(&repo_config, &global_config, args))
        }
        Cmd::Config(args) => match &args.cmd {
            ConfigCmd::Init { force } => {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()?;
                runtime.block_on(config_init::run(*force, cli.global_config.as_deref()))?;
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
        Cmd::Image(args) => match &args.cmd {
            ImageCmd::Add { name, force } => {
                let cwd = std::env::current_dir()?;
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()?;
                runtime.block_on(image_setup::add::run(
                    &cwd,
                    cli.global_config.as_deref(),
                    name.clone(),
                    *force,
                ))?;
                Ok(0)
            }
            ImageCmd::Init { dir, force } => {
                let cwd = std::env::current_dir()?;
                image_setup::init::run(&cwd, dir.as_deref(), *force)?;
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
        Cmd::Clean(args) => {
            let (cwd, global, runtime) = session_cmd_ctx(cli)?;
            let session_root = cli.session_root.as_deref();
            let repo_cfg = cli.config.as_deref();
            runtime.block_on(clean::execute(args, session_root, repo_cfg, &global, &cwd))
        }
    }
}

fn init_tracing(verbose: u8) {
    let outrig_log = std::env::var("OUTRIG_LOG").ok();
    let rust_log = std::env::var("RUST_LOG").ok();
    let mut filter =
        EnvFilter::try_new(log_filter_spec(outrig_log.as_deref(), rust_log.as_deref()))
            .unwrap_or_else(|_| EnvFilter::new("info"));
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

fn log_filter_spec<'a>(outrig_log: Option<&'a str>, rust_log: Option<&'a str>) -> &'a str {
    outrig_log.or(rust_log).unwrap_or("info")
}

/// Shared preamble for `ls`/`logs`/`discard`: cwd, the resolved global
/// config path, and a current-thread tokio runtime ready to drive the
/// async `execute` form of each subcommand. The repo config is resolved
/// inside each handler because session lookups can substring-match across
/// repos and shouldn't fail on a missing repo config.
fn session_cmd_ctx(cli: &Cli) -> Result<(PathBuf, PathBuf, tokio::runtime::Runtime)> {
    let cwd = std::env::current_dir()?;
    let global = global_config_path(cli.global_config.as_deref());
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
    let repo_config = resolve_repo_config(cli.config.as_deref(), &cwd)?;
    let global_config = global_config_path(cli.global_config.as_deref());
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    Ok((repo_config, global_config, runtime))
}

#[cfg(test)]
mod tests {
    use super::log_filter_spec;

    #[test]
    fn outrig_log_wins_over_rust_log() {
        assert_eq!(
            log_filter_spec(Some("outrig=trace"), Some("debug")),
            "outrig=trace"
        );
    }

    #[test]
    fn rust_log_is_used_when_outrig_log_is_unset() {
        assert_eq!(log_filter_spec(None, Some("debug")), "debug");
    }

    #[test]
    fn log_filter_defaults_to_info() {
        assert_eq!(log_filter_spec(None, None), "info");
    }
}
