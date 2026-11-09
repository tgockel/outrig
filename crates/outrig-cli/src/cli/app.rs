use clap::{ArgAction, Args, Parser, Subcommand};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::filter::Directive;

use crate::cli::build::{self, BuildArgs};
use crate::cli::clean::{self, CleanArgs};
use crate::cli::design_prompt::{self, DesignArgs, DesignCommand};
use crate::cli::discard::{self, DiscardArgs};
use crate::cli::log_divert::LogWriter;
use crate::cli::logs::{self, LogsArgs};
use crate::cli::ls::{self, LsArgs};
use crate::cli::mcp::{self, McpArgs};
use crate::cli::mcp_self as mcp_self_cli;
use crate::cli::program;
use crate::cli::run::{self, RunArgs};
use crate::cli::run_new::{self, RunNewArgs};
use crate::error::{CliError, OutrigError, Result};
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
    ///
    /// `outrig run-legacy` is the same command. A later release makes `outrig run` what `outrig
    /// run-new` is today.
    Run(RunArgs),
    /// Start an interactive agent session.
    ///
    /// Another name for `outrig run`.
    RunLegacy(RunArgs),
    /// Start an interactive session whose agent acts by writing Python, or run a Python program
    /// on the host (preview).
    ///
    /// The model has one tool, which runs Python in a persistent interpreter
    /// inside the session's container. Names it binds stay bound from one
    /// message to the next. No MCP server and no sidecar is started.
    ///
    /// With a script, it runs the script on the host under the Python OutRig carries, as
    /// `python3 <script> [args...]` would run it, with `import outrig` available. That starts no
    /// container and calls no model until the program enters a session with `outrig.Session`.
    /// The program chooses its own session's inputs, so `--agent`, `--model`, `--image` and
    /// `--session-dir` are refused before the script, while `--config`, `--global-config`,
    /// `--session-root` and `-v` apply as for every command, and `--grace` to the program alone.
    /// Everything after the script is the program's own.
    ///
    /// `outrig run` is unchanged by this command, and `outrig run-legacy` is
    /// another name for it.
    RunNew(RunNewArgs),
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
        /// Overwrite Dockerfile and image.toml if present; preserve an existing README.md.
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
        let code = match dispatch(&cli) {
            Ok(code) => code,
            Err(e) => {
                // A signal announced itself as it landed, and the terminal it
                // would be reported to may be gone.
                if !matches!(e, CliError::Interrupted(_)) {
                    super::eprint_bounded(&format!("error: {e}\n"));
                }
                e.exit_code()
            }
        };
        ExitCode::from(code.clamp(0, 255) as u8)
    })
}

/// Drive a session command, then shut its runtime down without waiting on
/// blocking work. A REPL or a stdio MCP transport can leave a blocking stdin
/// read in flight that returns only when the other end writes or closes the
/// pipe, and dropping the runtime normally waits for it -- after a signal, a
/// stopped container, or a second Ctrl-C, the session was torn down and the
/// process sat there until someone pressed Enter. So can `run-new`'s question
/// about a binding a repository declared, which a signal answers by ending the
/// start that asked it, and a build's output written to a stderr nobody reads.
/// Every session has finished its own teardown by the time `fut` resolves.
///
/// The runtime lives in a [`ShutdownOnDrop`] guard rather than a local
/// binding so that a panic out of `fut` gets the same treatment as a normal
/// return. `block_on` propagates such a panic by unwinding out of this
/// function, and a bare local would then be dropped the ordinary way --
/// `Runtime::drop` waits, with no timeout, for that same blocking read, which
/// is the hang this function exists to avoid (#517). The guard's `Drop` runs
/// on that unwind path and calls `shutdown_background()` just as the return
/// path below does, so either way out stops waiting on blocking work.
fn block_on_session(
    runtime: tokio::runtime::Runtime,
    fut: impl Future<Output = Result<i32>>,
) -> Result<i32> {
    let mut guard = ShutdownOnDrop(Some(runtime));
    let result = guard
        .0
        .as_ref()
        .expect("guard holds the runtime until shutdown")
        .block_on(fut);
    guard
        .0
        .take()
        .expect("guard holds the runtime until shutdown")
        .shutdown_background();
    result
}

/// Holds a [`tokio::runtime::Runtime`] and, if it is still there when this
/// drops, shuts it down in the background instead of letting `Runtime::drop`
/// wait on blocking work. See [`block_on_session`].
struct ShutdownOnDrop(Option<tokio::runtime::Runtime>);

impl Drop for ShutdownOnDrop {
    fn drop(&mut self) {
        if let Some(runtime) = self.0.take() {
            runtime.shutdown_background();
        }
    }
}

/// The subcommand `--config` is not read by, spelled as `outrig <name>`,
/// paired with the line that says what it reads instead. `None` is one of the
/// commands that do read the flag: `run` (and `run-legacy`), `run-new`, `mcp`
/// as a server, and `build`.
fn config_ignored_by(cmd: &Cmd) -> Option<(&'static str, &'static str)> {
    /// `image add`, which finds the repo config the way a command without
    /// `--config` finds one: `repo::resolve_or_bootstrap` walks up from cwd.
    const WALKS_UP: &str = "it writes the repo config it finds by walking up from the current \
         directory, so run it from the repo you mean";
    /// `init`, which does not walk: `init::run_with` sets up the directory it
    /// was run in, whatever an ancestor holds.
    const CWD_ONLY: &str = "it sets up the current directory, writing \
         .agents/outrig/config.toml there";
    const NO_REPO_CONFIG: &str = "it reads no repo config";
    const GLOBAL_ONLY: &str = "it writes the global config, which --global-config names";

    match cmd {
        Cmd::Run(_) | Cmd::RunLegacy(_) | Cmd::RunNew(_) | Cmd::Build(_) => None,
        // Only the server reads a config; `mcp self` describes outrig itself.
        Cmd::Mcp(args) => args
            .is_self_description()
            .then_some(("mcp self", NO_REPO_CONFIG)),
        // Matched through to the leaf, so a subcommand added beside these
        // has to say its own spelling rather than inherit a wrong one.
        Cmd::Design(args) => Some(match &args.cmd {
            DesignCommand::Prompt(_) => ("design prompt", NO_REPO_CONFIG),
        }),
        Cmd::Config(args) => Some(match &args.cmd {
            ConfigCmd::Init { .. } => ("config init", GLOBAL_ONLY),
        }),
        Cmd::Init { .. } => Some(("init", CWD_ONLY)),
        Cmd::Image(args) => Some(match &args.cmd {
            ImageCmd::Add { .. } => ("image add", WALKS_UP),
            ImageCmd::Init { .. } => ("image init", NO_REPO_CONFIG),
            ImageCmd::Build { .. } => ("image build", NO_REPO_CONFIG),
            ImageCmd::Inspect { .. } => ("image inspect", NO_REPO_CONFIG),
        }),
        Cmd::Ls(_) => Some(("ls", NO_REPO_CONFIG)),
        Cmd::Logs(_) => Some(("logs", NO_REPO_CONFIG)),
        Cmd::Discard(_) => Some(("discard", NO_REPO_CONFIG)),
        Cmd::Clean(_) => Some(("clean", NO_REPO_CONFIG)),
    }
}

/// Stop a command that would ignore the `--config` it was given.
///
/// The flag is global, so every subcommand accepts it, but only `run`,
/// `run-new`, `mcp`, and `build` read one. The rest used to take it and go on
/// as if it were absent, which for the commands that write a config meant
/// writing somewhere else: `outrig --config ci/outrig.toml image add foo` put
/// `[images.foo]` into the repo's own `.agents/outrig/config.toml`, and the
/// next `outrig --config ci/outrig.toml build --image foo` read the named file
/// and reported no such image (#412). Refusing makes the flag mean one thing --
/// the file the command reads -- and says which commands that is, rather than
/// leaving the mistake to be noticed a command later.
fn refuse_ignored_config(cli: &Cli) -> Result<()> {
    let (Some(path), Some((name, reason))) = (&cli.config, config_ignored_by(&cli.cmd)) else {
        return Ok(());
    };
    Err(OutrigError::Configuration(format!(
        "`outrig {name}` does not read --config {path}: {reason}\n\
         help: only `run`, `run-new`, `mcp`, and `build` read the repo config --config names",
        path = path.display(),
    ))
    .into())
}

fn dispatch(cli: &Cli) -> Result<i32> {
    refuse_ignored_config(cli)?;
    match &cli.cmd {
        Cmd::Run(args) | Cmd::RunLegacy(args) => {
            let (repo_config, global_config, runtime) = repo_cmd_ctx(cli, false)?;
            block_on_session(
                runtime,
                run::execute(
                    &repo_config,
                    global_config.as_deref(),
                    cli.session_root.as_deref(),
                    args,
                    cli.verbose,
                ),
            )
        }
        Cmd::RunNew(args) if !args.program.is_empty() => {
            let (repo_config, global_config, runtime) = repo_cmd_ctx(cli, false)?;
            runtime.block_on(program::execute(
                &repo_config,
                global_config.as_deref(),
                cli.session_root.as_deref(),
                args.grace,
                &args.program,
            ))
        }
        Cmd::RunNew(args) => {
            let (repo_config, global_config, runtime) = repo_cmd_ctx(cli, false)?;
            block_on_session(
                runtime,
                run_new::execute(
                    &repo_config,
                    global_config.as_deref(),
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
                    global_config.as_deref(),
                    cli.session_root.as_deref(),
                    args,
                    cli.verbose,
                ),
            )
        }
        Cmd::Design(args) => design_prompt::execute(args),
        Cmd::Build(args) => {
            let (repo_config, global_config, runtime) = repo_cmd_ctx(cli, true)?;
            runtime.block_on(build::execute(&repo_config, global_config.as_deref(), args))
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

/// The filter in force when neither `OUTRIG_LOG` nor `RUST_LOG` is set: outrig's own records at
/// `info`, with rig's agent-loop instrumentation (a line per turn, the tool arguments and result,
/// `Agent run finished`) held at `warn` so it does not land between the prompt and the reply.
/// rig 0.45 raises those lines from two crates, `rig_agent`'s engine and `rig_core`. Naming
/// either crate in either variable is the opt-in.
const DEFAULT_LOG_FILTER: &str = "info,rig_core=warn,rig_agent=warn";

fn init_tracing(verbose: u8) {
    let outrig_log = std::env::var("OUTRIG_LOG").ok();
    let rust_log = std::env::var("RUST_LOG").ok();
    let spec = log_filter_spec(outrig_log.as_deref(), rust_log.as_deref());
    let mut filter = EnvFilter::try_new(spec).unwrap_or_else(|_| EnvFilter::new(DEFAULT_LOG_FILTER));
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
        // stderr through `crate::notice`, so a record raised while `outrig
        // run`'s prompt is up waits for the read to end rather than landing on
        // the prompt's row -- unless `run-new`'s screen holds the terminal and
        // has taken the records for itself.
        .with_writer(LogWriter::global().clone())
        // Its default, true, reports an event that could not be written with an
        // `eprintln!` of its own, which panics when stderr's reader has gone:
        // one log line would end the command with a 101.
        .log_internal_errors(false)
        .init();
    if verbose >= 2 {
        tracing::trace!(target: "outrig", "verbose tracing enabled");
    }
}

fn log_filter_spec<'a>(outrig_log: Option<&'a str>, rust_log: Option<&'a str>) -> &'a str {
    outrig_log.or(rust_log).unwrap_or(DEFAULT_LOG_FILTER)
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

/// Shared preamble for `run`/`run-new`/`mcp`/`build`: the resolved repo config,
/// the resolved global config (`None` with nowhere to look for one), and a
/// current-thread tokio runtime. With `require_config` (build), errors if no
/// repo config can be located; otherwise (run/mcp) a missing config falls back
/// to the current directory as repo root, merged over the global config.
fn repo_cmd_ctx(
    cli: &Cli,
    require_config: bool,
) -> Result<(RepoConfig, Option<PathBuf>, tokio::runtime::Runtime)> {
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

    use super::{Cli, Cmd, EnvFilter, ImageCmd, config_ignored_by, log_filter_spec, quiet_rustyline};

    /// The guard in [`super::block_on_session`] has to catch the unwind path,
    /// not just the return path: a panic out of the session future must
    /// reach the caller without first waiting on blocking work still parked
    /// in the runtime's blocking pool (#517). `park_rx.recv()` never returns
    /// on its own -- `_park_tx` stays alive for the whole test -- so an
    /// ordinary `Runtime::drop` here would hang, and `recv_timeout` below
    /// would time out instead of observing the unwind.
    #[test]
    fn block_on_session_unwind_does_not_wait_on_blocking_work() {
        use std::sync::mpsc;
        use std::time::Duration;

        let (_park_tx, park_rx) = mpsc::channel::<()>();
        let (unwound_tx, unwound_rx) = mpsc::channel();

        std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("runtime builds");
            runtime.spawn_blocking(move || {
                let _ = park_rx.recv();
            });

            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                super::block_on_session(runtime, async { panic!("injected session panic") })
            }));
            assert!(
                result.is_err(),
                "the injected panic should propagate out of block_on_session"
            );
            let _ = unwound_tx.send(());
        });

        unwound_rx
            .recv_timeout(Duration::from_secs(2))
            .expect(
                "the unwind should reach the caller without waiting on the parked blocking task",
            );
    }

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

    /// The program's arguments as `outrig run-new` parses `argv`, or clap's refusal.
    fn program(argv: &[&str]) -> Result<Vec<String>, clap::Error> {
        let cli = Cli::try_parse_from(argv)?;
        let Cmd::RunNew(args) = cli.cmd else {
            panic!("{argv:?} is not `run-new`");
        };
        assert_eq!((cli.verbose, cli.config), (0, None), "{argv:?}");
        Ok(args
            .program
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect())
    }

    /// Everything after the script is the program's -- OutRig's own flags, `--`, `--help` --
    /// while before it OutRig's options apply, and a session flag there is refused by name.
    #[test]
    fn everything_after_the_script_is_the_programs() {
        assert_eq!(
            program(&[
                "outrig", "run-new", "worker.py", "--agent", "x", "-v", "--config", "c", "--",
                "--help", ""
            ])
            .unwrap(),
            ["worker.py", "--agent", "x", "-v", "--config", "c", "--", "--help", ""]
        );
        assert_eq!(
            program(&["outrig", "run-new", "--", "-dash.py", "y"]).unwrap(),
            ["-dash.py", "y"]
        );
        assert!(program(&["outrig", "run-new"]).unwrap().is_empty());

        let cli =
            Cli::try_parse_from(["outrig", "-v", "run-new", "--config", "c", "w.py"]).unwrap();
        assert_eq!((cli.verbose, cli.config.as_deref()), (1, Some("c".as_ref())));

        for (flag, value) in [("--agent", "x"), ("--model", "m"), ("--session-dir", "d")] {
            let err = program(&["outrig", "run-new", flag, value, "worker.py"]).unwrap_err();
            assert_eq!(err.kind(), ErrorKind::ArgumentConflict, "{flag}: {err}");
            let text = err.to_string();
            assert!(
                text.starts_with(&format!("error: the argument '{flag} <")),
                "{flag}: {text}"
            );
        }
        let err = program(&["outrig", "run-new", "--unknown", "worker.py"]).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::UnknownArgument, "{err}");
        let help = program(&["outrig", "run-new", "--help"]).unwrap_err();
        assert_eq!(help.kind(), ErrorKind::DisplayHelp);
    }

    /// `--grace` is the program's alone: `run-new` refuses it without a script, and `run` and
    /// `run-legacy` never take it.
    #[test]
    fn grace_belongs_to_the_script_form() {
        let cli = Cli::try_parse_from(["outrig", "run-new", "--grace", "7", "w.py"]).unwrap();
        let Cmd::RunNew(args) = cli.cmd else {
            panic!("not `run-new`");
        };
        assert_eq!(args.grace, Some(7));

        let err = Cli::try_parse_from(["outrig", "run-new", "--grace", "7"]).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::MissingRequiredArgument, "{err}");
        for verb in ["run", "run-legacy"] {
            let err = Cli::try_parse_from(["outrig", verb, "--grace", "7"]).unwrap_err();
            assert_eq!(err.kind(), ErrorKind::UnknownArgument, "{verb}: {err}");
        }
    }

    /// A script given to `run` or `run-legacy` is a usage error that names `run-new`, whatever
    /// follows the script and whatever flags precede it.
    #[test]
    fn a_script_given_to_run_is_refused_naming_run_new() {
        for argv in [
            &["outrig", "run", "worker.py"][..],
            &["outrig", "run", "--agent", "x", "worker.py", "./workspace", "--help"],
            &["outrig", "run", "--", "worker.py"],
            &["outrig", "run-legacy", "worker.py", "a"],
        ] {
            let err = Cli::try_parse_from(argv).unwrap_err();
            assert_eq!(err.kind(), ErrorKind::ValueValidation, "{argv:?}: {err}");
            assert_eq!(err.exit_code(), 2, "{argv:?}");
            assert!(
                err.to_string().contains("`outrig run-new worker.py`"),
                "{argv:?}: {err}"
            );
        }
    }

    /// A script path that is not UTF-8 is a path `run-new` runs, so `run` refuses it with the
    /// same hint rather than a bare conversion error.
    #[test]
    fn a_script_path_that_is_not_utf8_is_refused_naming_run_new() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt as _;

        let script = OsString::from_vec(b"w\xffrk.py".to_vec());
        for verb in ["run", "run-legacy"] {
            let argv = ["outrig".into(), verb.into(), script.clone()];
            let err = Cli::try_parse_from(argv).unwrap_err();
            assert_eq!(err.kind(), ErrorKind::ValueValidation, "{verb}: {err}");
            assert_eq!(err.exit_code(), 2, "{verb}");
            assert!(
                err.to_string().contains("`outrig run-new w\u{fffd}rk.py`"),
                "{verb}: {err}"
            );
        }
        let ran = ["outrig".into(), "run-new".into(), script];
        let Cmd::RunNew(args) = Cli::try_parse_from(ran).unwrap().cmd else {
            panic!("not `run-new`");
        };
        assert_eq!(args.program.len(), 1);
    }

    /// `run-legacy` is the interactive session alone: it takes the session's flags.
    #[test]
    fn run_legacy_takes_the_sessions_flags() {
        let cli = Cli::try_parse_from(["outrig", "run-legacy", "--agent", "x"]).unwrap();
        let Cmd::RunLegacy(args) = cli.cmd else {
            panic!("not `run-legacy`");
        };
        assert_eq!(args.agent.as_deref(), Some("x"));
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
    fn log_filter_defaults_to_info_with_rig_held_at_warn() {
        assert_eq!(
            log_filter_spec(None, None),
            "info,rig_core=warn,rig_agent=warn"
        );
    }

    /// rig's `info!` calls are the noise -- rig 0.45 raises the per-turn lines from `rig_agent`'s
    /// engine, the rest from `rig_core` -- and its `warn!`s and anything outrig logs stay, and
    /// `OUTRIG_LOG=debug` brings rig's lines back.
    #[test]
    fn the_default_filter_quiets_rigs_info_only() {
        use tracing_subscriber::filter::LevelFilter;
        let default = EnvFilter::new(log_filter_spec(None, None));
        assert_eq!(default.max_level_hint(), Some(LevelFilter::INFO));
        let rendered = default.to_string();
        assert!(rendered.contains("rig_core=warn"), "{rendered}");
        assert!(rendered.contains("rig_agent=warn"), "{rendered}");
        let opted_in = EnvFilter::new(log_filter_spec(Some("debug"), None));
        assert_eq!(opted_in.max_level_hint(), Some(LevelFilter::DEBUG));
        assert!(!opted_in.to_string().contains("rig_"));
    }

    /// `--config` means the repo config, and `mcp` is the one subcommand that
    /// reads it in one mode and not in the other: serving a repo reads it,
    /// `mcp self` describes outrig itself and has nothing to read it for
    /// (#412).
    #[test]
    fn mcp_reads_config_as_a_server_but_not_as_self() {
        let serving = Cli::try_parse_from(["outrig", "mcp"]).expect("`mcp` parses");
        assert_eq!(config_ignored_by(&serving.cmd), None);

        let described = Cli::try_parse_from(["outrig", "mcp", "self"]).expect("`mcp self` parses");
        let (name, _) = config_ignored_by(&described.cmd).expect("`mcp self` reads no config");
        assert_eq!(name, "mcp self");
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
