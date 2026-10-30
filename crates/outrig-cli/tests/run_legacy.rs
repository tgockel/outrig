//! `outrig run-legacy` is `outrig run`, and `outrig run-new` is a command of
//! its own that leaves the interactive `run` as it was.
//!
//! Not `e2e`-gated, like `builtin_default.rs`: the binary runs against a
//! `podman` and `buildah` that fail at once, so each comparison is reached
//! without a container runtime -- and reached through the whole of `run`'s
//! setup up to the first pull, not only through clap.

use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::process::{Output, Stdio};
use std::time::Duration;

use regex::Regex;
use tokio::process::Command;
use tokio::time::timeout;

const TEST_TIMEOUT: Duration = Duration::from_secs(60);

/// The global config, which alone may declare a provider. The provider is
/// never contacted.
const GLOBAL: &str = r#"
[providers.openai]
style    = "chat-completions"
base-url = "http://127.0.0.1:1/v1"
api-key  = "${OUTRIG_TEST_KEY}"
"#;

/// A repo config that gets `run`, over [`GLOBAL`], through model and image
/// resolution to its first podman call.
const CONFIG: &str = r#"
default-model = "fast"
default-image = "primary"

[models.fast]
provider   = "openai"
identifier = "test-model"

[images.primary]
image-name = "docker.io/library/alpine:latest"
"#;

/// A `PATH` whose `podman` and `buildah` exit non-zero immediately.
fn stub_runtime_path(dir: &Path) -> std::ffi::OsString {
    for name in ["podman", "buildah"] {
        let stub = dir.join(name);
        std::fs::write(&stub, "#!/bin/sh\nexit 1\n").expect("write runtime stub");
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755))
            .expect("chmod runtime stub");
    }
    let mut path = std::ffi::OsString::from(dir);
    path.push(":");
    path.push(std::env::var_os("PATH").unwrap_or_default());
    path
}

/// Run `outrig args...` in `cwd` against the failing runtime.
async fn outrig(cwd: &Path, args: &[&str]) -> Output {
    let stubs = tempfile::tempdir().expect("tempdir stubs");
    let cache = tempfile::tempdir().expect("tempdir cache");
    timeout(
        TEST_TIMEOUT,
        Command::new(env!("CARGO_BIN_EXE_outrig"))
            .args(args)
            .current_dir(cwd)
            .env("OUTRIG_TEST_KEY", "test-key")
            .env("OUTRIG_LOG", "info")
            .env("PATH", stub_runtime_path(stubs.path()))
            .env("XDG_CACHE_HOME", cache.path())
            .stdin(Stdio::null())
            .output(),
    )
    .await
    .expect("outrig timed out")
    .expect("spawn outrig")
}

/// A repo holding `config`.
fn repo_holding(config: &str) -> tempfile::TempDir {
    let repo = tempfile::tempdir().expect("a repo");
    let cfg_dir = repo.path().join(".agents/outrig");
    std::fs::create_dir_all(&cfg_dir).expect("create config dir");
    std::fs::write(cfg_dir.join("config.toml"), config).expect("write repo config");
    repo
}

/// A repo holding [`CONFIG`].
fn configured_repo() -> tempfile::TempDir {
    repo_holding(CONFIG)
}

/// The two keys only `run-new` reads: a context window on a second model,
/// `windowed`, which a run opts into with `--model windowed`, and the event log.
const RUN_NEW_ONLY_KEYS: &str = r#"
[models.windowed]
provider       = "openai"
identifier     = "test-model"
context-window = 32000

[events]
mode = "record"
"#;

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// `path` as an argument.
fn arg(path: &Path) -> &str {
    path.to_str().expect("utf-8 path")
}

/// What a `run` invocation did, with what differs between any two runs --
/// the session id and elapsed times -- written out of it: the exit code, the
/// stderr, and the one session record it left.
async fn observed(cwd: &Path, verb: &str, flags: &[&str]) -> (Option<i32>, String, String) {
    let sessions = tempfile::tempdir().expect("tempdir sessions");
    // Outside the repo, which is the workspace.
    let global = tempfile::tempdir().expect("tempdir global");
    let global = global.path().join("config.toml");
    std::fs::write(&global, GLOBAL).expect("write the global config");
    let mut args = vec![
        "--global-config",
        arg(&global),
        "--session-root",
        arg(sessions.path()),
        verb,
    ];
    args.extend(flags);
    let output = outrig(cwd, &args).await;

    let records: Vec<_> = std::fs::read_dir(sessions.path())
        .expect("the session root")
        .map(|entry| entry.expect("an entry").path().join("session.json"))
        .filter(|record| record.exists())
        .collect();
    let record = match records.as_slice() {
        [] => String::new(),
        [record] => std::fs::read_to_string(record).expect("a record"),
        more => panic!("one invocation left {} records", more.len()),
    };

    let normalize = |s: &str| {
        let s = s.replace(arg(sessions.path()), "<root>");
        let s = Regex::new(r"\d{8}T\d{6}-[0-9a-f]{4}")
            .expect("regex")
            .replace_all(&s, "<sid>")
            .into_owned();
        let s = Regex::new(r#""(started|ended)_at": "[^"]+""#)
            .expect("regex")
            .replace_all(&s, r#""${1}_at": "<time>""#)
            .into_owned();
        Regex::new(r"\(\d+(\.\d+)?m?s\)")
            .expect("regex")
            .replace_all(&s, "(<elapsed>)")
            .into_owned()
    };
    (
        output.status.code(),
        normalize(&text(&output.stderr)),
        normalize(&record),
    )
}

/// `run-legacy` does what `run` does, observed rather than assumed: the same
/// exit, the same output, and the same session record, both where a run fails
/// before any session exists and where it gets as far as its first pull.
#[tokio::test]
async fn run_legacy_behaves_as_run() {
    let bare = tempfile::tempdir().expect("a bare dir");
    let configured = configured_repo();
    let used = tempfile::tempdir().expect("a used dir");
    std::fs::write(used.path().join("notes.md"), "mine").expect("a note");

    for (cwd, flags, expect) in [
        (bare.path(), &[][..], "no model selected"),
        (configured.path(), &[][..], "podman"),
        (
            configured.path(),
            &["--max-tool-calls", "7", "--session-dir", arg(used.path())][..],
            "is not empty (it holds notes.md)",
        ),
    ] {
        let run = observed(cwd, "run", flags).await;
        let legacy = observed(cwd, "run-legacy", flags).await;
        assert_eq!(run, legacy, "`run-legacy {flags:?}` diverged from `run`");
        assert_eq!(run.0, Some(1), "{}", run.1);
        assert!(run.1.contains(expect), "{flags:?}: {}", run.1);
        // A config that sets neither key gets no word about them.
        assert!(
            !run.1.contains(ONLY_RUN_NEW_READS_IT),
            "{flags:?}: {}",
            run.1
        );
    }
}

/// `run-legacy` takes exactly the arguments `run` takes, and neither takes a script or the
/// script's `--grace`: those are `run-new`'s.
#[tokio::test]
async fn run_legacy_has_run_s_session_flags() {
    let dir = tempfile::tempdir().expect("a dir");
    let run = outrig(dir.path(), &["run", "--help"]).await;
    let legacy = outrig(dir.path(), &["run-legacy", "--help"]).await;
    assert!(run.status.success() && legacy.status.success());
    let (run, legacy) = (text(&run.stdout), text(&legacy.stdout));
    let options = |help: &str| -> Vec<String> {
        help.lines()
            .filter(|line| line.trim_start().starts_with('-'))
            .map(str::to_string)
            .collect()
    };
    assert_eq!(options(&run), options(&legacy), "{run}\n---\n{legacy}");
    for help in [&run, &legacy] {
        assert!(
            !help.contains("SCRIPT") && !help.contains("--grace") && !help.contains("Python"),
            "{help}"
        );
    }
}

/// `run-new`'s help names both its forms, `run`'s says it is the interactive
/// session and names the retarget to come, and `run-legacy`'s is the
/// interactive session's alone.
#[tokio::test]
async fn run_new_help_names_both_its_forms_and_run_names_the_retarget() {
    let dir = tempfile::tempdir().expect("a dir");
    let help = text(&outrig(dir.path(), &["run-new", "--help"]).await.stdout);
    assert!(
        help.contains("acts by writing Python, or run a Python program on the host (preview)")
            && help.contains("No MCP server and no sidecar is started")
            && help.contains("With a script, it runs the script on the host")
            && help.contains("`outrig run` is unchanged by this command")
            && help.contains("`outrig run-legacy` is another name for it"),
        "{help}"
    );

    let top = text(&outrig(dir.path(), &["--help"]).await.stdout);
    let line = |verb: &str| {
        top.lines()
            .find(|line| line.trim_start().starts_with(&format!("{verb} ")))
            .unwrap_or_else(|| panic!("no `{verb}` line in:\n{top}"))
            .to_string()
    };
    for verb in ["run", "run-legacy"] {
        assert!(
            line(verb).contains("Start an interactive agent session")
                && !line(verb).contains("Python"),
            "{top}"
        );
    }
    assert!(
        line("run-new").contains("writing Python, or run a Python program on the host (preview)"),
        "{top}"
    );

    let run = text(&outrig(dir.path(), &["run", "--help"]).await.stdout);
    assert!(
        run.starts_with("Start an interactive agent session.\n")
            && run.contains("`outrig run-legacy` is the same command")
            && run.contains("A later release makes `outrig run` what `outrig run-new` is today")
            && !run.contains("Python"),
        "{run}"
    );
    let legacy = text(&outrig(dir.path(), &["run-legacy", "--help"]).await.stdout);
    assert!(
        legacy.starts_with("Start an interactive agent session.\n") && !legacy.contains("Python"),
        "{legacy}"
    );
}

/// A config that names no model fails `run-new` before anything is pulled or
/// started, and before a session directory exists.
#[tokio::test]
async fn run_new_fails_on_the_model_before_any_session_exists() {
    let bare = tempfile::tempdir().expect("a bare dir");
    let (code, stderr, record) = observed(bare.path(), "run-new", &[]).await;
    assert_eq!(code, Some(1), "{stderr}");
    assert!(stderr.contains("no model selected"), "{stderr}");
    assert!(!stderr.contains("ensuring image"), "{stderr}");
    assert!(record.is_empty(), "{record}");
}

/// A session that fails to start is recorded and at once finalized, so it is
/// never a live record without a container.
#[tokio::test]
async fn run_new_records_a_failed_start_as_ended() {
    let repo = configured_repo();
    let (code, stderr, record) = observed(repo.path(), "run-new", &[]).await;
    assert_eq!(code, Some(1), "{stderr}");
    assert!(
        stderr.contains("[\"pull\""),
        "the pull is what failed: {stderr}"
    );
    let record: serde_json::Value = serde_json::from_str(&record).expect("a record");
    assert_eq!(record["exit_code"], 1, "{record:#}");
    assert_eq!(record["ended_at"], "<time>", "{record:#}");
    assert_eq!(record["image_config_name"], "primary", "{record:#}");
}

/// A `--session-dir` another `run-new` holds while it starts is refused before
/// any image work, and the refused invocation writes nothing there -- neither
/// a record nor `logs/` -- so the one holding it is not disturbed.
#[tokio::test]
async fn run_new_refuses_a_session_dir_another_is_starting_in() {
    let repo = configured_repo();
    let dir = tempfile::tempdir().expect("a session dir");
    let held = nix::fcntl::Flock::lock(
        std::fs::File::open(dir.path()).expect("open the dir"),
        nix::fcntl::FlockArg::LockExclusiveNonblock,
    )
    .map_err(|(_, e)| e)
    .expect("the test holds it");

    let (code, stderr, _) =
        observed(repo.path(), "run-new", &["--session-dir", arg(dir.path())]).await;
    assert_eq!(code, Some(1), "{stderr}");
    assert!(
        stderr.contains("is in use by another `outrig run-new`"),
        "{stderr}"
    );
    assert!(!stderr.contains("ensuring image"), "{stderr}");
    assert!(!dir.path().join("session.json").exists());
    assert!(!dir.path().join("logs").exists());
    drop(held);
}

/// `run-new --session-dir` follows `run`'s rule: a directory that is not there
/// yet is created, with its parents, and the session that fails to start is
/// recorded in it.
#[tokio::test]
async fn run_new_creates_an_absent_session_dir() {
    let repo = configured_repo();
    let parent = tempfile::tempdir().expect("a parent");
    let dir = parent.path().join("new/session");

    let (code, stderr, record) =
        observed(repo.path(), "run-new", &["--session-dir", arg(&dir)]).await;
    assert_eq!(code, Some(1), "{stderr}");
    assert!(
        stderr.contains("[\"pull\""),
        "the pull is what failed: {stderr}"
    );
    assert!(dir.join("logs").is_dir(), "{stderr}");
    assert!(dir.join("session.json").is_file(), "{stderr}");
    assert!(!record.is_empty(), "the root links to it: {stderr}");
}

/// A `--session-dir` that holds anything is refused, by its first entry, before
/// any image work -- and gets nothing from the refused invocation.
#[tokio::test]
async fn run_new_refuses_a_session_dir_that_is_not_empty() {
    let repo = configured_repo();
    let dir = tempfile::tempdir().expect("a dir of notes");
    std::fs::write(dir.path().join("notes.md"), "mine").expect("a note");

    let (code, stderr, record) =
        observed(repo.path(), "run-new", &["--session-dir", arg(dir.path())]).await;
    assert_eq!(code, Some(1), "{stderr}");
    assert!(
        stderr.contains("is not empty (it holds notes.md)"),
        "{stderr}"
    );
    assert!(!stderr.contains("ensuring image"), "{stderr}");
    assert!(!dir.path().join("logs").exists());
    assert!(record.is_empty(), "{record}");
}

/// What `run` says at startup about a key only `run-new` reads.
const ONLY_RUN_NEW_READS_IT: &str = "only `run-new` reads it";

/// A config that sets `context-window` on the session's model and turns the
/// event log on gets one warning per key from `run`, ahead of the first pull,
/// because it parses both and does nothing with either. `run-new` reads both,
/// so it says nothing. (`run-legacy` is `run`; `run_legacy_behaves_as_run`
/// holds that.)
#[tokio::test]
async fn run_warns_once_per_key_only_run_new_reads() {
    let repo = repo_holding(&format!("{CONFIG}{RUN_NEW_ONLY_KEYS}"));
    let flags = ["--model", "windowed"];

    let (code, stderr, _) = observed(repo.path(), "run", &flags).await;
    assert_eq!(code, Some(1), "{stderr}");
    let warned: Vec<&str> = stderr
        .lines()
        .filter(|line| line.contains(ONLY_RUN_NEW_READS_IT))
        .collect();
    assert_eq!(
        warned,
        [
            "[outrig] warning: context-window on [models.windowed] is ignored by \
             `run`; only `run-new` reads it",
            "[outrig] warning: [events] mode = \"record\" is ignored by \
             `run`; only `run-new` reads it",
        ],
        "{stderr}"
    );
    let first_warning = stderr.find(ONLY_RUN_NEW_READS_IT).expect("a warning");
    let pull = stderr.find("ensuring image").expect("it reached the pull");
    assert!(first_warning < pull, "{stderr}");

    let (code, stderr, _) = observed(repo.path(), "run-new", &flags).await;
    assert_eq!(code, Some(1), "{stderr}");
    assert!(
        stderr.contains("ensuring image"),
        "it reached the pull: {stderr}"
    );
    assert!(!stderr.contains(ONLY_RUN_NEW_READS_IT), "{stderr}");
}
