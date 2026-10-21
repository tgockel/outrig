//! #368: the global config is found from the environment alone, never from the working
//! directory. An empty or relative `XDG_CONFIG_HOME` is unset, as the XDG Base Directory
//! Specification says, and a home directory that is not absolute is no home directory.
//!
//! Through the binary, without a container runtime. Not `e2e`-gated, for the reason
//! `builtin_default.rs` gives: each assertion is settled before the first podman call. Every
//! test plants a config the old lookup would have read from the working directory, and one that
//! is a TOML syntax error, so a command that reads it fails and cannot go unnoticed.

mod common;

use std::fs;
use std::path::Path;
use std::process::{Command, Output, Stdio};

use common::{run_outrig_output, sample_session, utf8};
use outrig_cli::session::{SessionId, SessionStore};

/// The file the issue's reproduction planted: any load that reads it fails.
const BROKEN: &str = "session-root = [\n";

/// `body` as the config at `<dir>/config.toml`, creating `dir`.
fn write_config(dir: &Path, body: &str) {
    fs::create_dir_all(dir).unwrap();
    fs::write(dir.join("config.toml"), body).unwrap();
}

/// One session recorded under `root`.
fn record(root: &Path, sid: &str) -> SessionId {
    let sid = SessionId(sid.into());
    SessionStore::new(root.to_path_buf())
        .create(&sid, None, &mut sample_session(&sid))
        .expect("record a session");
    sid
}

/// A config whose `session-root` resolves beside it, with one session recorded there. `ls`
/// listing the session says this is the config it read.
fn config_with_a_session(dir: &Path, sid: &str) -> SessionId {
    write_config(dir, "session-root = \"sessions\"\n");
    record(&dir.join("sessions"), sid)
}

/// `ls` in `work` with `env`, which must succeed; its stdout.
async fn ls(work: &Path, env: &[(&str, &Path)]) -> String {
    let out = run_outrig_output(work, &["ls"], env).await;
    assert!(
        out.status.success(),
        "ls with {env:?}:\n{}",
        String::from_utf8_lossy(&out.stderr),
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// `outrig` in `work` with the relative `HOME=home` and no `XDG_CONFIG_HOME` at all. The shared
/// harness can only add variables, and before the fix an empty `XDG_CONFIG_HOME` was taken as
/// set, so the home directory was never consulted.
fn outrig_with_a_relative_home(work: &Path, data: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_outrig"))
        .args(args)
        .current_dir(work)
        .env("HOME", "home")
        .env_remove("XDG_CONFIG_HOME")
        .env("XDG_DATA_HOME", data)
        .stdin(Stdio::null())
        .output()
        .expect("spawn outrig")
}

/// The issue's reproduction. `XDG_CONFIG_HOME=` read `./outrig/config.toml` and
/// `XDG_CONFIG_HOME=conf` read `./conf/outrig/config.toml`, each as the global config. Both are
/// unset now, so the global config is `~/.outrig/config.toml`. An absolute value is still
/// honored ahead of it.
#[tokio::test]
async fn an_empty_or_relative_xdg_config_home_is_unset() {
    let tmp = tempfile::tempdir().unwrap();
    let work = tmp.path().join("work");
    write_config(&work.join("outrig"), BROKEN);
    write_config(&work.join("conf/outrig"), BROKEN);
    let home = tmp.path().join("home");
    let from_home = config_with_a_session(&home.join(".outrig"), "20261012T100000-aaaa");
    let data = tmp.path().join("data");

    for xdg in ["", "conf"] {
        let env = [
            ("HOME", home.as_path()),
            ("XDG_CONFIG_HOME", Path::new(xdg)),
            ("XDG_DATA_HOME", data.as_path()),
        ];
        let stdout = ls(&work, &env).await;
        assert!(
            stdout.contains(from_home.as_str()),
            "XDG_CONFIG_HOME={xdg:?} must leave ~/.outrig/config.toml in charge:\n{stdout}",
        );
    }

    let xdg = tmp.path().join("xdg");
    let from_xdg = config_with_a_session(&xdg.join("outrig"), "20261012T100001-bbbb");
    let env = [
        ("HOME", home.as_path()),
        ("XDG_CONFIG_HOME", xdg.as_path()),
        ("XDG_DATA_HOME", data.as_path()),
    ];
    let stdout = ls(&work, &env).await;
    assert!(stdout.contains(from_xdg.as_str()), "{stdout}");
    assert!(!stdout.contains(from_home.as_str()), "{stdout}");
}

/// A home directory that is not absolute is no home directory, so there is no default location
/// and the command runs without a global config: the session root is the XDG default, not
/// whatever `./home/.outrig/config.toml` says.
#[test]
fn a_relative_home_names_no_global_config() {
    let tmp = tempfile::tempdir().unwrap();
    let work = tmp.path().join("work");
    write_config(&work.join("home/.outrig"), BROKEN);
    let data = tmp.path().join("data");
    let sid = record(&data.join("outrig/sessions"), "20261012T100002-cccc");

    let out = outrig_with_a_relative_home(&work, &data, &["ls"]);

    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains(sid.as_str()), "{stdout}");
}

/// With nowhere to put the global config, the commands that write it stop and say what to do,
/// rather than write it under the working directory. `--global-config` names a place, and the
/// wizard goes on.
#[test]
fn the_writers_stop_when_there_is_nowhere_to_put_the_global_config() {
    let tmp = tempfile::tempdir().unwrap();
    let work = tmp.path().join("work");
    fs::create_dir_all(&work).unwrap();
    let data = tmp.path().join("data");

    for args in [&["config", "init"][..], &["init"]] {
        let out = outrig_with_a_relative_home(&work, &data, args);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            !out.status.success(),
            "`outrig {args:?}` must stop:\n{stderr}"
        );
        assert!(
            stderr.contains(
                "nowhere to put the global config: there is no absolute XDG_CONFIG_HOME and no \
                 absolute home directory\n\
                 help: pass --global-config <path>, or set HOME to an absolute path"
            ),
            "`outrig {args:?}` must say what to do:\n{stderr}",
        );
    }
    assert!(
        !work.join("home").exists(),
        "nothing is written beside the cwd"
    );

    let flag = tmp.path().join("global.toml");
    let args = ["--global-config", utf8(&flag), "config", "init"];
    let out = outrig_with_a_relative_home(&work, &data, &args);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains(&format!(
            "[outrig] writing global config to {}\n",
            flag.display()
        )),
        "{stderr}",
    );
}

/// The commands that write the global config used to write it where it was read: under the
/// working directory. They name `~/.outrig/config.toml` now. With null stdin the wizard stops at
/// its first prompt, after the line that says where it would write.
#[tokio::test]
async fn config_init_does_not_place_the_global_config_in_the_working_directory() {
    let tmp = tempfile::tempdir().unwrap();
    let work = tmp.path().join("work");
    fs::create_dir_all(&work).unwrap();
    let home = tmp.path().join("home");
    fs::create_dir_all(&home).unwrap();

    let env = [("HOME", home.as_path()), ("XDG_CONFIG_HOME", Path::new(""))];
    let out = run_outrig_output(&work, &["config", "init"], &env).await;
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains(&format!(
            "[outrig] writing global config to {}\n",
            home.join(".outrig/config.toml").display()
        )),
        "{stderr}",
    );
    assert!(
        !work.join("outrig").exists(),
        "nothing is written beside the cwd"
    );
}
