//! `outrig run-legacy` is `outrig run` under another name, and `outrig run-new`
//! is a command of its own that leaves `run` as it was.
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

/// A repo config that gets `run` through model and image resolution to its
/// first podman call. The provider is never contacted.
const CONFIG: &str = r#"
default-model = "fast"
default-image = "primary"

[providers.openai]
style    = "openai"
base-url = "http://127.0.0.1:1/v1"
api-key  = "${OUTRIG_TEST_KEY}"

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

/// A repo holding [`CONFIG`].
fn configured_repo() -> tempfile::TempDir {
    let repo = tempfile::tempdir().expect("a repo");
    let cfg_dir = repo.path().join(".agents/outrig");
    std::fs::create_dir_all(&cfg_dir).expect("create config dir");
    std::fs::write(cfg_dir.join("config.toml"), CONFIG).expect("write repo config");
    repo
}

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
    let absent = cwd.join("no-such-global.toml");
    let mut args = vec![
        "--global-config",
        arg(&absent),
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
    let missing = configured.path().join("missing");

    for (cwd, flags, expect) in [
        (bare.path(), &[][..], "no model selected"),
        (configured.path(), &[][..], "podman"),
        (
            configured.path(),
            &["--max-tool-calls", "7", "--session-dir", arg(&missing)][..],
            "is not an existing directory",
        ),
    ] {
        let run = observed(cwd, "run", flags).await;
        let legacy = observed(cwd, "run-legacy", flags).await;
        assert_eq!(run, legacy, "`run-legacy {flags:?}` diverged from `run`");
        assert_eq!(run.0, Some(1), "{}", run.1);
        assert!(run.1.contains(expect), "{flags:?}: {}", run.1);
    }
}

#[tokio::test]
async fn run_legacy_has_run_s_help() {
    let dir = tempfile::tempdir().expect("a dir");
    let run = outrig(dir.path(), &["run", "--help"]).await;
    let legacy = outrig(dir.path(), &["run-legacy", "--help"]).await;
    assert!(run.status.success());
    assert_eq!(text(&run.stdout), text(&legacy.stdout));
}

/// `run-new`'s help says what it is, and nothing about `run` suggests that
/// `run` has changed: its summary and its own help are as they were.
#[tokio::test]
async fn run_new_help_describes_it_without_touching_run() {
    let dir = tempfile::tempdir().expect("a dir");
    let help = text(&outrig(dir.path(), &["run-new", "--help"]).await.stdout);
    assert!(
        help.contains("acts by writing Python (preview)")
            && help.contains("No MCP server and no sidecar is started")
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
    assert!(
        line("run").contains("Start an interactive agent session [alias: run-legacy]"),
        "{top}"
    );
    assert!(
        line("run-new").contains("writing Python (preview)"),
        "{top}"
    );

    let run = text(&outrig(dir.path(), &["run", "--help"]).await.stdout);
    assert!(
        run.starts_with("Start an interactive agent session\n")
            && !run.contains("run-new")
            && !run.contains("Python"),
        "{run}"
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
