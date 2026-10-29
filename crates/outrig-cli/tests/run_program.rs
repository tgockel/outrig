//! `outrig run-new <script.py>` as a user runs it: the binary, a script on disk, and the payload's
//! Python on the host.
//!
//! Not `e2e`-gated: nothing here needs a container runtime, and `podman` and `buildah` are stubs
//! that log every call, so a test can say none was made. Each test passes an absent
//! `--global-config` and its own working directory. The payload unpacks into the user's cache, as
//! the library's tests use it, rather than a fresh `XDG_CACHE_HOME` per run.
//!
//! `outrig` starts in a process group of its own, as a shell starts a job, so a signal a test sends
//! reaches it and nothing else.

mod common;

use std::ffi::OsStr;
use std::io::{Read, Write as _};
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt as _;
use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use nix::errno::Errno;
use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use tokio::process::Command;
use tokio::time::timeout;

use common::{CannedResponse, drain_recorded, start_mock_http, stub_runtime_path};

const TEST_TIMEOUT: Duration = Duration::from_secs(60);

/// A working directory, a global config outside it, and a `PATH` whose `podman` and `buildah`
/// log their calls and fail.
struct Case {
    dir: tempfile::TempDir,
    global: tempfile::TempDir,
    path: std::ffi::OsString,
}

impl Case {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let stubs = dir.path().join("stubs");
        std::fs::create_dir(&stubs).expect("mkdir stubs");
        let path = stub_runtime_path(&stubs);
        let global = tempfile::tempdir().expect("tempdir global");
        let case = Self { dir, global, path };
        case.write_global("");
        case
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    fn global_config(&self) -> PathBuf {
        self.global.path().join("config.toml")
    }

    /// The global config, which a `--global-config` must name and only it may declare a provider
    /// in.
    fn write_global(&self, contents: &str) {
        std::fs::write(self.global_config(), contents).expect("write the global config");
    }

    fn write(&self, name: &str, contents: &str) -> PathBuf {
        let path = self.path().join(name);
        std::fs::create_dir_all(path.parent().expect("a parent")).expect("mkdir");
        std::fs::write(&path, contents).expect("write");
        path
    }

    /// Every call the runtime stubs logged.
    fn runtime_calls(&self) -> String {
        std::fs::read_to_string(self.path().join("stubs/runtime.log")).unwrap_or_default()
    }

    /// `outrig --global-config <the case's> <args>`, working in the case's directory, in a process
    /// group of its own.
    fn outrig(&self, args: &[&str]) -> Command {
        let mut command = self.outrig_in_this_group(args);
        command.process_group(0);
        command
    }

    /// [`Case::outrig`], left in this process's group for its own pre-exec hook to move.
    fn outrig_in_this_group(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_outrig"));
        command
            .arg("--global-config")
            .arg(self.global_config())
            .args(args)
            .current_dir(self.path())
            .env("PATH", &self.path)
            .env("NO_PROXY", "127.0.0.1,localhost")
            .env("no_proxy", "127.0.0.1,localhost")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        command
    }

    async fn run(&self, args: &[&str]) -> Output {
        timeout(TEST_TIMEOUT, self.outrig(args).output())
            .await
            .expect("outrig timed out")
            .expect("outrig starts")
    }
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// A global config whose provider is the mock at `addr`, so a model call would be recorded.
fn global_config(addr: std::net::SocketAddr) -> String {
    format!(
        r#"
[providers.mock]
style    = "anthropic"
base-url = "http://{addr}"
api-key  = "${{OUTRIG_TEST_PROGRAM_KEY}}"
"#
    )
}

/// A repo config whose model is on [`global_config`]'s provider.
fn repo_config(default_model: &str) -> String {
    format!(
        r#"
default-model = "{default_model}"

[models.{default_model}]
provider   = "mock"
identifier = "mock-model"
max-tokens = 1024
"#
    )
}

/// The requirement's example runs on the host alone: no container runtime call, no model call,
/// the program's two lines, and its status -- 0, then 2 for a program returning 2, and 1 with
/// Python's traceback for one that raises. A missing script is a launcher diagnostic and status 1,
/// with nothing started.
#[tokio::test]
async fn the_requirements_example_runs_on_the_host_alone() {
    let case = Case::new();
    let (addr, mut requests) =
        start_mock_http(vec![CannedResponse::ok(serde_json::json!({}))]).await;
    case.write_global(&global_config(addr));
    case.write(".agents/outrig/config.toml", &repo_config("fast"));
    std::fs::create_dir(case.path().join("workspace")).unwrap();
    case.write(
        "worker.py",
        "import sys\nworkspace, task = sys.argv[1:]\nprint(f'workspace: {workspace}')\n\
         print(f'task: {task}')\n",
    );
    case.write("returns_two.py", "import sys\nsys.exit(2)\n");
    case.write(
        "raises.py",
        "raise RuntimeError('the tests are still failing')\n",
    );

    let ran = case
        .run(&["run-new", "worker.py", "./workspace", "Fix the tests"])
        .await;
    assert_eq!(ran.status.code(), Some(0), "{}", text(&ran.stderr));
    assert_eq!(
        text(&ran.stdout),
        "workspace: ./workspace\ntask: Fix the tests\n"
    );
    assert_eq!(text(&ran.stderr), "");

    assert_eq!(
        case.run(&["run-new", "returns_two.py"]).await.status.code(),
        Some(2)
    );

    let raised = case.run(&["run-new", "raises.py"]).await;
    assert_eq!(raised.status.code(), Some(1));
    assert_eq!(
        text(&raised.stderr),
        "Traceback (most recent call last):\n  File \"raises.py\", line 1, in <module>\n    \
         raise RuntimeError('the tests are still failing')\n\
         RuntimeError: the tests are still failing\n"
    );

    let missing = case.run(&["run-new", "missing.py"]).await;
    assert_eq!(missing.status.code(), Some(1));
    assert_eq!(
        text(&missing.stderr),
        "error: cannot run the program missing.py: No such file or directory (os error 2)\n"
    );
    assert_eq!(text(&missing.stdout), "");

    assert_eq!(case.runtime_calls(), "", "a container runtime was called");
    assert!(
        drain_recorded(&mut requests).is_empty(),
        "the model provider was called"
    );
}

/// A session flag before the script is refused, naming it; after the script, the same spelling
/// is the program's argument.
#[tokio::test]
async fn a_session_flag_before_the_script_is_refused_and_after_it_is_the_programs() {
    let case = Case::new();
    case.write("argv.py", "import sys\nprint(sys.argv)\n");

    let refused = case.run(&["run-new", "--agent", "x", "argv.py"]).await;
    assert_eq!(refused.status.code(), Some(2));
    assert!(
        text(&refused.stderr)
            .starts_with("error: the argument '--agent <NAME>' cannot be used with '[SCRIPT]...'"),
        "{}",
        text(&refused.stderr)
    );

    let passed = case
        .run(&["run-new", "argv.py", "--agent", "x", "--help"])
        .await;
    assert_eq!(passed.status.code(), Some(0), "{}", text(&passed.stderr));
    assert_eq!(
        text(&passed.stdout),
        "['argv.py', '--agent', 'x', '--help']\n"
    );
    assert_eq!(case.runtime_calls(), "");
}

/// A script given to `run`, the interactive session's command, is a usage error that names
/// `run-new`, the command that runs one, and `--grace` is not `run`'s.
#[tokio::test]
async fn a_script_given_to_run_is_refused_naming_run_new() {
    let case = Case::new();
    case.write("worker.py", "print('ran')\n");

    for args in [
        &["run", "worker.py", "./workspace", "Fix the tests"][..],
        &["run-legacy", "worker.py"],
    ] {
        let refused = case.run(args).await;
        assert_eq!(refused.status.code(), Some(2), "{args:?}");
        assert_eq!(text(&refused.stdout), "", "{args:?}");
        assert!(
            text(&refused.stderr).contains("`outrig run-new worker.py`"),
            "{args:?}: {}",
            text(&refused.stderr)
        );
    }
    let refused = case.run(&["run", "--grace", "1"]).await;
    assert_eq!(refused.status.code(), Some(2), "{}", text(&refused.stderr));
    assert!(
        text(&refused.stderr).contains("unexpected argument '--grace'"),
        "{}",
        text(&refused.stderr)
    );
    assert_eq!(case.runtime_calls(), "", "a container runtime was called");
}

/// The configuration is the invocation's: with a repo config in the working directory and
/// another beside the script, the program sees the first; `--config` names a third, which wins.
#[tokio::test]
async fn the_configuration_comes_from_the_invocation() {
    let case = Case::new();
    case.write_global(&global_config("127.0.0.1:1".parse().unwrap()));
    case.write(".agents/outrig/config.toml", &repo_config("from-cwd"));
    case.write(
        "elsewhere/.agents/outrig/config.toml",
        &repo_config("from-script"),
    );
    case.write("chosen.toml", &repo_config("from-flag"));
    case.write(
        "elsewhere/which.py",
        "import outrig\nview = outrig.config()\nprint(view.default_model, sorted(view.models))\n",
    );

    let invocation = case.run(&["run-new", "elsewhere/which.py"]).await;
    assert_eq!(
        invocation.status.code(),
        Some(0),
        "{}",
        text(&invocation.stderr)
    );
    assert_eq!(text(&invocation.stdout), "from-cwd ['from-cwd']\n");

    let chosen = case
        .run(&["--config", "chosen.toml", "run-new", "elsewhere/which.py"])
        .await;
    assert_eq!(chosen.status.code(), Some(0), "{}", text(&chosen.stderr));
    assert_eq!(text(&chosen.stdout), "from-flag ['from-flag']\n");
}

/// In a repository whose directory name is not UTF-8, which Linux allows, a declared workspace is
/// a launcher diagnostic and status 1 with nothing started -- `outrig.config()` could not carry
/// its path -- not a panic. A configuration there that declares no workspace runs the program.
#[tokio::test]
async fn a_workspace_path_that_is_not_utf8_is_refused_and_none_still_runs() {
    let case = Case::new();
    let repo = case.path().join(OsStr::from_bytes(b"repo-\xff"));
    std::fs::create_dir_all(repo.join("ws")).unwrap();
    std::fs::create_dir_all(repo.join(".agents/outrig")).unwrap();
    std::fs::write(
        repo.join("probe.py"),
        "import outrig\nprint(outrig.config().workspace)\n",
    )
    .unwrap();
    let config = repo.join(".agents/outrig/config.toml");
    let run = || {
        let mut command = case.outrig(&["run-new", "probe.py"]);
        command.current_dir(&repo);
        async move {
            timeout(TEST_TIMEOUT, command.output())
                .await
                .expect("outrig timed out")
                .expect("outrig starts")
        }
    };

    std::fs::write(&config, "[workspace]\nhost-path = \"ws\"\n").unwrap();
    let refused = run().await;
    assert_eq!(
        (refused.status.code(), text(&refused.stderr)),
        (
            Some(1),
            format!(
                "error: configuration: [workspace] host-path resolves to {:?}, which is not \
                 UTF-8 and so cannot be shown as outrig.config().workspace\n",
                repo.join("ws")
            )
        )
    );
    assert_eq!(text(&refused.stdout), "");

    std::fs::write(&config, "").unwrap();
    let ran = run().await;
    assert_eq!(ran.status.code(), Some(0), "{}", text(&ran.stderr));
    assert_eq!(text(&ran.stdout), "None\n");
    assert_eq!(case.runtime_calls(), "");
}

/// Wait up to `within` for `done`, failing with `what` it waited for.
async fn until(within: Duration, done: impl Fn() -> bool, what: impl Fn() -> String) {
    let deadline = Instant::now() + within;
    while !done() {
        assert!(Instant::now() < deadline, "no {} within {within:?}", what());
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Wait for `path` to exist.
async fn appears(path: &Path) {
    until(
        TEST_TIMEOUT,
        || path.exists(),
        || path.display().to_string(),
    )
    .await;
}

/// `SIGINT` and `SIGTERM` sent to `outrig` reach the program: one with handlers says so and
/// exits 0, and outrig with it; one without ends by the signal, and outrig exits 128 plus its
/// number.
#[tokio::test]
async fn signals_sent_to_outrig_reach_the_program() {
    let case = Case::new();
    case.write(
        "handles.py",
        "import signal, sys, time\n\
         def handle(number, frame):\n    print('got', signal.Signals(number).name)\n    sys.exit(0)\n\
         signal.signal(signal.SIGINT, handle)\n\
         signal.signal(signal.SIGTERM, handle)\n\
         open(sys.argv[1], 'w').close()\n\
         time.sleep(60)\n",
    );
    case.write(
        "ignores.py",
        "import sys, time\nopen(sys.argv[1], 'w').close()\ntime.sleep(60)\n",
    );
    for signal in [Signal::SIGINT, Signal::SIGTERM] {
        for (script, code, stdout) in [
            ("handles.py", 0, format!("got {signal}\n")),
            ("ignores.py", 128 + signal as i32, String::new()),
        ] {
            let ready = case.path().join(format!("{script}-{signal}.ready"));
            let child = case
                .outrig(&["run-new", script, ready.to_str().unwrap()])
                .spawn()
                .expect("outrig starts");
            appears(&ready).await;
            kill(Pid::from_raw(child.id().unwrap() as i32), signal).unwrap();
            let ran = timeout(TEST_TIMEOUT, child.wait_with_output())
                .await
                .expect("outrig timed out")
                .unwrap();
            assert_eq!(
                (ran.status.code(), text(&ran.stdout)),
                (Some(code), stdout),
                "{script} {signal}: {}",
                text(&ran.stderr)
            );
        }
    }
}

/// A `SIGHUP` -- a closed terminal, a CI timeout -- reaches the program as the `SIGTERM` it asks
/// for, rather than ending `outrig` by the default action and leaving the engine that stops the
/// program's sessions with it (#644). A program that handles `SIGTERM` says so and exits 0; one
/// that does not is ended by it, and `outrig` exits with its 143.
#[tokio::test]
async fn a_sighup_to_outrig_reaches_the_program_as_a_sigterm() {
    let case = Case::new();
    case.write(
        "handles.py",
        "import signal, sys, time\n\
         def handle(number, frame):\n    print('got', signal.Signals(number).name)\n    sys.exit(0)\n\
         signal.signal(signal.SIGTERM, handle)\n\
         open(sys.argv[1], 'w').close()\n\
         time.sleep(60)\n",
    );
    case.write(
        "ignores.py",
        "import sys, time\nopen(sys.argv[1], 'w').close()\ntime.sleep(60)\n",
    );
    for (script, code, stdout) in [
        ("handles.py", 0, "got SIGTERM\n".to_string()),
        ("ignores.py", 128 + Signal::SIGTERM as i32, String::new()),
    ] {
        let ready = case.path().join(format!("{script}.hup-ready"));
        let child = case
            .outrig(&["run-new", script, ready.to_str().unwrap()])
            .spawn()
            .expect("outrig starts");
        appears(&ready).await;
        kill(Pid::from_raw(child.id().unwrap() as i32), Signal::SIGHUP).unwrap();
        let ran = timeout(TEST_TIMEOUT, child.wait_with_output())
            .await
            .expect("outrig timed out")
            .unwrap();
        assert_eq!(
            (ran.status.code(), text(&ran.stdout)),
            (Some(code), stdout),
            "{script}: {}",
            text(&ran.stderr)
        );
    }
}

/// `kill -9` of `outrig` ends the program and what it started: its whole process group.
#[tokio::test]
async fn killing_outrig_ends_the_programs_group() {
    let case = Case::new();
    case.write(
        "waits.py",
        "import os, subprocess, sys, time\n\
         subprocess.Popen(['sleep', '60'])\n\
         with open(sys.argv[1] + '.tmp', 'w') as f:\n    f.write(str(os.getpgrp()))\n\
         os.rename(sys.argv[1] + '.tmp', sys.argv[1])\n\
         time.sleep(60)\n",
    );
    let reported = case.path().join("pgid");
    let mut child = case
        .outrig(&["run-new", "waits.py", reported.to_str().unwrap()])
        .spawn()
        .expect("outrig starts");
    appears(&reported).await;
    let pgid: i32 = std::fs::read_to_string(&reported).unwrap().parse().unwrap();

    child.start_kill().unwrap();
    child.wait().await.unwrap();
    until(
        Duration::from_secs(10),
        || kill(Pid::from_raw(-pgid), None) == Err(Errno::ESRCH),
        || format!("end of the program's group {pgid}"),
    )
    .await;
}

// ------------------------------------------------------------------------------- the terminal

/// `outrig` on a terminal of its own: the leader of a new session whose controlling terminal is
/// a fresh pty, holding its foreground, as a shell's foreground job does. The pty's other end, and
/// everything written to the terminal, gathered as it arrives.
struct OnTerminal {
    child: tokio::process::Child,
    master: std::fs::File,
    seen: Arc<Mutex<String>>,
}

impl OnTerminal {
    fn start(case: &Case, args: &[&str]) -> Self {
        let pty = nix::pty::openpty(None, None).expect("openpty");
        let slave: OwnedFd = pty.slave;
        let stdio = |fd: &OwnedFd| Stdio::from(fd.try_clone().expect("dup the pty"));
        // Not `Case::outrig`'s group of its own: a group leader cannot `setsid`.
        let mut command = case.outrig_in_this_group(args);
        command
            .stdin(stdio(&slave))
            .stdout(stdio(&slave))
            .stderr(stdio(&slave));
        // SAFETY: two async-signal-safe calls between fork and exec.
        unsafe {
            command.pre_exec(|| {
                nix::unistd::setsid()?;
                if nix::libc::ioctl(0, nix::libc::TIOCSCTTY, 0) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = command.spawn().expect("outrig starts");
        drop(slave);
        let master = std::fs::File::from(pty.master);
        let seen = Arc::new(Mutex::new(String::new()));
        let mut reader = master.try_clone().expect("dup the master");
        let into = Arc::clone(&seen);
        std::thread::spawn(move || {
            let mut buf = [0; 4096];
            // `EIO` once the last slave descriptor closes.
            while let Ok(n @ 1..) = reader.read(&mut buf) {
                into.lock()
                    .unwrap()
                    .push_str(&String::from_utf8_lossy(&buf[..n]));
            }
        });
        Self {
            child,
            master,
            seen,
        }
    }

    async fn saw(&self, text: &str) {
        let seen = || self.seen.lock().unwrap().clone();
        until(
            TEST_TIMEOUT,
            || seen().contains(text),
            || format!("{text:?} on the terminal: {:?}", seen()),
        )
        .await;
    }

    async fn exit(mut self) -> (std::process::ExitStatus, String) {
        let status = timeout(TEST_TIMEOUT, self.child.wait())
            .await
            .expect("outrig timed out")
            .unwrap();
        let seen = self.seen.lock().unwrap().clone();
        (status, seen)
    }
}

/// A program does not read the terminal: run from one, its read fails at once with `EIO` rather
/// than stopping it and leaving `outrig` waiting. Writing to it works.
#[tokio::test]
async fn a_terminal_read_fails_at_once() {
    let case = Case::new();
    case.write(
        "reads.py",
        "import errno, os\nprint('before the read', flush=True)\n\
         try:\n    os.read(0, 100)\nexcept OSError as e:\n    \
         print('read failed:', errno.errorcode[e.errno])\n",
    );
    let terminal = OnTerminal::start(&case, &["run-new", "reads.py"]);
    let (status, seen) = terminal.exit().await;
    assert_eq!(status.code(), Some(0), "{seen}");
    assert!(
        seen.contains("before the read") && seen.contains("read failed: EIO"),
        "{seen:?}"
    );
}

/// Ctrl-C at the terminal reaches `outrig`, whose group holds the terminal, and `outrig` forwards
/// it to the program.
#[tokio::test]
async fn ctrl_c_at_the_terminal_reaches_the_program() {
    let case = Case::new();
    case.write(
        "handles.py",
        "import signal, sys, time\n\
         def handle(number, frame):\n    print('got', signal.Signals(number).name, flush=True)\n    \
         sys.exit(0)\n\
         signal.signal(signal.SIGINT, handle)\n\
         print('ready', flush=True)\n\
         time.sleep(60)\n",
    );
    let mut terminal = OnTerminal::start(&case, &["run-new", "handles.py"]);
    terminal.saw("ready").await;
    terminal.master.write_all(b"\x03").unwrap();
    let (status, seen) = terminal.exit().await;
    assert_eq!(status.code(), Some(0), "{seen}");
    assert!(seen.contains("got SIGINT"), "{seen:?}");
}

// ------------------------------------------------------------------------- the grace and sessions

/// Start `outrig run-new` with `args`, wait for the program to raise `ready`, and send `outrig` each
/// of `signals`, a fifth of a second apart. How it ended, and how long after the first signal.
async fn signalled(
    case: &Case,
    args: &[&str],
    ready: &Path,
    signals: &[Signal],
) -> (Output, Duration) {
    let child = case.outrig(args).spawn().expect("outrig starts");
    appears(ready).await;
    let pid = Pid::from_raw(child.id().unwrap() as i32);
    let first = Instant::now();
    for (n, signal) in signals.iter().enumerate() {
        if n > 0 {
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        kill(pid, *signal).unwrap();
    }
    let ran = timeout(TEST_TIMEOUT, child.wait_with_output())
        .await
        .expect("outrig timed out")
        .unwrap();
    (ran, first.elapsed())
}

/// A program that ignores `SIGINT` is stopped once its grace has passed -- `SIGTERM`, which ends
/// it -- and the grace is `--grace`, else `[host] grace-secs`, else five seconds; the flag wins
/// over the configuration. `--grace` is the program's, and `outrig run-new` without a script refuses it.
#[tokio::test]
async fn the_grace_is_the_flags_else_the_configurations() {
    let case = Case::new();
    case.write(
        "ignores.py",
        "import signal, sys, time\n\
         signal.signal(signal.SIGINT, signal.SIG_IGN)\n\
         open(sys.argv[1], 'w').close()\n\
         time.sleep(60)\n",
    );
    let ready = case.path().join("ready");
    let ready_arg = ready.to_str().unwrap();
    for (config, flag, at_least, below) in [
        ("", Some("1"), 1, 4),
        ("[host]\ngrace-secs = 2\n", None, 2, 5),
        ("[host]\ngrace-secs = 30\n", Some("1"), 1, 4),
    ] {
        let _ = std::fs::remove_file(&ready);
        case.write(".agents/outrig/config.toml", config);
        let mut args = vec!["run-new"];
        if let Some(flag) = flag {
            args.extend(["--grace", flag]);
        }
        args.extend(["ignores.py", ready_arg]);
        let (ran, waited) = signalled(&case, &args, &ready, &[Signal::SIGINT]).await;
        assert_eq!(
            ran.status.code(),
            Some(143),
            "{config:?} {flag:?}: {}",
            text(&ran.stderr)
        );
        assert!(
            waited >= Duration::from_secs(at_least) && waited < Duration::from_secs(below),
            "{config:?} {flag:?}: {waited:?}"
        );
    }

    let refused = case.run(&["run-new", "--grace", "1"]).await;
    assert_eq!(refused.status.code(), Some(2), "{}", text(&refused.stderr));
    assert!(
        text(&refused.stderr).contains("<SCRIPT>"),
        "{}",
        text(&refused.stderr)
    );
}

/// A second signal ends the grace at once: a program that ignores `SIGINT` is stopped at the
/// second, well inside a minute's grace.
#[tokio::test]
async fn a_second_signal_ends_the_grace_at_once() {
    let case = Case::new();
    case.write(
        "ignores.py",
        "import signal, sys, time\n\
         signal.signal(signal.SIGINT, signal.SIG_IGN)\n\
         open(sys.argv[1], 'w').close()\n\
         time.sleep(60)\n",
    );
    let ready = case.path().join("ready");
    let (ran, waited) = signalled(
        &case,
        &[
            "run-new",
            "--grace",
            "60",
            "ignores.py",
            ready.to_str().unwrap(),
        ],
        &ready,
        &[Signal::SIGINT, Signal::SIGINT],
    )
    .await;
    assert_eq!(ran.status.code(), Some(143), "{}", text(&ran.stderr));
    assert!(waited < Duration::from_secs(5), "{waited:?}");
}

/// Making an `outrig.Session` asks the container runtime for nothing; entering it does. With the
/// runtime's stubs failing every call, the entry fails, saying so, and the program goes on.
#[tokio::test]
async fn a_session_asks_the_runtime_for_nothing_until_it_is_entered() {
    let case = Case::new();
    case.write(
        ".agents/outrig/config.toml",
        "default-image = \"alpine\"\n\n[images.alpine]\nimage-name = \"docker.io/library/alpine:latest\"\n",
    );
    case.write(
        "enters.py",
        "import asyncio, os, outrig, sys, time\n\
         async def main():\n\
         \x20   session = outrig.Session()\n\
         \x20   open(sys.argv[1], 'w').close()\n\
         \x20   while not os.path.exists(sys.argv[2]):\n\
         \x20       await asyncio.sleep(0.01)\n\
         \x20   try:\n\
         \x20       async with session:\n\
         \x20           print('entered')\n\
         \x20   except RuntimeError:\n\
         \x20       print('refused')\n\
         asyncio.run(main())\n",
    );
    let (made, go) = (case.path().join("made"), case.path().join("go"));
    let child = case
        .outrig(&[
            "run-new",
            "enters.py",
            made.to_str().unwrap(),
            go.to_str().unwrap(),
        ])
        .spawn()
        .expect("outrig starts");
    appears(&made).await;
    assert_eq!(
        case.runtime_calls(),
        "",
        "made, and nothing asked of the runtime"
    );
    std::fs::write(&go, "").unwrap();
    let ran = timeout(TEST_TIMEOUT, child.wait_with_output())
        .await
        .expect("outrig timed out")
        .unwrap();
    assert_eq!(ran.status.code(), Some(0), "{}", text(&ran.stderr));
    assert_eq!(text(&ran.stdout), "refused\n");
    assert!(
        case.runtime_calls().starts_with("podman "),
        "{}",
        case.runtime_calls()
    );
}

/// A host directory mounted twice fails the entry before anything is asked of the runtime.
#[tokio::test]
async fn a_repeated_mount_fails_entry_before_the_runtime_is_asked() {
    let case = Case::new();
    std::fs::create_dir(case.path().join("data")).unwrap();
    case.write(
        "twice.py",
        "import asyncio, outrig\n\
         async def main():\n\
         \x20   try:\n\
         \x20       async with outrig.Session(mounts=[outrig.Mount('data', '/a'), outrig.Mount('data', '/b')]):\n\
         \x20           print('entered')\n\
         \x20   except RuntimeError as e:\n\
         \x20       print(e)\n\
         asyncio.run(main())\n",
    );
    let ran = case.run(&["run-new", "twice.py"]).await;
    assert_eq!(ran.status.code(), Some(0), "{}", text(&ran.stderr));
    let data = case.path().join("data");
    assert!(
        text(&ran.stdout).starts_with(&format!(
            "workspace mount host-path \"{}\" is mounted more than once",
            data.canonicalize().unwrap_or(data.clone()).display()
        )) || text(&ran.stdout).starts_with(&format!(
            "workspace mount host-path \"{}\" is mounted more than once",
            data.display()
        )),
        "{}",
        text(&ran.stdout)
    );
    assert_eq!(case.runtime_calls(), "");
}
