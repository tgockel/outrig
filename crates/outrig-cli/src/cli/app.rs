use clap::{ArgAction, Args, Parser, Subcommand};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::filter::Directive;

use crate::cli::build::{self, BuildArgs};
use crate::cli::clean::{self, CleanArgs};
use crate::cli::design_prompt::{self, DesignArgs};
use crate::cli::discard::{self, DiscardArgs};
use crate::cli::logs::{self, LogsArgs};
use crate::cli::ls::{self, LsArgs};
use crate::cli::mcp::{self, McpArgs};
use crate::cli::mcp_self as mcp_self_cli;
use crate::cli::run::{self, RunArgs};
use crate::error::{CliError, Result};
use crate::paths::{
    RepoConfig, resolve_global_config, resolve_repo_config, resolve_repo_config_optional,
};
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
    /// flag > global config's `session-root` > `<XDG_DATA_HOME>/outrig/sessions/`.
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
    /// Build a standalone image project and validate the built image.
    Build {
        /// Project directory holding `image.toml`. Defaults to the current
        /// directory.
        dir: Option<PathBuf>,
        /// Tag the build output as this ref instead of `[image].ref`. Does not
        /// rewrite `image.toml`.
        #[arg(long, value_name = "REF")]
        tag: Option<String>,
        /// Skip the live MCP server test. Still validates the embedded
        /// `image.toml`.
        #[arg(long = "no-test")]
        no_test: bool,
        /// Force a clean build (passes `--no-cache` to buildah).
        #[arg(long = "no-cache")]
        no_cache: bool,
    },
    /// Inspect an image's OutRig labels without starting it.
    Inspect {
        /// Inspect the registry ref with skopeo instead of the local image store.
        #[arg(long)]
        remote: bool,
        /// Image ref to inspect. Local mode never pulls; remote mode reads registry metadata.
        #[arg(value_name = "REF")]
        image_ref: String,
    },
}

pub fn run() -> ExitCode {
    let cli = Cli::parse();
    init_tracing(cli.verbose);
    raise_descriptor_limit();

    // The hook is the panic sweep only in an abort build; in this one, the
    // sweep is a panic leaving everything below.
    outrig::container::install_panic_hook();
    outrig::container::with_panic_sweep(|| {
        tracing::debug!("outrig starting");
        match dispatch(&cli) {
            Ok(0) => ExitCode::SUCCESS,
            Ok(code) => ExitCode::from(code.clamp(0, 255) as u8),
            Err(e) => {
                // A signal announced itself as it landed, and the terminal it
                // would be reported to may be gone.
                if !matches!(e, CliError::Interrupted(_)) {
                    eprintln!("error: {e}");
                }
                ExitCode::from(e.exit_code().clamp(0, 255) as u8)
            }
        }
    })
}

/// Drive a session command, then shut its runtime down without waiting on
/// blocking work. A REPL or a stdio MCP transport can leave a blocking stdin
/// read in flight that returns only when the other end writes or closes the
/// pipe, and dropping the runtime normally waits for it -- after a signal, a
/// stopped container, or a second Ctrl-C, the session was torn down and the
/// process sat there until someone pressed Enter. Every session has finished
/// its own teardown by the time `fut` resolves.
fn block_on_session(
    runtime: tokio::runtime::Runtime,
    fut: impl Future<Output = Result<i32>>,
) -> Result<i32> {
    let result = runtime.block_on(fut);
    runtime.shutdown_background();
    result
}

fn dispatch(cli: &Cli) -> Result<i32> {
    match &cli.cmd {
        Cmd::Run(args) => {
            let (repo_config, global_config, runtime) = repo_cmd_ctx(cli, false)?;
            block_on_session(
                runtime,
                run::execute(
                    &repo_config,
                    &global_config,
                    cli.session_root.as_deref(),
                    args,
                    cli.verbose,
                ),
            )
        }
        Cmd::Mcp(args) => {
            if args.is_self_description() {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()?;
                return runtime.block_on(mcp_self_cli::execute(args));
            }
            let (repo_config, global_config, runtime) = repo_cmd_ctx(cli, false)?;
            block_on_session(
                runtime,
                mcp::execute(
                    &repo_config,
                    &global_config,
                    cli.session_root.as_deref(),
                    args,
                    cli.verbose,
                ),
            )
        }
        Cmd::Design(args) => design_prompt::execute(args),
        Cmd::Build(args) => {
            let (repo_config, global_config, runtime) = repo_cmd_ctx(cli, true)?;
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
                let cwd = crate::paths::current_dir()?;
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
                let cwd = crate::paths::current_dir()?;
                image_setup::init::run(&cwd, dir.as_deref(), *force)?;
                Ok(0)
            }
            ImageCmd::Build {
                dir,
                tag,
                no_test,
                no_cache,
            } => {
                let cwd = crate::paths::current_dir()?;
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()?;
                runtime.block_on(image_setup::build::run(
                    &cwd,
                    dir.as_deref(),
                    tag.as_deref(),
                    *no_test,
                    *no_cache,
                ))?;
                Ok(0)
            }
            ImageCmd::Inspect { remote, image_ref } => {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()?;
                runtime.block_on(image_setup::inspect::run(image_ref, *remote))?;
                Ok(0)
            }
        },
        Cmd::Ls(args) => {
            let (global, runtime) = session_cmd_ctx(cli)?;
            runtime.block_on(ls::execute(args, cli.session_root.as_deref(), global))
        }
        Cmd::Logs(args) => {
            let (global, runtime) = session_cmd_ctx(cli)?;
            runtime.block_on(logs::execute(args, cli.session_root.as_deref(), global))
        }
        Cmd::Discard(args) => {
            let (global, runtime) = session_cmd_ctx(cli)?;
            runtime.block_on(discard::execute(args, cli.session_root.as_deref(), global))
        }
        Cmd::Clean(args) => {
            let (global, runtime) = session_cmd_ctx(cli)?;
            runtime.block_on(clean::execute(args, cli.session_root.as_deref(), global))
        }
    }
}

/// Raises this process's soft limit on open descriptors to its hard limit.
///
/// The soft limit is commonly 1024, kept low for programs that still use
/// `select`, which outrig does not. Under network audit or filter each
/// connection a container makes holds two of outrig's descriptors, on top of
/// the MCP pipes, logs and LLM sockets it already has, so a few busy
/// containers could take all 1024 between them -- after which an LLM call
/// fails as surely as the container's next connection. The hard limit is the
/// ceiling the host set, so going up to it asks for nothing more. Processes
/// outrig starts inherit it; podman, the main one, raises its own to the same
/// ceiling regardless.
fn raise_descriptor_limit() {
    use nix::sys::resource::{Resource, getrlimit, setrlimit};
    let raised = getrlimit(Resource::RLIMIT_NOFILE).and_then(|(soft, hard)| {
        if soft < hard {
            setrlimit(Resource::RLIMIT_NOFILE, hard, hard)?;
            tracing::debug!("raised the open-file limit from {soft} to {hard}");
        }
        Ok(())
    });
    if let Err(e) = raised {
        tracing::debug!("left the open-file limit as it was: {e}");
    }
}

fn init_tracing(verbose: u8) {
    let outrig_log = std::env::var("OUTRIG_LOG").ok();
    let rust_log = std::env::var("RUST_LOG").ok();
    let spec = log_filter_spec(outrig_log.as_deref(), rust_log.as_deref());
    let mut filter = EnvFilter::try_new(spec).unwrap_or_else(|_| EnvFilter::new("info"));
    if let Some(directive) = quiet_rustyline(spec) {
        filter = filter.add_directive(directive);
    }
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

/// Hold rustyline's own records at `warn` unless the filter names it.
///
/// rustyline logs every key it decodes and every buffer it reads at `debug`
/// -- the text typed at the REPL prompt, including what a Ctrl-C discards --
/// and the `log` bridge this subscriber installs would carry all of it to
/// stderr under the documented `RUST_LOG=debug`, where a redirected
/// diagnostic log is the last place a pasted key belongs. A spec that
/// mentions `rustyline` is the opt-in and is left alone.
fn quiet_rustyline(spec: &str) -> Option<Directive> {
    (!spec.contains("rustyline")).then(|| {
        "rustyline=warn"
            .parse()
            .expect("hard-coded rustyline directive must parse")
    })
}

/// Shared preamble for `ls`/`logs`/`discard`/`clean`: `--global-config` as
/// given and a current-thread tokio runtime ready to drive the async
/// `execute` form of each subcommand. No repo config and no cwd: the session
/// root is global-only, so these commands answer the same from any directory.
/// The global config is resolved only where a command reads it.
fn session_cmd_ctx(cli: &Cli) -> Result<(Option<&Path>, tokio::runtime::Runtime)> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    Ok((cli.global_config.as_deref(), runtime))
}

/// Shared preamble for `run`/`mcp`/`build`: the resolved repo config, the
/// resolved global config, and a current-thread tokio runtime. With
/// `require_config` (build), errors if no repo config can be located;
/// otherwise (run/mcp) a missing config falls back to the current directory
/// as repo root, merged over the global config.
fn repo_cmd_ctx(
    cli: &Cli,
    require_config: bool,
) -> Result<(RepoConfig, PathBuf, tokio::runtime::Runtime)> {
    let cwd = crate::paths::current_dir()?;
    let repo_config = if require_config {
        resolve_repo_config(cli.config.as_deref(), &cwd)?
    } else {
        resolve_repo_config_optional(cli.config.as_deref(), &cwd)?
    };
    // After the repo config, so a bad `--config` is the error reported.
    let global_config = resolve_global_config(cli.global_config.as_deref())?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    Ok((repo_config, global_config, runtime))
}

#[cfg(test)]
mod tests {
    use clap::{Parser, error::ErrorKind};

    use super::{Cli, Cmd, EnvFilter, ImageCmd, log_filter_spec, quiet_rustyline};

    /// The `outrig ...` lines of the README's `## Commands` fence, with
    /// trailing comments dropped and an `a|b` alternation in the final word
    /// expanded into one command each.
    fn readme_commands(readme: &str) -> Vec<String> {
        let fence = readme
            .split_once("\n## Commands\n")
            .expect("README has a `## Commands` section")
            .1
            .split_once("```sh\n")
            .expect("`## Commands` opens a ```sh block")
            .1
            .split_once("```")
            .expect("the ```sh block is terminated")
            .0;

        let mut commands = Vec::new();
        for line in fence.lines() {
            let line = line.split_once('#').map_or(line, |(code, _)| code).trim();
            if line.is_empty() {
                continue;
            }
            // Alternations sit in the final word: `outrig ls|logs|discard|clean`.
            let (head, last) = line
                .rsplit_once(' ')
                .expect("every README command is `outrig <verb> ...`");
            commands.extend(last.split('|').map(|verb| format!("{head} {verb}")));
        }
        assert!(!commands.is_empty(), "the README fence listed no commands");
        commands
    }

    /// Every command the crates.io README advertises resolves in the clap
    /// tree. The README is the published front page and the one doc a reader
    /// meets before installing, so a command spelled only there is a promise
    /// the binary breaks -- `outrig design` was exactly that, since `design`
    /// requires a subcommand and the real spelling is `outrig design prompt`.
    ///
    /// A line may legitimately still fail to parse for want of a required
    /// *argument*: the README lists `outrig image inspect` without an image
    /// ref. That is a shape, not a wrong command, so it is the one error kind
    /// this accepts.
    #[test]
    fn every_readme_command_resolves_in_the_clap_tree() {
        for command in readme_commands(include_str!("../../README.md")) {
            if let Err(err) = Cli::try_parse_from(command.split_whitespace()) {
                assert_eq!(
                    err.kind(),
                    ErrorKind::MissingRequiredArgument,
                    "the README documents `{command}`, which clap rejects:\n{err}",
                );
            }
        }
    }

    /// The other half of the claim above, stated directly so a README edit
    /// cannot satisfy the sweep by deleting the line rather than fixing it.
    /// `DisplayHelpOnMissingArgumentOrSubcommand` is clap's way of saying the
    /// command tree stops here: it renders help and exits 2, so bare
    /// `outrig design` is a usage failure rather than a command.
    #[test]
    fn design_requires_the_prompt_subcommand() {
        let err = Cli::try_parse_from(["outrig", "design"]).expect_err("`design` needs a verb");
        assert_eq!(err.kind(), ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand);
        assert_eq!(err.exit_code(), 2, "a usage failure is not a successful run");
        Cli::try_parse_from(["outrig", "design", "prompt"]).expect("`design prompt` parses");
    }

    /// `RUST_LOG=debug` is documented for podman transcripts; it must not also
    /// print what the user types at the line editor.
    #[test]
    fn a_global_filter_holds_rustyline_at_warn() {
        let directive = quiet_rustyline("debug").expect("a global spec gets the directive");
        let filter = EnvFilter::new("debug").add_directive(directive);
        assert!(filter.to_string().contains("rustyline=warn"), "{filter}");
    }

    #[test]
    fn naming_rustyline_in_the_filter_is_the_opt_in() {
        assert!(quiet_rustyline("rustyline=debug").is_none());
        assert!(quiet_rustyline("outrig=trace,rustyline=trace").is_none());
    }

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

    #[test]
    fn image_inspect_defaults_to_local() {
        let cli =
            Cli::try_parse_from(["outrig", "image", "inspect", "rust-dev"]).expect("arg parses");

        let Cmd::Image(args) = cli.cmd else {
            panic!("expected image command");
        };
        let ImageCmd::Inspect { remote, image_ref } = args.cmd else {
            panic!("expected image inspect command");
        };

        assert!(!remote);
        assert_eq!(image_ref, "rust-dev");
    }

    #[test]
    fn image_inspect_remote_arg_parses() {
        let cli = Cli::try_parse_from([
            "outrig",
            "image",
            "inspect",
            "--remote",
            "quay.io/acme/rust-dev:latest",
        ])
        .expect("arg parses");

        let Cmd::Image(args) = cli.cmd else {
            panic!("expected image command");
        };
        let ImageCmd::Inspect { remote, image_ref } = args.cmd else {
            panic!("expected image inspect command");
        };

        assert!(remote);
        assert_eq!(image_ref, "quay.io/acme/rust-dev:latest");
    }
}
