//! Failed diagnostics must not turn a configuration error into a panic (#907).
//! No container runtime is needed: these commands fail during setup.

mod common;

use std::process::Stdio;
use std::time::Duration;

use tokio::process::Command;
use tokio::time::timeout;

async fn run_without_model(
    stderr: Stdio,
    repo_config: bool,
    hard_link: bool,
) -> std::process::Output {
    let repo = common::RepoDir::new();
    if repo_config {
        let dir = repo.path().join(".agents/outrig");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("config.toml"), "").unwrap();
    }
    let global = common::EmptyGlobalConfig::new();
    if hard_link {
        std::fs::hard_link(global.path(), repo.beside("global-link.toml")).unwrap();
    }
    let child = Command::new(env!("CARGO_BIN_EXE_outrig"))
        .arg("--global-config")
        .arg(global.path())
        .arg("--session-root")
        .arg(repo.beside("sessions"))
        .arg("run")
        .current_dir(repo.path())
        .env_remove("RUST_LOG")
        .env_remove("RUST_BACKTRACE")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(stderr)
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    timeout(Duration::from_secs(20), child.wait_with_output())
        .await
        .expect("run must exit promptly")
        .expect("wait for run")
}

#[tokio::test]
async fn startup_with_a_reader_already_gone_keeps_the_error_status() {
    for repo_config in [false, true] {
        for hard_link in [false, true] {
            // Close the reader before spawn, not just before waiting on the
            // child, so even its earliest diagnostic deterministically fails.
            let (reader, writer) = std::io::pipe().unwrap();
            drop(reader);
            let output = run_without_model(writer.into(), repo_config, hard_link).await;
            assert_eq!(output.status.code(), Some(1), "{output:?}");
            assert!(output.stdout.is_empty(), "{output:?}");
        }
    }
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn startup_with_a_full_stderr_keeps_the_error_status() {
    for repo_config in [false, true] {
        for hard_link in [false, true] {
            let full = std::fs::OpenOptions::new()
                .write(true)
                .open("/dev/full")
                .unwrap();
            let output = run_without_model(full.into(), repo_config, hard_link).await;
            assert_eq!(output.status.code(), Some(1), "{output:?}");
            assert!(output.stdout.is_empty(), "{output:?}");
        }
    }
}

#[tokio::test]
async fn writable_stderr_keeps_workspace_warning_and_error_diagnostics() {
    for repo_config in [false, true] {
        let output = run_without_model(Stdio::piped(), repo_config, true).await;
        assert_eq!(output.status.code(), Some(1), "{output:?}");
        let stderr = String::from_utf8(output.stderr).unwrap();
        let workspace_note = if repo_config {
            "[outrig] workspace:"
        } else {
            "[outrig] no repo config found; using current directory as workspace"
        };
        assert!(stderr.contains(workspace_note), "{stderr}");
        assert!(stderr.contains("[outrig] warning:"), "{stderr}");
        assert!(stderr.contains("error: no model selected"), "{stderr}");
    }
}
