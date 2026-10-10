//! `outrig run-new`: an interactive session whose agent acts by writing Python.
//!
//! The loop lives in `outrig`, and this module and `converse` are the terminal
//! around it: the session is started, driven, watched and stopped through
//! [`outrig::harness`] and nothing else of the loop, as any other owner of one
//! would. It shares none of `run`'s session setup, and `run`'s code is left
//! exactly as it was. Nor does it share `run`'s REPL, which reads a line only
//! between rounds: here what the user types reaches the agent's `user`
//! channel while a round runs, too.
//!
//! **No MCP server and no sidecar starts**: see
//! [`harness::container_spec`], which describes the container.
//!
//! The session is recorded like any other, in the order that keeps
//! `session.json` honest: the record is written once the interpreter is up,
//! carrying the container name the session reports, so a live record always
//! has a container behind it. The container is `outrig-<sid>`, labeled
//! `org.outrig.session=<sid>`, as `run`'s is: the session id reaches the
//! launch through its `LaunchSpec`, and `discard` and `clean` read the name
//! back to tell a live session from a finished one. Until the record is
//! written, the session directory is held by a lock instead, so nothing else
//! starts a session in it meanwhile.
//!
//! It leaves through the session's report: `/quit`, end of input, or a second
//! Ctrl-C at the prompt close the session to new work, stop it, print what the
//! report found, and exit with the status the report decides.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use clap::Parser;
use nix::fcntl::{Flock, FlockArg};
use outrig::config::{Config, EnvSecrets, EventsMode, ImageConfig};
use outrig::error::IoPathExt;
use outrig::harness::{
    self, DEFAULT_DRAIN, ExecutionStatus, Progress, SessionBuilder, ShutdownReport, Step,
    Stopped, Verdict,
};
use outrig::image::{self, ImageTag};
use outrig::LaunchSpec;

use crate::builtin_image;
use crate::cli::session_setup::ProgressSpan;
use crate::error::{OutrigError, Result};
use crate::paths::{RepoConfig, default_session_root, refuse_home_workspace};
use crate::session::{Session, SessionId, SessionStore, resolve_session_root};

mod converse;

#[derive(Debug, Parser)]
pub struct RunNewArgs {
    /// Pick an `[agents.<name>]` block. Defaults to `default-agent` from
    /// config; with neither, the session runs with no agent preamble.
    #[arg(long, value_name = "NAME")]
    pub agent: Option<String>,

    /// Pick a `[models.<name>]` block for this run. Overrides the agent's
    /// `model` and the top-level `default-model`.
    #[arg(long, value_name = "NAME")]
    pub model: Option<String>,

    /// Pick an `[images.<name>]` block. Overrides the agent's `image` and the
    /// top-level `default-image`. Unlike `outrig run`, a local image ref that
    /// no block names is not accepted.
    #[arg(long, value_name = "NAME")]
    pub image: Option<String>,

    /// Write the session into an explicit, already-existing directory. The
    /// session root gets a symlink at `<root>/<sid>` pointing at this path.
    #[arg(long = "session-dir", value_name = "PATH")]
    pub session_dir: Option<PathBuf>,
}

/// Run one `outrig run-new` invocation end to end. Returns the process exit
/// code.
pub async fn execute(
    repo: &RepoConfig,
    global_cfg_path: &Path,
    session_root_flag: Option<&Path>,
    args: &RunNewArgs,
) -> Result<i32> {
    let repo_root = repo.root.clone();
    let span = ProgressSpan::start("loading config");
    let mut cfg = repo.load_for_run(global_cfg_path, args.agent.as_deref(), args.model.as_deref())?;
    span.done("config loaded");
    if !repo.config_path().exists() {
        eprintln!(
            "[outrig] no repo config found; using current directory as workspace ({})",
            repo_root.display()
        );
    }
    // A workspace no config declared is outrig's pick, and outrig never picks
    // the home directory or one above it -- the rule `run` applies.
    if cfg.workspace.declared_host_path().is_none() {
        refuse_home_workspace(&cfg.workspace.resolved_host_path(&repo_root), &repo_root)?;
    }

    // What the session needs from outside, given before it starts: the
    // terminal sees each submission through a subscription of its own, and
    // the event log is a second one when the user asked for it.
    let agent_name = args.agent.clone().or_else(|| cfg.default_agent.clone());
    let mut builder = SessionBuilder::new(cfg.clone(), agent_name.as_deref(), args.model.as_deref())
        .secrets(EnvSecrets)
        .progress(progress_lines());
    if matches!(cfg.events.mode(), EventsMode::Record) {
        builder = builder.record_events();
    }
    let mut shown = builder.subscribe();
    // Before anything is pulled or started, so a config that names no usable
    // model costs nothing.
    builder.check()?;

    let (image_cfg_name, builtin_default) =
        image_config_name(&mut cfg, args.image.as_deref(), agent_name.as_deref())?;
    let image_cfg = cfg.images.get(&image_cfg_name).ok_or_else(|| {
        OutrigError::Configuration(format!(
            "image-config {image_cfg_name:?} does not match any [images.<name>]; `outrig run-new` \
             takes a configured image-config, not a local image ref"
        ))
    })?;

    let store = SessionStore::new(resolve_session_root(
        session_root_flag,
        &cfg,
        &default_session_root(),
    ));
    let sid = SessionId::new();
    // Held until this returns: the directory is this invocation's from here.
    let (_reserved, log_dir) = reserve_session_dir(&store, &sid, args.session_dir.as_deref())?;

    let mut session = Session {
        id: sid.clone(),
        started_at: SystemTime::now(),
        ended_at: None,
        container_name: String::new(),
        sidecar_container_names: Vec::new(),
        image_tag: String::new(),
        image_config_name: Some(image_cfg_name.clone()),
        agent_name: agent_name.clone(),
        working_dir: repo_root.clone(),
        session_dir: PathBuf::new(), // set by `create` below
        exit_code: None,
        link_target: None,
    };
    let started = start(Start {
        builder,
        cfg: &cfg,
        image_cfg_name: &image_cfg_name,
        image_cfg,
        repo_root: &repo_root,
        log_dir,
        record: &mut session,
    })
    .await;

    // Written only now, once the container runs under the name recorded. A
    // session that failed to start is recorded too, and at once finalized, so
    // it is never a live record without a container.
    if let Err(e) = store.create(&sid, args.session_dir.as_deref(), &mut session) {
        if let Ok(running) = started {
            let report = running.shutdown(DEFAULT_DRAIN).await;
            if report.verdict() != Verdict::Clean {
                print_report(&report);
            }
        }
        return Err(e.into());
    }
    let mut running = match started {
        Ok(running) => running,
        Err(e) => {
            let _ = store.finalize(&sid, SystemTime::now(), 1);
            return Err(e);
        }
    };

    eprint!(
        "{}",
        render_banner(&Banner {
            image_config_row: builtin_image::banner_image_config_row(
                &image_cfg_name,
                builtin_default
            ),
            image_tag: &session.image_tag,
            model: running.model(),
            python_version: running.python_version(),
            container_name: running.container_name(),
            session_id: sid.as_str(),
        })
    );

    let conversed = converse::converse(&mut running, &mut shown).await;

    // Said first, since stopping can take a few seconds and a Ctrl-C there
    // does nothing.
    eprintln!("[outrig] closing the session");
    let report = running.shutdown(DEFAULT_DRAIN).await;
    let summary = summarize(&report);
    eprint!("{}", render_report(&summary));
    let outcome = conversed.map(|()| exit_status(&summary));
    let exit = outcome.as_ref().copied().unwrap_or(1);
    if let Err(e) = store.finalize(&sid, SystemTime::now(), exit) {
        tracing::warn!(target: "outrig::cli::run_new", "finalizing session {sid}: {e}");
    }
    outcome
}

/// The exit status a session's report decides: 0 for a clean stop, 2 for a
/// stop that cut some Python off, 3 for one not proven stopped -- which
/// outranks everything, since something may still be running. A session the
/// interpreter's exit closed keeps its documented 1.
fn exit_status(summary: &Summary) -> i32 {
    match summary.verdict {
        Verdict::NotProvenStopped => 3,
        _ if summary.closed.is_some() => 1,
        Verdict::StoppedWithUnknown => 2,
        Verdict::Clean => 0,
        // A verdict this build does not know is not one it can call clean.
        _ => 3,
    }
}

/// The report, on stderr.
fn print_report(report: &ShutdownReport) {
    eprint!("{}", render_report(&summarize(report)));
}

/// What the terminal says of a report.
#[derive(Debug)]
struct Summary {
    verdict: Verdict,
    /// What closed the session, when it was not its owner: the interpreter
    /// exiting.
    closed: Option<String>,
    /// Why it is not proven stopped, when it is not.
    not_stopped: Option<String>,
    executions: Vec<(u64, ExecutionStatus)>,
    last_sequence: u64,
    /// What the terminal's own subscription missed.
    missed: u64,
    /// What the event log did not get.
    log: Option<String>,
}

fn summarize(report: &ShutdownReport) -> Summary {
    Summary {
        verdict: report.verdict(),
        closed: match &report.closed_by {
            harness::ClosedBy::Owner => None,
            by => Some(by.to_string()),
        },
        not_stopped: match &report.stopped {
            Stopped::NotProven { reason, .. } => Some(reason.clone()),
            _ => None,
        },
        executions: report
            .executions
            .iter()
            .map(|execution| (execution.id.get(), execution.status))
            .collect(),
        last_sequence: report.events.last_sequence,
        missed: report.events.missed.first().copied().unwrap_or(0),
        log: report.events.log.as_ref().map(ToString::to_string),
    }
}

/// The report, on stderr: whether the session is stopped, how each execution
/// running at the close ended, and anything its events lost.
fn render_report(summary: &Summary) -> String {
    let mut text = String::from(match summary.verdict {
        Verdict::Clean => "[outrig] session closed: stopped, every outcome known\n",
        Verdict::StoppedWithUnknown => {
            "[outrig] session closed: stopped, but some Python was cut off -- what it did may or \
             may not have happened\n"
        }
        _ => "[outrig] session closed: NOT proven stopped -- something it started may still be \
              running\n",
    });
    if let Some(by) = &summary.closed {
        text.push_str(&format!("[outrig]   closed because {by}\n"));
    }
    if let Some(reason) = &summary.not_stopped {
        text.push_str(&format!("[outrig]   not proven stopped: {reason}\n"));
    }
    for (id, status) in &summary.executions {
        let status = match status {
            ExecutionStatus::Ok => "ok",
            ExecutionStatus::Error => "error",
            _ => "unknown",
        };
        text.push_str(&format!("[outrig]   execution {id}: {status}\n"));
    }
    if summary.missed > 0 {
        text.push_str(&format!(
            "[outrig]   events: {} published; {} were not shown here\n",
            summary.last_sequence, summary.missed
        ));
    }
    if let Some(log) = &summary.log {
        text.push_str(&format!("[outrig] warning: {log}\n"));
    }
    text
}

/// The two lines the start prints for each of its steps, as each begins and
/// ends.
fn progress_lines() -> impl FnMut(Progress) + Send + 'static {
    let mut span = None;
    move |progress| match progress {
        Progress::Started(step) => {
            span = Some(ProgressSpan::start(match step {
                Step::Container => "starting container",
                Step::Python => "starting python",
                _ => "starting",
            }));
        }
        Progress::Finished(step) => {
            if let Some(span) = span.take() {
                span.done(match step {
                    Step::Container => "container ready",
                    Step::Python => "python ready",
                    _ => "ready",
                });
            }
        }
        _ => {}
    }
}

/// The image-config the session runs: `--image`, the agent's, `default-image`,
/// or outrig's built-in default, in that order. Also says whether it was the
/// built-in, for the startup line. The built-in comes without its servers and
/// sidecars, which a session that starts none has no use for.
fn image_config_name(
    cfg: &mut Config,
    image_flag: Option<&str>,
    agent: Option<&str>,
) -> Result<(String, bool)> {
    let chosen = image_flag
        .or_else(|| agent.and_then(|name| cfg.agents.get(name)?.image.as_deref()))
        .or(cfg.default_image.as_deref())
        .map(str::to_string);
    // One of the built-in's own names means the built-in, as it does to
    // `outrig run` and `outrig build`, unless the config declares it.
    if let Some(name) = chosen
        .as_ref()
        .filter(|name| !builtin_image::is_reserved(name) || cfg.images.contains_key(*name))
    {
        return Ok((name.clone(), false));
    }
    let injection = builtin_image::inject_primary(cfg);
    if chosen.is_none() && injection.applied {
        eprintln!(
            "[outrig] no --image, agent image, or default-image configured; using outrig's \
             built-in default"
        );
    }
    for note in &injection.notes {
        eprintln!("[outrig] {note}");
    }
    let name = match chosen {
        Some(name) => name,
        // A reserved name other than `[images.outrig-default]` is declared, so
        // injection was vetoed and there is nothing to fall back to. Name the
        // block that did it -- the one the note above names.
        None => injection.resolved.map(str::to_string).map_err(|block| {
            OutrigError::Configuration(format!(
                "no --image or default-image configured, and {block} shadows outrig's \
                 built-in default, leaving no [images.outrig-default] to fall back to"
            ))
        })?,
    };
    Ok((name, injection.applied))
}

/// Take the session's directory for this invocation, and create its `logs/`,
/// which the launch writes to before the record exists.
///
/// The record is written only once the interpreter is up, so until then the
/// directory is claimed by an exclusive lock on the directory itself. Without
/// it, a second `run-new --session-dir` naming the same directory would pass
/// every check while the first is still starting, and whichever failed would
/// write its record into the other's directory. The lock is not waited for:
/// a second invocation fails at once, having written nothing. The kernel
/// drops it with the process, so a killed invocation leaves nothing stale.
///
/// `explicit` is checked here rather than when the record is written, so a
/// bad `--session-dir` fails before anything starts.
fn reserve_session_dir(
    store: &SessionStore,
    sid: &SessionId,
    explicit: Option<&Path>,
) -> Result<(Flock<File>, PathBuf)> {
    let dir = match explicit {
        Some(dir) if !dir.is_dir() => {
            return Err(OutrigError::Configuration(format!(
                "--session-dir {} is not an existing directory (create it first or omit the flag)",
                dir.display()
            ))
            .into());
        }
        Some(dir) => dir.to_path_buf(),
        None => {
            let dir = store.symlink_path(sid);
            std::fs::create_dir_all(&dir).path_ctx("create directory", &dir)?;
            dir
        }
    };
    let handle = File::open(&dir).path_ctx("open", &dir)?;
    let reserved = Flock::lock(handle, FlockArg::LockExclusiveNonblock).map_err(|(_, e)| {
        OutrigError::Configuration(format!(
            "session directory {} is in use by another `outrig run-new` ({e})",
            dir.display()
        ))
    })?;
    // Checked under the lock, so two invocations cannot both find it free.
    if dir.join("session.json").exists() {
        return Err(OutrigError::Configuration(format!(
            "--session-dir {} already contains session.json",
            dir.display()
        ))
        .into());
    }
    let log_dir = dir.join("logs");
    std::fs::create_dir_all(&log_dir).path_ctx("create directory", &log_dir)?;
    Ok((reserved, log_dir))
}

struct Start<'a> {
    builder: SessionBuilder,
    cfg: &'a Config,
    image_cfg_name: &'a str,
    image_cfg: &'a ImageConfig,
    repo_root: &'a Path,
    log_dir: PathBuf,
    /// Filled in with the image tag and the container name as each is learned.
    record: &'a mut Session,
}

/// Ensure the image, then start the session in it. On failure nothing is left
/// running.
async fn start(args: Start<'_>) -> Result<harness::Session> {
    let Start {
        builder,
        cfg,
        image_cfg_name,
        image_cfg,
        repo_root,
        log_dir,
        record,
    } = args;

    // Ensured here, under the image-config's name as `run` and `build` do,
    // rather than left to `launch`: it never pulls an `image-name`, and it
    // would build a Dockerfile under a nameless tag of its own.
    let span = ProgressSpan::start(format!("ensuring image for {image_cfg_name}"));
    let tag = image::compute_tag_for(image_cfg_name, image_cfg, repo_root).await?;
    let image =
        image::ensure_tagged_image_for(image_cfg_name, image_cfg, repo_root, &tag, false, None)
            .await?;
    span.done(format!(
        "image ready: {} ({})",
        image.tag,
        if image.cache_hit { "cache hit" } else { "built" }
    ));
    record.image_tag = image.tag.to_string();

    let (spec, skipped) =
        launch_spec(cfg, image_cfg_name, &image.tag, repo_root, log_dir, &record.id).await?;
    if !skipped.is_empty() {
        eprintln!(
            "[outrig] run-new starts no MCP servers; not started: {}",
            skipped.join(", ")
        );
    }

    let session = builder.container(spec).start().await?;
    record.container_name = session.container_name().to_string();
    Ok(session)
}

/// The launch for `image_cfg_name`, running the already-ensured `tag`, with no
/// MCP server and no sidecar in it -- and the names of the servers left out.
/// Its container is named for `sid` and carries it as `org.outrig.session`, as
/// `run`'s is.
///
/// The image-config is replaced by one naming `tag`, keeping only its
/// security, so the image that runs is the one the session records and
/// `launch` has nothing to build.
async fn launch_spec(
    cfg: &Config,
    image_cfg_name: &str,
    tag: &ImageTag,
    repo_root: &Path,
    log_dir: PathBuf,
    sid: &SessionId,
) -> Result<(LaunchSpec, Vec<String>)> {
    let container =
        harness::container_spec(cfg, image_cfg_name, Some(tag), repo_root, log_dir).await?;
    Ok((
        container.spec.with_session_id(sid.as_str()),
        container.left_out,
    ))
}

struct Banner<'a> {
    image_config_row: String,
    image_tag: &'a str,
    model: &'a str,
    python_version: &'a str,
    container_name: &'a str,
    session_id: &'a str,
}

fn render_banner(banner: &Banner<'_>) -> String {
    format!(
        "{}\n\
         [outrig] image:         {}\n\
         [outrig] model:         {}\n\
         [outrig] python {} ready in {}\n\
         [outrig] session id: {}   (Ctrl-D to exit, /help for slash commands)\n",
        banner.image_config_row,
        banner.image_tag,
        banner.model,
        banner.python_version,
        banner.container_name,
        banner.session_id,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use outrig::EmbeddedMcpPolicy;

    /// A config whose image carries a primary server holding a secret and a
    /// sidecar-hosted server, the two ways an MCP server could start.
    const WITH_SERVERS: &str = r#"
[images.primary]
dockerfile = "Dockerfile"
context    = "."

[images.primary.security]
cap-add = ["NET_ADMIN"]

[images.tools]
image-name = "docker.io/library/alpine:latest"

[sidecars.tools]
image = "tools"

[images.primary.mcp]
leaky = { command = ["/nonexistent-mcp"], env = { TOKEN = "${OUTRIG_TEST_RUN_NEW_UNSET_SECRET}" } }
fs    = { command = ["mcp-fs"], sidecar = "tools" }
"#;

    /// The policy, asserted on what is launched rather than on what Python can
    /// read: the spec carries no server and no sidecar, ignores the image's
    /// label, and names what it left out. Were `leaky` started, resolving its
    /// unset `${..}` alone would fail the launch. The image-config, here a
    /// Dockerfile, is pinned to the tag already ensured and keeps its security.
    /// The spec carries the session id, so the container is named and labeled
    /// for it.
    #[tokio::test]
    async fn the_launch_starts_no_mcp_server_and_no_sidecar() {
        let cfg = Config::load_from_str(WITH_SERVERS).expect("config parses");
        cfg.validate(None).expect("config validates");
        let repo = tempfile::tempdir().expect("a repo dir");

        let tag = ImageTag::new("docker.io/library/alpine:latest");
        let sid = SessionId("20260925T000000-abcd".to_string());
        let (spec, skipped) = launch_spec(
            &cfg,
            "primary",
            &tag,
            repo.path(),
            repo.path().join("logs"),
            &sid,
        )
        .await
        .expect("lowers without touching podman");

        assert_eq!(spec.session_id.as_deref(), Some("20260925T000000-abcd"));
        assert!(spec.mcp.is_empty(), "{:?}", spec.mcp.keys());
        assert!(spec.sidecars.is_empty(), "a sidecar would start");
        assert_eq!(spec.embedded_mcp_policy, EmbeddedMcpPolicy::Ignore);
        assert_eq!(skipped, ["fs", "leaky"]);
        assert_eq!(
            format!("{:?}", spec.security),
            format!(
                "{:?}",
                outrig::SecuritySpec::from(&cfg.images["primary"].security)
            ),
        );
        assert!(
            cfg.images["primary"].mcp.contains_key("leaky") && cfg.sidecars.contains_key("tools"),
            "the session's own config is left whole"
        );
    }

    /// A summary of a report that `verdict` reads as, and nothing else.
    fn summary(verdict: Verdict) -> Summary {
        Summary {
            verdict,
            closed: None,
            not_stopped: None,
            executions: Vec::new(),
            last_sequence: 0,
            missed: 0,
            log: None,
        }
    }

    /// What a stop found decides the status, and one not proven stopped
    /// outranks everything. A report that reads not proven stopped cannot be
    /// had from podman on demand, so this is where 3 is forced.
    #[test]
    fn the_report_decides_the_exit_status() {
        assert_eq!(exit_status(&summary(Verdict::Clean)), 0);
        assert_eq!(exit_status(&summary(Verdict::StoppedWithUnknown)), 2);
        assert_eq!(exit_status(&summary(Verdict::NotProvenStopped)), 3);
        let died = |verdict| Summary {
            closed: Some("the Python interpreter exited (exit status: 1)".to_string()),
            ..summary(verdict)
        };
        assert_eq!(exit_status(&died(Verdict::Clean)), 1);
        assert_eq!(exit_status(&died(Verdict::StoppedWithUnknown)), 1);
        assert_eq!(exit_status(&died(Verdict::NotProvenStopped)), 3);
    }

    /// What the terminal says of each kind of report.
    #[test]
    fn the_report_says_what_stopped_and_what_was_cut_off() {
        assert_eq!(
            render_report(&Summary {
                last_sequence: 40,
                ..summary(Verdict::Clean)
            }),
            "[outrig] session closed: stopped, every outcome known\n"
        );

        let cut = render_report(&Summary {
            executions: vec![(7, ExecutionStatus::Unknown), (9, ExecutionStatus::Ok)],
            last_sequence: 80,
            missed: 3,
            ..summary(Verdict::StoppedWithUnknown)
        });
        assert!(cut.starts_with("[outrig] session closed: stopped, but some Python was cut off"));
        assert!(cut.contains("[outrig]   execution 7: unknown\n"), "{cut}");
        assert!(cut.contains("[outrig]   execution 9: ok\n"), "{cut}");
        assert!(cut.contains("80 published; 3 were not shown here"), "{cut}");

        let stuck = render_report(&Summary {
            closed: Some("the Python interpreter exited (exit status: 1)".to_string()),
            not_stopped: Some("the primary container: timed out".to_string()),
            log: Some("2 agent event(s) could not be written".to_string()),
            ..summary(Verdict::NotProvenStopped)
        });
        assert!(stuck.starts_with("[outrig] session closed: NOT proven stopped"), "{stuck}");
        assert!(
            stuck.contains("closed because the Python interpreter exited (exit status: 1)"),
            "{stuck}"
        );
        assert!(stuck.contains("not proven stopped: the primary container: timed out"));
        assert!(stuck.contains("[outrig] warning: 2 agent event(s)"), "{stuck}");
    }

    /// What a person needs before typing: the model, and that Python is ready
    /// and which one.
    #[test]
    fn the_banner_names_the_model_and_the_python() {
        let banner = render_banner(&Banner {
            image_config_row: builtin_image::banner_image_config_row("outrig-default", true),
            image_tag: "docker.io/library/alpine:latest",
            model: "sonnet",
            python_version: "3.13.15",
            container_name: "outrig-20260925T000000-abcd",
            session_id: "20260925T000000-abcd",
        });
        assert_eq!(
            banner,
            "[outrig] image-config:  outrig-default (built-in default)\n\
             [outrig] image:         docker.io/library/alpine:latest\n\
             [outrig] model:         sonnet\n\
             [outrig] python 3.13.15 ready in outrig-20260925T000000-abcd\n\
             [outrig] session id: 20260925T000000-abcd   (Ctrl-D to exit, /help for slash commands)\n"
        );
    }

    #[test]
    fn a_session_dir_must_exist_and_hold_no_record() {
        let root = tempfile::tempdir().expect("a root");
        let store = SessionStore::new(root.path().to_path_buf());
        let sid = SessionId::from("sid".to_string());
        let missing = root.path().join("missing");
        let err = reserve_session_dir(&store, &sid, Some(&missing))
            .expect_err("a missing dir is refused")
            .to_string();
        assert!(err.contains("is not an existing directory"), "{err}");

        let used = tempfile::tempdir().expect("a used dir");
        std::fs::write(used.path().join("session.json"), "{}").expect("a record");
        let err = reserve_session_dir(&store, &sid, Some(used.path()))
            .expect_err("a dir with a record is refused")
            .to_string();
        assert!(err.contains("already contains session.json"), "{err}");

        let (_reserved, log_dir) =
            reserve_session_dir(&store, &sid, None).expect("the default layout");
        assert_eq!(log_dir, root.path().join("sid").join("logs"));
        assert!(log_dir.is_dir());
    }

    /// Two invocations naming one directory: the first holds it until it
    /// returns, and the second is refused before it creates or writes anything
    /// there. Once the first lets go, the directory can be taken again.
    #[test]
    fn a_session_dir_is_held_by_one_invocation_at_a_time() {
        let root = tempfile::tempdir().expect("a root");
        let store = SessionStore::new(root.path().to_path_buf());
        let dir = tempfile::tempdir().expect("a session dir");
        let first = SessionId::from("first".to_string());
        let second = SessionId::from("second".to_string());

        let held = reserve_session_dir(&store, &first, Some(dir.path())).expect("free");
        std::fs::remove_dir(dir.path().join("logs")).expect("the first made logs/");
        let err = reserve_session_dir(&store, &second, Some(dir.path()))
            .expect_err("held by the first")
            .to_string();
        assert!(err.contains("is in use by another `outrig run-new`"), "{err}");
        assert!(
            !dir.path().join("logs").exists(),
            "the refused one made nothing there"
        );

        drop(held);
        reserve_session_dir(&store, &second, Some(dir.path())).expect("free again");
    }
}
