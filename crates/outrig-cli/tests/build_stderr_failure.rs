//! Build diagnostics must not prevent a build or pull when stderr fails (#986).
//! Stub the image stores so these regressions run without Podman/Buildah.

use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::process::{Output, Stdio};
use std::time::Duration;

use tokio::process::Command;
use tokio::time::timeout;

fn stub(dir: &Path, name: &str, body: &str) {
    let path = dir.join(name);
    std::fs::write(&path, format!("#!/bin/sh\n{body}")).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

async fn build(stderr: Stdio, args: &[&str], config: &str, cache_hit: bool) -> (Output, String) {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    let image = repo.join("image");
    std::fs::create_dir_all(repo.join(".agents/outrig")).unwrap();
    std::fs::create_dir_all(&image).unwrap();
    std::fs::write(image.join("Dockerfile"), "FROM scratch\n").unwrap();
    std::fs::write(repo.join(".agents/outrig/config.toml"), config).unwrap();
    let global = tmp.path().join("global.toml");
    std::fs::write(&global, "").unwrap();
    let trace = tmp.path().join("calls");
    let stubs = tmp.path().join("stubs");
    std::fs::create_dir(&stubs).unwrap();
    stub(
        &stubs,
        "buildah",
        r#"
printf '%s\n' "buildah $1" >> "$OUTRIG_TEST_RUNTIME_TRACE"
case "$1" in
  images) if [ "$OUTRIG_TEST_CACHE_HIT" = 1 ]; then echo cached-id; exit 0; fi; exit 125 ;;
  build|from|config|commit|rm|rmi) exit 0 ;;
esac
exit 1
"#,
    );
    stub(
        &stubs,
        "podman",
        r#"
printf '%s\n' "podman $1 $2" >> "$OUTRIG_TEST_RUNTIME_TRACE"
case "$1 $2" in
  'image inspect') printf 'null\n'; exit 0 ;;
  'image exists') if [ "$OUTRIG_TEST_CACHE_HIT" = 1 ]; then exit 0; fi; exit 1 ;;
  'pull '*) exit 0 ;;
esac
exit 1
"#,
    );
    let mut path = std::ffi::OsString::from(stubs.as_os_str());
    path.push(":");
    path.push(std::env::var_os("PATH").unwrap_or_default());
    let output = timeout(
        Duration::from_secs(20),
        Command::new(env!("CARGO_BIN_EXE_outrig"))
            .arg("--global-config")
            .arg(global)
            .arg("build")
            .args(args)
            .current_dir(repo)
            .env("PATH", path)
            .env("XDG_CACHE_HOME", tmp.path().join("cache"))
            .env("OUTRIG_TEST_RUNTIME_TRACE", &trace)
            .env("OUTRIG_TEST_CACHE_HIT", if cache_hit { "1" } else { "0" })
            .env_remove("RUST_LOG")
            .env_remove("RUST_BACKTRACE")
            .stdin(Stdio::null())
            .stderr(stderr)
            .output(),
    )
    .await
    .expect("build must exit promptly")
    .expect("spawn build");
    let calls = std::fs::read_to_string(&trace).unwrap_or_default();
    (output, calls)
}

const BUILD: &str = "default-image = \"coding\"\n[images.coding]\ndockerfile = \"image/Dockerfile\"\ncontext = \"image\"\n";
const IMAGE: &str =
    "default-image = \"coding\"\n[images.coding]\nimage-name = \"example.invalid/coding:latest\"\n";

#[tokio::test]
async fn failed_stderr_does_not_prevent_build_or_pull() {
    for (config, args, called) in [
        (BUILD, vec!["--no-cache"], "buildah build"),
        (IMAGE, vec!["--no-cache"], "podman pull"),
        (BUILD, vec!["--all", "--no-cache"], "buildah build"),
        (IMAGE, vec!["--all", "--no-cache"], "podman pull"),
    ] {
        let (reader, writer) = std::io::pipe().unwrap();
        drop(reader);
        let (out, calls) = build(writer.into(), &args, config, false).await;
        assert_eq!(out.status.code(), Some(0), "{args:?}: {out:?}; {calls}");
        assert!(calls.contains(called), "{args:?}: {calls}");

        #[cfg(target_os = "linux")]
        {
            let full = std::fs::OpenOptions::new()
                .write(true)
                .open("/dev/full")
                .unwrap();
            let (out, calls) = build(full.into(), &args, config, false).await;
            assert_eq!(out.status.code(), Some(0), "{args:?}: {out:?}; {calls}");
            assert!(calls.contains(called), "{args:?}: {calls}");
        }
    }
}

#[tokio::test]
async fn writable_stderr_keeps_build_diagnostics() {
    for (config, args, expected) in [
        (BUILD, vec!["--no-cache"], "[outrig] dockerfile:"),
        (IMAGE, vec!["--no-cache"], "[outrig] image:"),
        (
            BUILD,
            vec!["--all", "--no-cache"],
            "[outrig] all images ready",
        ),
    ] {
        let (out, _) = build(Stdio::piped(), &args, config, false).await;
        assert!(out.status.success(), "{out:?}");
        let stderr = String::from_utf8(out.stderr).unwrap();
        assert!(stderr.contains(expected), "{stderr}");
        assert!(
            stderr.contains("image ready") || stderr.contains("all images ready"),
            "{stderr}"
        );
    }
}

#[tokio::test]
async fn cache_hit_diagnostics_are_best_effort_too() {
    for (config, marker) in [(BUILD, "cache hit"), (IMAGE, "already pulled")] {
        let (reader, writer) = std::io::pipe().unwrap();
        drop(reader);
        let (out, calls) = build(writer.into(), &[], config, true).await;
        assert_eq!(out.status.code(), Some(0), "{out:?}; {calls}");
        assert!(
            !calls.contains("buildah build") && !calls.contains("podman pull"),
            "{calls}"
        );

        let (out, _) = build(Stdio::piped(), &[], config, true).await;
        assert!(out.status.success(), "{out:?}");
        let stderr = String::from_utf8(out.stderr).unwrap();
        assert!(stderr.contains(marker), "{stderr}");
    }
}
