//! `outrig run-new`: an interactive session whose agent acts by writing Python.
//!
//! The loop lives in `outrig` ([`PythonAgent`]); this module is the terminal
//! around it. It shares [`Repl`] with `outrig run` and none of `run`'s session
//! setup, because only [`Outrig::launch`] mounts the interpreter's payload --
//! so the session is launched through the library facade, and `run`'s code is
//! left exactly as it was.
//!
//! **No MCP server and no sidecar starts.** The model's one tool submits
//! Python, so nothing could call them. A primary-placed server would also run
//! in the interpreter's container as the same user, holding whatever secrets
//! its `env` resolved -- readable from Python wherever `/proc` allows it. Not
//! handing the model a tool does not put a credential out of reach; not
//! starting the server does.
//!
//! The session is recorded like any other, in the order that keeps
//! `session.json` honest: the record is written once the interpreter is up,
//! carrying the container name [`PythonAgent`] reports. `Outrig::launch`
//! chooses that name itself, and `discard` and `clean` read it to tell a live
//! session from a finished one, so a record written earlier would have to hold
//! a name that is not yet true. Until it is written, the session directory is
//! held by a lock instead, so nothing else starts a session in it meanwhile.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use clap::Parser;
use nix::fcntl::{Flock, FlockArg};
use outrig::config::{Config, ImageConfig};
use outrig::error::IoPathExt;
use outrig::image::{self, ImageTag};
use outrig::{EmbeddedMcpPolicy, LaunchSpec, Outrig, PythonAgent};
use tokio::sync::Mutex;

use crate::builtin_image;
use crate::cli::session_setup::ProgressSpan;
use crate::error::{CliError, OutrigError, Result};
use crate::paths::{default_session_root, repo_root_from_config_path};
use crate::repl::Repl;
use crate::session::{Session, SessionId, SessionStore, resolve_session_root};

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
    repo_cfg_path: &Path,
    global_cfg_path: &Path,
    session_root_flag: Option<&Path>,
    args: &RunNewArgs,
) -> Result<i32> {
    let repo_root = repo_root_from_config_path(repo_cfg_path);
    let span = ProgressSpan::start("loading config");
    let mut cfg = Config::load_for_run(
        &repo_root,
        Some(global_cfg_path),
        args.agent.as_deref(),
        args.model.as_deref(),
    )?;
    span.done("config loaded");
    if !repo_cfg_path.exists() {
        eprintln!(
            "[outrig] no repo config found; using current directory as workspace ({})",
            repo_root.display()
        );
    }

    // Before anything is pulled or started, so a config that names no usable
    // model costs nothing.
    let agent_name = args.agent.clone().or_else(|| cfg.default_agent.clone());
    PythonAgent::check(&cfg, agent_name.as_deref(), args.model.as_deref())
        .map_err(CliError::PythonAgent)?;

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
        cfg: &cfg,
        image_cfg_name: &image_cfg_name,
        image_cfg,
        repo_root: &repo_root,
        log_dir,
        agent_name: agent_name.as_deref(),
        model: args.model.as_deref(),
        session: &mut session,
    })
    .await;

    // Written only now, with the name the container really has. A session that
    // failed to start is recorded too, and at once finalized, so it is never a
    // live record without a container.
    if let Err(e) = store.create(&sid, args.session_dir.as_deref(), &mut session) {
        if let Ok((outrig, agent)) = started {
            drop(agent);
            shut_down(outrig).await;
        }
        return Err(e.into());
    }
    let (outrig, mut agent) = match started {
        Ok(started) => started,
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
            model: agent.model(),
            python_version: agent.python_version(),
            container_name: agent.container_name(),
            session_id: sid.as_str(),
        })
    );
    agent.on_submit(|source| eprint!("{}", render_submission(source)));

    let outcome = repl(agent).await;

    shut_down(outrig).await;
    let exit = outcome.as_ref().copied().unwrap_or(1);
    if let Err(e) = store.finalize(&sid, SystemTime::now(), exit) {
        tracing::warn!(target: "outrig::cli::run_new", "finalizing session {sid}: {e}");
    }
    outcome
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
    let name = chosen
        .or(injection.resolved.map(str::to_string))
        .ok_or_else(|| {
            OutrigError::Configuration(
                "no --image or default-image configured, and outrig's built-in default is \
                 shadowed by a [sidecars.<name>] block using one of its reserved names"
                    .to_string(),
            )
        })?;
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
    cfg: &'a Config,
    image_cfg_name: &'a str,
    image_cfg: &'a ImageConfig,
    repo_root: &'a Path,
    log_dir: PathBuf,
    agent_name: Option<&'a str>,
    model: Option<&'a str>,
    /// Filled in with the image tag and the container name as each is learned.
    session: &'a mut Session,
}

/// Ensure the image, launch the session, and start the agent's interpreter in
/// it. On failure nothing is left running.
async fn start(args: Start<'_>) -> Result<(Outrig, PythonAgent)> {
    let Start {
        cfg,
        image_cfg_name,
        image_cfg,
        repo_root,
        log_dir,
        agent_name,
        model,
        session,
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
    session.image_tag = image.tag.to_string();

    let (spec, skipped) = launch_spec(cfg, image_cfg_name, &image.tag, repo_root, log_dir).await?;
    if !skipped.is_empty() {
        eprintln!(
            "[outrig] run-new starts no MCP servers; not started: {}",
            skipped.join(", ")
        );
    }

    let span = ProgressSpan::start("starting container");
    let outrig = Outrig::launch(&spec).await?;
    span.done("container ready");

    let span = ProgressSpan::start("starting python");
    match PythonAgent::start(&outrig, cfg, agent_name, model).await {
        Ok(agent) => {
            span.done("python ready");
            session.container_name = agent.container_name().to_string();
            Ok((outrig, agent))
        }
        Err(e) => {
            shut_down(outrig).await;
            Err(CliError::PythonAgent(e))
        }
    }
}

/// The launch for `image_cfg_name`, running the already-ensured `tag`, with no
/// MCP server and no sidecar in it -- and the names of the servers left out.
///
/// The image-config is replaced by one naming `tag`, keeping only its
/// security, so the image that runs is the one the session records and
/// `launch` has nothing to build. That also drops its MCP servers. Sidecars go
/// before lowering rather than after: lowering builds or pulls every sidecar
/// image it plans, for containers that would never start. Servers an image
/// declares in its own `org.outrig.mcp` label are only known at launch, so the
/// policy that ignores the label is what keeps those out.
async fn launch_spec(
    cfg: &Config,
    image_cfg_name: &str,
    tag: &ImageTag,
    repo_root: &Path,
    log_dir: PathBuf,
) -> Result<(LaunchSpec, Vec<String>)> {
    let mut pinned = cfg.clone();
    pinned.sidecars.clear();
    let mut skipped = Vec::new();
    if let Some(image) = pinned.images.get_mut(image_cfg_name) {
        skipped = image.mcp.keys().cloned().collect();
        let security = std::mem::take(&mut image.security);
        *image = ImageConfig::from_image_name(tag.as_str());
        image.security = security;
    }
    let spec = LaunchSpec::from_config(&pinned, image_cfg_name, repo_root, log_dir)
        .await?
        .with_embedded_mcp_policy(EmbeddedMcpPolicy::Ignore);
    Ok((spec, skipped))
}

/// Shut the session's containers down, saying so if that fails: by the time
/// this runs there is nothing left to return an error to.
async fn shut_down(outrig: Outrig) {
    if let Err(e) = outrig.shutdown().await {
        eprintln!("[outrig] warning: shutting the session down: {e}");
    }
}

/// Hand typed lines to the agent until the user leaves.
///
/// The agent sits behind an async mutex so a round can borrow it across its
/// awaits. A Ctrl-C drops the round's future, and with it the guard, so the
/// agent -- its conversation and its interpreter -- stays with the session
/// rather than going down with the future.
async fn repl(agent: PythonAgent) -> Result<i32> {
    let agent = Mutex::new(agent);
    let on_prompt = |line: String| {
        let agent = &agent;
        async move {
            // A failed round is reported and the session carries on:
            // `PythonAgent`'s own errors say whether to resend or continue,
            // and either needs a prompt to type it at.
            Ok(match agent.lock().await.round(&line).await {
                Ok(reply) => reply,
                Err(e) => {
                    eprintln!("[outrig] error: {e}");
                    String::new()
                }
            })
        }
    };
    Repl::run("", &[], on_prompt, |_, _| async { None }).await?;
    Ok(0)
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

/// What the terminal shows of a submission: its source, indented under a
/// heading, on stderr with everything else that is not the model's reply.
fn render_submission(source: &str) -> String {
    let mut text = String::from("[outrig] python:\n");
    for line in source.trim_end().lines() {
        text.push_str("    ");
        text.push_str(line);
        text.push('\n');
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

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
    #[tokio::test]
    async fn the_launch_starts_no_mcp_server_and_no_sidecar() {
        let cfg = Config::load_from_str(WITH_SERVERS).expect("config parses");
        cfg.validate(None).expect("config validates");
        let repo = tempfile::tempdir().expect("a repo dir");

        let tag = ImageTag::new("docker.io/library/alpine:latest");
        let (spec, skipped) = launch_spec(
            &cfg,
            "primary",
            &tag,
            repo.path(),
            repo.path().join("logs"),
        )
        .await
        .expect("lowers without touching podman");

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

    #[test]
    fn a_submission_is_shown_indented_under_its_heading() {
        assert_eq!(
            render_submission("x = 41\nprint(x)\n"),
            "[outrig] python:\n    x = 41\n    print(x)\n"
        );
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
