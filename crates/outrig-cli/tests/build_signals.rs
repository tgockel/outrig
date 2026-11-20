//! Real signals to direct builds, with a scripted engine and no podman needed.

mod common;

use std::os::unix::fs::PermissionsExt;
use std::process::Stdio;
use std::time::Duration;

use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use tokio::process::Command;
use tokio::time::{sleep, timeout};

const CEILING: Duration = Duration::from_secs(20);

async fn wait_for_file(path: &std::path::Path) -> String {
    timeout(CEILING, async {
        loop {
            if let Ok(text) = std::fs::read_to_string(path)
                && !text.is_empty()
            {
                return text;
            }
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("missing {}", path.display()))
}

#[tokio::test]
async fn signals_cancel_direct_builds_and_stop_the_all_queue() {
    for stalled in [false, true] {
        if stalled && !cfg!(target_os = "linux") {
            continue;
        }
        for all in [false, true] {
            for (signal, code) in [
                (Signal::SIGINT, 130),
                (Signal::SIGTERM, 143),
                (Signal::SIGHUP, 129),
            ] {
                let repo = common::RepoDir::new();
                let global = common::EmptyGlobalConfig::new();
                let config = repo.path().join(".agents/outrig");
                std::fs::create_dir_all(&config).unwrap();
                std::fs::write(
                    config.join("config.toml"),
                    "default-image = 'aaa'\n\
                 [images.aaa]\ndockerfile = 'Dockerfile'\ncontext = '.'\n\
                 [images.zzz]\ndockerfile = 'Dockerfile'\ncontext = '.'\n",
                )
                .unwrap();
                std::fs::write(repo.path().join("Dockerfile"), "FROM scratch\n").unwrap();
                let tools = repo.beside("tools");
                std::fs::create_dir(&tools).unwrap();
                let engine = tools.join("buildah");
                // The delayed TERM handler proves the CLI keeps its runtime alive
                // for graceful unwind, rather than immediately escalating on exit.
                std::fs::write(
                    &engine,
                    r#"#!/bin/sh
case "$1" in
    images) exit 0 ;;
    build)
        trap 'if [ -n "$logger" ]; then kill -KILL "$logger"; fi; sleep 0.1; echo stopped > "$BUILD_TEST_STATE/stopped"; exit 1' TERM
        tag=
        while [ "$#" -gt 0 ]; do
            if [ "$1" = --tag ]; then shift; tag=$1; fi
            shift
        done
        printf '%s\n' "$tag" >> "$BUILD_TEST_STATE/builds"
        if [ "$BUILD_TEST_NOISY" = 1 ]; then
            while :; do printf '%01024d\n' 0 >&2; done &
            logger=$!
            wait
            exit 1
        fi
        while :; do sleep 0.01; done
        ;;
    rmi) printf '%s\n' "$2" >> "$BUILD_TEST_STATE/removed" ;;
    *) exit 99 ;;
esac
"#,
                )
                .unwrap();
                std::fs::set_permissions(&engine, std::fs::Permissions::from_mode(0o755)).unwrap();
                let mut path = std::ffi::OsString::from(&tools);
                path.push(":");
                path.push(std::env::var_os("PATH").unwrap_or_default());
                let mut cmd = Command::new(env!("CARGO_BIN_EXE_outrig"));
                cmd.arg("--global-config").arg(global.path()).arg("build");
                if all {
                    cmd.arg("--all");
                }
                if stalled {
                    cmd.arg("-vv");
                }
                let mut child = cmd
                    .current_dir(repo.path())
                    .env("PATH", path)
                    .env("BUILD_TEST_STATE", &tools)
                    .env("BUILD_TEST_NOISY", if stalled { "1" } else { "0" })
                    .env_remove("RUST_LOG")
                    .env_remove("OUTRIG_LOG")
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::piped())
                    .kill_on_drop(true)
                    .spawn()
                    .unwrap();
                let tag = wait_for_file(&tools.join("builds")).await;
                assert!(tag.starts_with("aaa:outrig-tmp-"), "{tag}");
                if stalled {
                    wait_for_stalled_output(child.id().unwrap()).await;
                }
                // Only the CLI gets the signal, not the engine: cleanup must come
                // from dropping the build, not from a terminal's group delivery.
                kill(Pid::from_raw(child.id().unwrap() as i32), signal).unwrap();
                let status = timeout(Duration::from_secs(5), child.wait())
                    .await
                    .expect("signals must cancel even with a live, undrained stderr reader")
                    .unwrap();
                assert_eq!(status.code(), Some(code), "{all}: {signal}");
                assert!(tools.join("stopped").exists(), "buildah did not unwind");
                let removed = wait_for_file(&tools.join("removed")).await;
                assert_eq!(removed.trim(), tag.trim());
                assert_eq!(std::fs::read_to_string(tools.join("builds")).unwrap(), tag);
                let mut stderr = String::new();
                use tokio::io::AsyncReadExt;
                child
                    .stderr
                    .take()
                    .unwrap()
                    .read_to_string(&mut stderr)
                    .await
                    .unwrap();
                if !stalled {
                    assert!(
                        stderr.contains(&format!("{signal} received; ending the build")),
                        "{stderr}"
                    );
                }
                assert!(!stderr.contains("all images ready"), "{stderr}");
                assert!(!stderr.contains("image ready:"), "{stderr}");
            }
        }
    }
}

// Linux exposes the actual blocked write, so this regression does not race
// signal delivery against a guess about how fast the engine fills the pipe.
// Keep the pipe reader open throughout; never drain it to make exit possible.
async fn wait_for_stalled_output(pid: u32) {
    timeout(CEILING, async {
        loop {
            let tasks = std::fs::read_dir(format!("/proc/{pid}/task")).unwrap();
            if tasks.filter_map(Result::ok).any(|task| {
                std::fs::read_to_string(task.path().join("wchan"))
                    .is_ok_and(|wchan| wchan.contains("pipe_write"))
            }) {
                break;
            }
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("stderr must actually stall a writer before signaling");
}

/// Tokio filesystem reads run on its blocking pool. Keep a hash input open
/// without EOF so canceling the build cannot finish that read. Normal runtime
/// destruction would then wait forever, even though the signal race resolved.
#[tokio::test]
async fn signals_exit_without_waiting_for_abandoned_hashing_reads() {
    use std::os::unix::fs::OpenOptionsExt;

    for all in [false, true] {
        for (signal, code) in [
            (Signal::SIGINT, 130),
            (Signal::SIGTERM, 143),
            (Signal::SIGHUP, 129),
        ] {
            let repo = common::RepoDir::new();
            let global = common::EmptyGlobalConfig::new();
            let config = repo.path().join(".agents/outrig");
            std::fs::create_dir_all(&config).unwrap();
            std::fs::write(
                config.join("config.toml"),
                "default-image = 'aaa'\n\
                 [images.aaa]\ndockerfile = 'Dockerfile'\ncontext = '.'\n",
            )
            .unwrap();
            let dockerfile = repo.path().join("Dockerfile");
            nix::unistd::mkfifo(
                &dockerfile,
                nix::sys::stat::Mode::S_IRUSR | nix::sys::stat::Mode::S_IWUSR,
            )
            .unwrap();
            let mut cmd = Command::new(env!("CARGO_BIN_EXE_outrig"));
            cmd.arg("--global-config").arg(global.path()).arg("build");
            if all {
                cmd.arg("--all");
            }
            let mut child = cmd
                .current_dir(repo.path())
                .env_remove("RUST_LOG")
                .env_remove("OUTRIG_LOG")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .kill_on_drop(true)
                .spawn()
                .unwrap();
            // A nonblocking writer can open only after the hash worker opens
            // the FIFO for reading. Hold it without writing or closing until
            // after the CLI exits: no timing assumption or large file needed.
            let writer = timeout(CEILING, async {
                loop {
                    match std::fs::OpenOptions::new()
                        .write(true)
                        .custom_flags(nix::libc::O_NONBLOCK)
                        .open(&dockerfile)
                    {
                        Ok(writer) => break writer,
                        Err(e) if e.raw_os_error() == Some(nix::libc::ENXIO) => {
                            assert!(
                                child.try_wait().unwrap().is_none(),
                                "build exited before hashing"
                            );
                            sleep(Duration::from_millis(10)).await;
                        }
                        Err(e) => panic!("open hashing input: {e}"),
                    }
                }
            })
            .await
            .expect("build must start its hashing read");
            kill(Pid::from_raw(child.id().unwrap() as i32), signal).unwrap();
            let status = timeout(Duration::from_secs(5), child.wait())
                .await
                .expect("cancellation must not wait for the abandoned hashing read")
                .unwrap();
            assert_eq!(status.code(), Some(code), "{all}: {signal}");
            drop(writer);
        }
    }
}

/// A static progress line can stall just like the engine transcript. Fill the
/// pipe before spawn and keep its reader open, without ever draining it.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn signals_cancel_progress_writes_to_an_already_full_stderr() {
    use std::io::Write;
    use std::os::fd::AsRawFd;

    for all in [false, true] {
        for (signal, code) in [
            (Some(Signal::SIGINT), 130),
            (Some(Signal::SIGTERM), 143),
            (Some(Signal::SIGHUP), 129),
            (None, 1),
        ] {
            let repo = common::RepoDir::new();
            let global = common::EmptyGlobalConfig::new();
            let config = repo.path().join(".agents/outrig");
            std::fs::create_dir_all(&config).unwrap();
            std::fs::write(
                config.join("config.toml"),
                if signal.is_some() {
                    "default-image = 'aaa'\n[images.aaa]\ndockerfile = 'Dockerfile'\ncontext = '.'\n"
                } else { "" },
            )
            .unwrap();
            std::fs::write(repo.path().join("Dockerfile"), "FROM scratch\n").unwrap();
            let tools = repo.beside("tools");
            std::fs::create_dir(&tools).unwrap();
            let engine = tools.join("buildah");
            std::fs::write(
                &engine,
                "#!/bin/sh\n[ \"$1\" = images ] || exit 99\necho cached\n",
            )
            .unwrap();
            std::fs::set_permissions(&engine, std::fs::Permissions::from_mode(0o755)).unwrap();
            let mut path = std::ffi::OsString::from(&tools);
            path.push(":");
            path.push(std::env::var_os("PATH").unwrap_or_default());
            let (reader, mut writer) = std::io::pipe().unwrap();
            // SAFETY: writer owns a valid pipe fd; F_GETPIPE_SZ takes no
            // pointer argument and does not change its blocking mode.
            let capacity = unsafe { nix::libc::fcntl(writer.as_raw_fd(), nix::libc::F_GETPIPE_SZ) };
            assert!(capacity > 0);
            writer.write_all(&vec![b'x'; capacity as usize]).unwrap();
            let mut cmd = Command::new(env!("CARGO_BIN_EXE_outrig"));
            cmd.arg("--global-config").arg(global.path()).arg("build");
            if all {
                cmd.arg("--all");
            }
            let mut child = cmd
                .current_dir(repo.path())
                .env("PATH", path)
                .env_remove("RUST_LOG")
                .env_remove("OUTRIG_LOG")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(writer)
                .kill_on_drop(true)
                .spawn()
                .unwrap();
            if let Some(signal) = signal {
                wait_for_stalled_output(child.id().unwrap()).await;
                kill(Pid::from_raw(child.id().unwrap() as i32), signal).unwrap();
            }
            let status = timeout(Duration::from_secs(5), child.wait())
                .await
                .expect("signals must cancel a stalled progress write")
                .unwrap();
            assert_eq!(status.code(), Some(code), "{all}: {signal:?}");
            drop(reader);
        }
    }
}
