//! `outrig run <script.py>` with sessions, end to end: the binary, a program entering sessions
//! in real containers, and what `outrig` does when the program is interrupted, ignores it, or
//! ends with a session open. Everything is asserted from outside: the status, what the terminal
//! showed, what podman still holds, and what the session left on disk.

#![cfg(feature = "e2e")]

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::time::timeout;

const ALPINE: &str = "docker.io/library/alpine:latest";
const TEST_TIMEOUT: Duration = Duration::from_secs(120);

/// A repository whose sessions run in alpine, and a session root beside it.
struct Case {
    dir: tempfile::TempDir,
}

impl Case {
    fn new(extra: &str) -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = dir.path().join("repo/.agents/outrig/config.toml");
        std::fs::create_dir_all(config.parent().unwrap()).expect("mkdir");
        std::fs::create_dir_all(dir.path().join("sessions")).expect("mkdir");
        std::fs::write(
            config,
            format!(
                "default-image = \"alpine\"\n\n[images.alpine]\nimage-name = {ALPINE:?}\n\n{extra}"
            ),
        )
        .expect("write the config");
        Self { dir }
    }

    fn repo(&self) -> PathBuf {
        self.dir.path().join("repo")
    }

    fn sessions(&self) -> PathBuf {
        self.dir.path().join("sessions")
    }

    fn write(&self, name: &str, source: &str) {
        std::fs::write(self.repo().join(name), source).expect("write the script");
    }

    /// `outrig --session-root <case> run <args>`, in the repository, in a process group of its
    /// own, its stdout and stderr piped.
    fn spawn(&self, args: &[&str]) -> Child {
        Command::new(env!("CARGO_BIN_EXE_outrig"))
            // An empty global config, which exists, as `--global-config` must.
            .arg("--global-config")
            .arg("/dev/null")
            .arg("--session-root")
            .arg(self.sessions())
            .arg("run")
            .args(args)
            .current_dir(self.repo())
            .env_remove("RUST_LOG")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .kill_on_drop(true)
            .spawn()
            .expect("outrig starts")
    }
}

/// Pull the image the sessions run in, once it is needed.
fn pull() {
    let status = std::process::Command::new("podman")
        .args(["pull", "--quiet", ALPINE])
        .stdout(Stdio::null())
        .status()
        .expect("podman pull");
    assert!(status.success(), "pull {ALPINE}");
}

/// The containers podman holds, running or not, labeled for the session `id`.
fn containers_of(id: &str) -> String {
    let out = std::process::Command::new("podman")
        .args(["ps", "-a", "--format", "{{.Names}}", "--filter"])
        .arg(format!("label=org.outrig.session={id}"))
        .output()
        .expect("podman ps");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// Read `child`'s stdout until a line starting `prefix`, and return that line.
async fn line_starting(child: &mut Child, prefix: &str) -> String {
    let stdout = child.stdout.as_mut().expect("piped");
    let mut lines = BufReader::new(stdout).lines();
    timeout(TEST_TIMEOUT, async {
        loop {
            let line = lines
                .next_line()
                .await
                .expect("read stdout")
                .unwrap_or_else(|| panic!("stdout ended before a line starting {prefix:?}"));
            if line.starts_with(prefix) {
                return line;
            }
        }
    })
    .await
    .expect("the line in time")
}

/// Wait for `child`, returning its status, its stderr, and the rest of its stdout.
async fn finish(child: Child) -> (std::process::ExitStatus, String, String) {
    let out = timeout(TEST_TIMEOUT, child.wait_with_output())
        .await
        .expect("outrig ended in time")
        .expect("wait");
    (
        out.status,
        String::from_utf8_lossy(&out.stderr).into_owned(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
    )
}

/// The example the run page gives for a program that leaves its session when interrupted.
fn documented_example() -> String {
    let page = include_str!("../../../doc/usage/run.md");
    let after = page
        .split_once("A program that leaves its session when interrupted, and prints the report")
        .expect("the run page gives the example")
        .1;
    let code = after.split_once("```python\n").expect("a python block").1;
    code.split_once("```")
        .expect("the block ends")
        .0
        .to_string()
}

/// The run page's example: Ctrl-C makes the program leave its session and print the report
/// itself, and `outrig` exits 0 with nothing left in podman. With `[events] mode = "record"` the
/// session's events are in its directory under the session root, readable by its owner alone,
/// from `starting` to `reported`.
#[tokio::test]
async fn the_documented_example_exits_0_after_ctrl_c() {
    use std::os::unix::fs::PermissionsExt as _;

    pull();
    let case = Case::new("[events]\nmode = \"record\"\n");
    case.write("leaves.py", &documented_example());
    let mut child = case.spawn(&["leaves.py"]);
    let started = line_starting(&mut child, "session ").await;
    let id = started
        .strip_prefix("session ")
        .and_then(|rest| rest.strip_suffix(" started"))
        .expect("the id")
        .to_string();
    kill(Pid::from_raw(child.id().unwrap() as i32), Signal::SIGINT).unwrap();
    let (status, stderr, stdout) = finish(child).await;
    assert_eq!(status.code(), Some(0), "{stderr}");
    assert_eq!(stdout, format!("session {id}: clean\n"));
    assert!(
        !stderr.contains("[outrig] session"),
        "nothing was left open: {stderr}"
    );
    assert_eq!(containers_of(&id), "");

    let log = case.sessions().join(&id).join("logs/events.jsonl");
    let mode = std::fs::metadata(&log)
        .expect("the log")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600);
    let records: Vec<serde_json::Value> = std::fs::read_to_string(&log)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let state = |record: &serde_json::Value| record["data"]["state"].clone();
    assert_eq!(state(&records[0]), "starting");
    assert_eq!(state(records.last().unwrap()), "reported");
}

/// A program that ignores the forwarded signal while holding a session is stopped once its
/// `--grace` has passed -- `SIGTERM`, which it ignores too, then `SIGKILL` -- and `outrig` shuts
/// its session down, prints the report, leaves no container, and exits with the program's 137.
#[tokio::test]
async fn a_program_ignoring_signals_is_stopped_and_its_session_with_it() {
    pull();
    let case = Case::new("");
    case.write(
        "stubborn.py",
        "import asyncio, outrig, signal\n\
         signal.signal(signal.SIGINT, signal.SIG_IGN)\n\
         signal.signal(signal.SIGTERM, signal.SIG_IGN)\n\
         async def main():\n\
         \x20   async with outrig.Session() as session:\n\
         \x20       print(f'session {session.id}', flush=True)\n\
         \x20       await asyncio.sleep(120)\n\
         asyncio.run(main())\n",
    );
    let mut child = case.spawn(&["--grace", "1", "stubborn.py"]);
    let id = line_starting(&mut child, "session ").await["session ".len()..].to_string();
    let signalled = Instant::now();
    kill(Pid::from_raw(child.id().unwrap() as i32), Signal::SIGTERM).unwrap();
    let (status, stderr, _) = finish(child).await;
    assert_eq!(status.code(), Some(137), "{stderr}");
    assert!(
        signalled.elapsed() >= Duration::from_secs(6),
        "{:?}",
        signalled.elapsed()
    );
    assert!(
        stderr.contains(&format!(
            "[outrig] session {id} was open when the program ended, and was shut down\n\
             [outrig] session closed: stopped, every outcome known\n"
        )),
        "{stderr}"
    );
    assert_eq!(containers_of(&id), "");
}

/// A program that ends with its session open -- returning, raising, or killed with `SIGKILL` --
/// leaves it to `outrig`, which shuts it down, prints its report, and leaves no container, all
/// within a minute, exiting with the program's own status.
#[tokio::test]
async fn outrig_stops_the_session_a_program_left_open() {
    pull();
    let case = Case::new("");
    case.write(
        "leaves.py",
        "import asyncio, os, outrig, sys, time\n\
         async def main():\n\
         \x20   session = outrig.Session()\n\
         \x20   await session.__aenter__()\n\
         \x20   print(f'session {session.id} {os.getpid()}', flush=True)\n\
         \x20   if sys.argv[1] == 'raise':\n\
         \x20       raise RuntimeError('left open')\n\
         \x20   if sys.argv[1] == 'kill':\n\
         \x20       time.sleep(120)\n\
         asyncio.run(main())\n",
    );
    for (how, code) in [("return", 0), ("raise", 1), ("kill", 137)] {
        let mut child = case.spawn(&["leaves.py", how]);
        let line = line_starting(&mut child, "session ").await;
        let mut words = line.split_whitespace().skip(1);
        let id = words.next().unwrap().to_string();
        let pid: i32 = words.next().unwrap().parse().unwrap();
        let ended = Instant::now();
        if how == "kill" {
            kill(Pid::from_raw(pid), Signal::SIGKILL).unwrap();
        }
        let (status, stderr, _) = finish(child).await;
        assert_eq!(status.code(), Some(code), "{how}: {stderr}");
        assert!(
            ended.elapsed() < Duration::from_secs(60),
            "{how}: {:?}",
            ended.elapsed()
        );
        assert!(
            stderr.contains(&format!(
                "[outrig] session {id} was open when the program ended, and was shut down"
            )),
            "{how}: {stderr}"
        );
        assert_eq!(containers_of(&id), "", "{how}");
        assert!(Path::new(&case.sessions().join(&id)).is_dir(), "{how}");
    }
}
