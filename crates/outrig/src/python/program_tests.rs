//! A host program against the real payload: what it sees of how it was started, what it imports,
//! how it exits, its streams and descriptors, the signals forwarded to it, the view of the
//! configuration it asks for, and its end when the engine's end of the connection closes.
//!
//! Each program runs in a directory of its own, which is also its working directory, with its
//! stdout and stderr written to files there, so a test reads them after the program exits.

use std::ffi::{OsStr, OsString};
use std::os::fd::AsRawFd as _;
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::process::{CommandExt as _, ExitStatusExt as _};
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::time::Duration;

use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use serde_json::{Value, json};
use tokio::process::Command;

use super::program;
use super::testing::{
    Flag, alive, eventually, host_home, package, py, python, wait_group_gone, within,
};
use crate::config::Config;
use crate::harness::{HostProgram, RunningProgram};
use crate::owner::tests::Testing;
use crate::owner::view;

/// A directory a program runs in: its scripts, its working directory, and its captured streams.
struct Case {
    dir: tempfile::TempDir,
}

/// How a program ended, and what it wrote.
struct Ran {
    status: ExitStatus,
    stdout: String,
    stderr: String,
}

impl Case {
    fn new() -> Self {
        Self {
            dir: tempfile::tempdir().expect("tempdir"),
        }
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    /// Write `source`, dedented, to `name` under the directory, making its parents.
    fn write(&self, name: &str, source: &str) -> PathBuf {
        let path = self.path().join(name);
        std::fs::create_dir_all(path.parent().expect("a parent")).expect("mkdir");
        std::fs::write(&path, py(source)).expect("write");
        path
    }

    /// Start `script`, as given, with `args`, `config` as the configuration of a repository at
    /// the directory, working in `cwd` -- the directory when `None` -- and `adjust` applied
    /// after the defaults: the test `HOME`, stdin from `stdin` in the directory when it exists,
    /// and stdout and stderr to files there.
    async fn start_with(
        &self,
        script: &str,
        args: &[&str],
        config: Config,
        cwd: Option<&Path>,
        adjust: impl FnOnce(&mut Command) + Send + 'static,
    ) -> RunningProgram {
        let dir = self.path().to_path_buf();
        let args: Vec<OsString> = args.iter().map(OsString::from).collect();
        let defaults = move |cmd: &mut Command| {
            let file = |name: &str| std::fs::File::create(dir.join(name)).expect("a stream file");
            let stdin =
                std::fs::File::open(dir.join("stdin")).map_or_else(|_| Stdio::null(), Stdio::from);
            cmd.env("HOME", host_home())
                .env_remove("PYTHONUSERBASE")
                .stdin(stdin)
                .stdout(file("stdout"))
                .stderr(file("stderr"));
            adjust(cmd);
        };
        let testing = Testing {
            cwd: Some(cwd.unwrap_or(self.path()).to_path_buf()),
            adjust: Some(Box::new(defaults)),
            ..Testing::default()
        };
        within(
            HostProgram::new(config, script)
                .args(args)
                .repo_root(self.path())
                .testing(testing)
                .start(),
        )
        .await
        .unwrap_or_else(|e| panic!("{e}"))
    }

    async fn start(&self, script: &str, args: &[&str]) -> RunningProgram {
        self.start_with(script, args, Config::default(), None, |_| {})
            .await
    }

    /// Wait for `program`, and read what it wrote.
    async fn finish(&self, mut program: RunningProgram) -> Ran {
        let status = within(program.wait()).await.expect("wait").status();
        let read = |name: &str| std::fs::read_to_string(self.path().join(name)).unwrap_or_default();
        Ran {
            status,
            stdout: read("stdout"),
            stderr: read("stderr"),
        }
    }

    async fn run(&self, script: &str, args: &[&str]) -> Ran {
        let program = self.start(script, args).await;
        self.finish(program).await
    }
}

/// The one JSON line a probe printed.
fn json_line(ran: &Ran) -> Value {
    serde_json::from_str(ran.stdout.trim())
        .unwrap_or_else(|e| panic!("{e}: stdout {:?}, stderr {:?}", ran.stdout, ran.stderr))
}

// ------------------------------------------------------------------------------- what it sees

/// `sys.argv` is the script as given and everything after it, unchanged; `__name__` is
/// `"__main__"`, `__file__` the script, and the working directory the invocation's.
#[tokio::test]
async fn argv_name_file_and_cwd_are_the_programs() {
    let case = Case::new();
    case.write(
        "bin/worker.py",
        r#"
        import json, os, sys
        print(json.dumps({"argv": sys.argv, "name": __name__, "file": __file__,
                          "cwd": os.getcwd()}))
        "#,
    );
    let args = ["./workspace", "Fix the tests", "", "--help", "-v", "--"];
    let ran = case.run("bin/worker.py", &args).await;
    assert_eq!(ran.status.code(), Some(0), "{}", ran.stderr);
    let mut argv = vec!["bin/worker.py"];
    argv.extend(args);
    assert_eq!(
        json_line(&ran),
        json!({
            "argv": argv,
            "name": "__main__",
            "file": "bin/worker.py",
            "cwd": case.path().canonicalize().unwrap(),
        })
    );
}

/// A script outside the working directory imports a module beside it; the working directory,
/// `PYTHONPATH`, the user site and `PYTHONUSERBASE` add nothing; the standard library comes
/// first, the script's directory last, and `outrig` imports from OutRig's own package.
#[tokio::test]
async fn imports_follow_the_script_and_nothing_in_the_environment() {
    let case = Case::new();
    case.write("elsewhere/prog.py", PROBE_IMPORTS);
    case.write("elsewhere/beside.py", "VALUE = 'beside'");
    case.write("cwd/incwd.py", "");
    case.write("pythonpath/inpythonpath.py", "");
    case.write("home/.local/lib/python3.13/site-packages/inusersite.py", "");
    case.write("userbase/lib/python3.13/site-packages/inuserbase.py", "");
    let root = case.path().to_path_buf();
    let cwd = root.join("cwd");
    let program = case
        .start_with(
            "../elsewhere/prog.py",
            &[],
            Config::default(),
            Some(&cwd),
            move |cmd| {
                cmd.env("PYTHONPATH", root.join("pythonpath"))
                    .env("HOME", root.join("home"))
                    .env("PYTHONUSERBASE", root.join("userbase"));
            },
        )
        .await;
    let ran = case.finish(program).await;
    assert_eq!(ran.status.code(), Some(0), "{}", ran.stderr);
    let found = json_line(&ran);
    assert_eq!(found["beside"], "beside");
    for module in ["incwd", "inpythonpath", "inusersite", "inuserbase"] {
        assert_eq!(found[module], false, "{module} imported: {found}");
    }
    let payload = python().parent().unwrap().parent().unwrap();
    let path: Vec<PathBuf> = serde_json::from_value(found["path"].clone()).unwrap();
    assert!(
        path[0].starts_with(payload),
        "sys.path[0] is not the standard library's: {path:?}"
    );
    let elsewhere = case.path().join("elsewhere").canonicalize().unwrap();
    assert_eq!(path.last(), Some(&elsewhere), "{path:?}");
    assert!(
        !path
            .iter()
            .any(|entry| entry.as_os_str().is_empty() || entry.ends_with("cwd"))
    );
    assert!(
        Path::new(found["outrig"].as_str().unwrap()).starts_with(package()),
        "{found}"
    );
}

const PROBE_IMPORTS: &str = r#"
    import importlib, json, sys
    import beside, outrig

    def imports(name):
        try:
            importlib.import_module(name)
            return True
        except ImportError:
            return False

    found = {name: imports(name) for name in ("incwd", "inpythonpath", "inusersite", "inuserbase")}
    print(json.dumps({"beside": beside.VALUE, "path": sys.path, "outrig": outrig.__file__, **found}))
    "#;

// ----------------------------------------------------------------------------------- its exit

/// The status is the program's: 0 off the end, `sys.exit`'s, and 1 for an uncaught exception,
/// whose traceback is Python's own, beginning at the script with nothing of the bootstrap.
#[tokio::test]
async fn the_exit_status_and_traceback_are_the_programs() {
    let case = Case::new();
    case.write("fine.py", "print('done')");
    case.write("two.py", "import sys\nsys.exit(2)");
    case.write("raises.py", "def f():\n    raise ValueError('boom')\nf()");
    case.write("syntax.py", "x = (");

    let fine = case.run("fine.py", &[]).await;
    assert_eq!(
        (fine.status.code(), fine.stdout.as_str()),
        (Some(0), "done\n")
    );
    assert_eq!(case.run("two.py", &[]).await.status.code(), Some(2));

    let raised = case.run("raises.py", &[]).await;
    assert_eq!(raised.status.code(), Some(1));
    assert!(
        raised
            .stderr
            .starts_with("Traceback (most recent call last):\n  File \"raises.py\", line 3"),
        "{}",
        raised.stderr
    );
    assert!(
        raised.stderr.ends_with("ValueError: boom\n"),
        "{}",
        raised.stderr
    );

    let syntax = case.run("syntax.py", &[]).await;
    assert_eq!(syntax.status.code(), Some(1));
    assert!(
        syntax.stderr.starts_with("  File \"syntax.py\", line 1\n"),
        "{}",
        syntax.stderr
    );
    for stderr in [&raised.stderr, &syntax.stderr] {
        assert!(
            !stderr.contains("<program>")
                && !stderr.contains("runpy")
                && !stderr.contains("<string>"),
            "{stderr}"
        );
    }
}

/// An uncaught `KeyboardInterrupt` ends the program by `SIGINT`, as Python ends any program.
#[tokio::test]
async fn an_uncaught_keyboard_interrupt_ends_the_program_by_sigint() {
    let case = Case::new();
    case.write("interrupted.py", "raise KeyboardInterrupt");
    let ran = case.run("interrupted.py", &[]).await;
    assert_eq!(
        ran.status.signal(),
        Some(Signal::SIGINT as i32),
        "{}",
        ran.stderr
    );
    assert!(ran.stderr.contains("KeyboardInterrupt"), "{}", ran.stderr);
}

/// A script that is missing, or a directory, is refused before anything starts, saying which.
#[tokio::test]
async fn a_script_that_is_not_a_file_is_refused() {
    let case = Case::new();
    std::fs::create_dir(case.path().join("adir")).unwrap();
    for (script, why) in [
        (case.path().join("missing.py"), "No such file or directory"),
        (case.path().join("adir"), "it is not a file"),
    ] {
        let err = HostProgram::new(Config::default(), &script)
            .start()
            .await
            .expect_err("refused");
        let text = err.to_string();
        assert!(
            text.starts_with(&format!("cannot run the program {}: ", script.display()))
                && text.contains(why),
            "{text}"
        );
    }
}

// ------------------------------------------------------------------- its streams and descriptors

/// The program's stdio carries nothing of the protocol: binary junk and a line that looks like a
/// request on stdout, fd 1 closed and reopened, a line read from stdin -- and `outrig.config()`
/// still answers afterward.
#[tokio::test]
async fn the_streams_are_the_programs_and_carry_no_protocol() {
    let case = Case::new();
    std::fs::write(
        case.path().join("stdin"),
        "a line for the program\nand more\n",
    )
    .unwrap();
    case.write(
        "streams.py",
        r#"
        import os, sys, outrig
        os.write(1, bytes(range(256)) * 64)
        os.write(1, b'{"t": "config", "id": 1}\n')
        os.close(1)
        assert os.open("reopened", os.O_WRONLY | os.O_CREAT | os.O_TRUNC) == 1
        line = sys.stdin.readline()
        os.write(1, f"read {line!r}\n".encode())
        os.write(1, f"events {outrig.config().events.mode}\n".encode())
        "#,
    );
    let ran = case.run("streams.py", &[]).await;
    assert_eq!(ran.status.code(), Some(0), "{}", ran.stderr);
    let stdout = std::fs::read(case.path().join("stdout")).unwrap();
    assert!(stdout.starts_with(&(0..=255).collect::<Vec<u8>>()));
    assert!(stdout.ends_with(b"{\"t\": \"config\", \"id\": 1}\n"));
    assert_eq!(
        std::fs::read_to_string(case.path().join("reopened")).unwrap(),
        "read 'a line for the program\\n'\nevents off\n"
    );
}

/// What the program starts holds neither of its connection's descriptors -- with `Popen`'s
/// default of closing them, and without -- and the program's exit is seen while those children
/// live on.
#[tokio::test]
async fn what_the_program_starts_holds_neither_owner_descriptor() {
    let case = Case::new();
    case.write(
        "spawns.py",
        r#"
        import json, os, subprocess
        import outrig._host
        pipes = [os.readlink(f"/proc/self/fd/{fd}") for fd in outrig._host._connection.fds]
        kids = [subprocess.Popen(["sleep", "60"]).pid,
                subprocess.Popen(["sleep", "60"], close_fds=False).pid]
        print(json.dumps({"pipes": pipes, "kids": kids}))
        "#,
    );
    let ran = case.run("spawns.py", &[]).await;
    assert_eq!(ran.status.code(), Some(0), "{}", ran.stderr);
    let found = json_line(&ran);
    let pipes: Vec<String> = serde_json::from_value(found["pipes"].clone()).unwrap();
    assert!(
        pipes.iter().all(|pipe| pipe.starts_with("pipe:[")),
        "{pipes:?}"
    );
    for kid in found["kids"].as_array().unwrap() {
        let kid = Pid::from_raw(kid.as_i64().unwrap() as i32);
        assert!(alive(kid), "the program's exit waited for {kid}");
        let held: Vec<String> = std::fs::read_dir(format!("/proc/{kid}/fd"))
            .unwrap()
            .map(|fd| {
                let link = std::fs::read_link(fd.unwrap().path()).unwrap_or_default();
                link.to_string_lossy().into_owned()
            })
            .collect();
        assert!(
            !held.iter().any(|link| pipes.contains(link)),
            "{kid} holds {held:?}, among them one of {pipes:?}"
        );
        let _ = kill(kid, Signal::SIGKILL);
    }
}

/// `asyncio.run` works, twice: nothing of the connection runs on the program's main thread.
#[tokio::test]
async fn asyncio_runs_and_runs_again() {
    let case = Case::new();
    case.write(
        "loops.py",
        r#"
        import asyncio, outrig

        async def main(n):
            await asyncio.sleep(0.01)
            view = await asyncio.to_thread(outrig.config)
            return f"{n} {view.events.mode}"

        print(asyncio.run(main(1)))
        print(asyncio.run(main(2)))
        "#,
    );
    let ran = case.run("loops.py", &[]).await;
    assert_eq!(ran.status.code(), Some(0), "{}", ran.stderr);
    assert_eq!(ran.stdout, "1 off\n2 off\n");
}

/// A child the program forks has no connection: `outrig.config()` there says so at once rather
/// than waiting on a reply the program's reader would take.
#[tokio::test]
async fn a_forked_child_has_no_connection() {
    let case = Case::new();
    case.write(
        "forks.py",
        r#"
        import os, outrig
        pid = os.fork()
        if pid == 0:
            try:
                outrig.config()
            except RuntimeError as e:
                print(e, flush=True)
            os._exit(0)
        os.waitpid(pid, 0)
        print(outrig.config().events.mode)
        "#,
    );
    let ran = case.run("forks.py", &[]).await;
    assert_eq!(ran.status.code(), Some(0), "{}", ran.stderr);
    assert_eq!(
        ran.stdout,
        "outrig.config() answers in the program `outrig run-new` started, not in a process it \
         forked\n\
         off\n"
    );
}

// ------------------------------------------------------------------------------------- signals

/// `SIGINT` and `SIGTERM` reach the program's group: a program with handlers says so and exits
/// 0, and the `sleep` it started, which has none, ends.
#[tokio::test]
async fn a_forwarded_signal_reaches_the_programs_group() {
    for signal in [Signal::SIGINT, Signal::SIGTERM] {
        let case = Case::new();
        let ready = Flag::new();
        case.write(
            "handles.py",
            &format!(
                r#"
                import signal, subprocess, sys, time
                kid = subprocess.Popen(["sleep", "60"])
                print(kid.pid, flush=True)
                def handle(number, frame):
                    print("got", signal.Signals(number).name, flush=True)
                    kid.wait()
                    sys.exit(0)
                signal.signal(signal.SIGINT, handle)
                signal.signal(signal.SIGTERM, handle)
                open({ready}, "w").close()
                time.sleep(60)
                "#,
                ready = ready.py(),
            ),
        );
        let program = case.start("handles.py", &[]).await;
        ready.wait();
        forward(&program, signal);
        let ran = case.finish(program).await;
        assert_eq!(ran.status.code(), Some(0), "{signal}: {}", ran.stderr);
        let mut lines = ran.stdout.lines();
        let kid = Pid::from_raw(lines.next().unwrap().parse().unwrap());
        assert_eq!(lines.next(), Some(format!("got {signal}").as_str()));
        assert!(
            !alive(kid),
            "{signal} did not reach the program's child {kid}"
        );
    }
}

/// A program without handlers ends by the forwarded signal itself.
#[tokio::test]
async fn a_program_without_handlers_ends_by_the_forwarded_signal() {
    for signal in [Signal::SIGINT, Signal::SIGTERM] {
        let case = Case::new();
        let ready = Flag::new();
        case.write(
            "sleeps.py",
            &format!(
                "import time\nopen({}, 'w').close()\ntime.sleep(60)",
                ready.py()
            ),
        );
        let program = case.start("sleeps.py", &[]).await;
        ready.wait();
        forward(&program, signal);
        let ran = case.finish(program).await;
        assert_eq!(ran.status.signal(), Some(signal as i32), "{}", ran.stderr);
    }
}

/// Forward `signal`, as an owner forwards the one it received.
fn forward(program: &RunningProgram, signal: Signal) {
    match signal {
        Signal::SIGINT => program.interrupt(),
        Signal::SIGTERM => program.terminate(),
        other => panic!("{other} is not forwarded"),
    }
}

/// Answer the greeting of a bootstrap started by hand, over the engine's ends of its pipes, as
/// the engine would, so the script runs.
fn greet(
    engine_reads: &mut std::io::BufReader<std::io::PipeReader>,
    engine_writes: &mut std::io::PipeWriter,
) {
    use std::io::{BufRead as _, Write as _};
    let mut hello = String::new();
    engine_reads
        .read_line(&mut hello)
        .expect("the program's greeting");
    let hello: Value = serde_json::from_str(&hello).expect("a greeting");
    assert_eq!(hello["t"], "hello", "{hello}");
    let answer = json!({"t": "hello", "id": hello["id"], "protocol": hello["protocol"]});
    writeln!(engine_writes, "{answer}").expect("answer the greeting");
}

// ----------------------------------------------------------------------------- the engine's end

/// The end of the connection is the engine's death: the program and what it started end,
/// whatever the program was doing, and it says why on its stderr.
#[test]
fn the_connections_end_ends_the_program_and_its_group() {
    let case = Case::new();
    let ready = Flag::new();
    case.write(
        "waits.py",
        &format!(
            "import subprocess, time\nsubprocess.Popen(['sleep', '60'])\nopen({}, 'w').close()\n\
             time.sleep(60)",
            ready.py()
        ),
    );
    let (program_reads, mut engine_writes) = std::io::pipe().unwrap();
    let (engine_reads, program_writes) = std::io::pipe().unwrap();
    let mut engine_reads = std::io::BufReader::new(engine_reads);
    let inherited = [program_reads.as_raw_fd(), program_writes.as_raw_fd()];
    let stderr = std::fs::File::create(case.path().join("stderr")).unwrap();
    let mut child = program::bootstrap(python(), package(), inherited, "waits.py".as_ref(), &[])
        .current_dir(case.path())
        .env("HOME", host_home())
        .env_remove("PYTHONUSERBASE")
        .stdin(Stdio::null())
        .stderr(stderr)
        .process_group(0)
        .spawn()
        .expect("the bootstrap starts");
    drop((program_reads, program_writes));
    greet(&mut engine_reads, &mut engine_writes);
    ready.wait();
    let pgid = Pid::from_raw(child.id() as i32);

    drop(engine_writes);
    let status = eventually(
        || child.try_wait().expect("try_wait"),
        || "the program's end".to_string(),
    );
    assert_eq!(status.signal(), Some(Signal::SIGKILL as i32));
    wait_group_gone(pgid, Duration::from_secs(5));
    let stderr = std::fs::read_to_string(case.path().join("stderr")).unwrap();
    assert!(
        stderr.contains("outrig-program: the engine that runs this program is gone"),
        "{stderr}"
    );
}

/// The engine's end ends the program even with its stderr a full pipe nobody drains: no
/// diagnostic on the way holds up the signals -- through the program's death handler, and through
/// the reader's own `SIGKILL` when a handler of the program's takes the signal instead.
#[test]
fn the_connections_end_ends_the_program_whatever_its_stderr_holds() {
    for handler in ["outrig's", "the program's"] {
        let case = Case::new();
        let ready = Flag::new();
        case.write(
            "blocked.py",
            &format!(
                r#"
                import os, signal, subprocess, sys, time
                if sys.argv[1] == "the program's":
                    signal.signal(signal.SIGHUP, lambda *_: None)
                subprocess.Popen(["sleep", "60"])
                os.set_blocking(2, False)
                try:
                    while True:
                        os.write(2, b"x" * 4096)
                except BlockingIOError:
                    pass
                os.set_blocking(2, True)
                open({ready}, "w").close()
                time.sleep(60)
                "#,
                ready = ready.py(),
            ),
        );
        let (program_reads, mut engine_writes) = std::io::pipe().unwrap();
        let (engine_reads, program_writes) = std::io::pipe().unwrap();
        let mut engine_reads = std::io::BufReader::new(engine_reads);
        let (_undrained, stderr) = std::io::pipe().unwrap();
        let inherited = [program_reads.as_raw_fd(), program_writes.as_raw_fd()];
        let args = [OsString::from(handler)];
        let mut child =
            program::bootstrap(python(), package(), inherited, "blocked.py".as_ref(), &args)
                .current_dir(case.path())
                .env("HOME", host_home())
                .env_remove("PYTHONUSERBASE")
                .stdin(Stdio::null())
                .stderr(stderr)
                .process_group(0)
                .spawn()
                .expect("the bootstrap starts");
        drop((program_reads, program_writes));
        greet(&mut engine_reads, &mut engine_writes);
        ready.wait();
        let pgid = Pid::from_raw(child.id() as i32);

        drop(engine_writes);
        let status = eventually(
            || child.try_wait().expect("try_wait"),
            || format!("the program's end, with {handler} handler"),
        );
        assert_eq!(status.signal(), Some(Signal::SIGKILL as i32), "{handler}");
        wait_group_gone(pgid, Duration::from_secs(5));
    }
}

/// The script's module is `__main__` for the program's whole life: an exit handler finds the
/// script's classes there, and pickles their instances, as under `python3 <script>`.
#[tokio::test]
async fn the_scripts_module_stays_main_through_shutdown() {
    let case = Case::new();
    case.write(
        "exits.py",
        r#"
        import atexit, pickle

        class Point:
            def __init__(self, x):
                self.x = x

        def at_exit():
            import __main__
            print(__main__.Point is Point, pickle.loads(pickle.dumps(Point(3))).x)

        atexit.register(at_exit)
        "#,
    );
    let ran = case.run("exits.py", &[]).await;
    assert_eq!(
        (ran.status.code(), ran.stdout.as_str(), ran.stderr.as_str()),
        (Some(0), "True 3\n", "")
    );
}

/// The view's workspace is the directory a session mounts: the declared path resolved against
/// its base, `..` and all, which the filesystem resolves through a symlink as the mount does --
/// not a path with `..` removed by text, which would name another directory.
#[test]
fn the_views_workspace_is_the_directory_a_session_mounts() {
    let case = Case::new();
    for dir in ["actual/child", "actual/ws", "ws"] {
        std::fs::create_dir_all(case.path().join(dir)).unwrap();
    }
    std::os::unix::fs::symlink(case.path().join("actual/child"), case.path().join("link")).unwrap();
    let config = Config::load_from_str("[workspace]\nhost-path = \"link/../ws\"\n").unwrap();
    let view = view(&config, case.path()).unwrap();
    let workspace = PathBuf::from(view["workspace"].as_str().unwrap());
    assert_eq!(workspace, config.workspace.resolved_host_path(case.path()));
    assert_eq!(
        workspace.canonicalize().unwrap(),
        case.path().join("actual/ws").canonicalize().unwrap()
    );
}

/// A declared workspace whose path is not UTF-8 -- a directory Linux allows -- is a configuration
/// error, not a panic: the view is JSON, which holds text, and a replaced byte would name another
/// directory. In the same directory, a configuration that declares no workspace has a view.
#[test]
fn a_workspace_path_that_is_not_utf8_is_refused() {
    let root = Path::new(OsStr::from_bytes(b"/repo-\xff"));
    let declared = Config::load_from_str("[workspace]\nhost-path = \"ws\"\n").unwrap();
    assert_eq!(
        view(&declared, root).unwrap_err().to_string(),
        r#"configuration: [workspace] host-path resolves to "/repo-\xFF/ws", which is not UTF-8 and so cannot be shown as outrig.config().workspace"#
    );
    assert_eq!(
        view(&Config::default(), root).unwrap()["workspace"],
        Value::Null
    );
}

// ------------------------------------------------------------------------------- the view

const CONFIG: &str = r#"
default-model = "fast"

[providers.local]
style    = "chat-completions"
base-url = "http://127.0.0.1:1/v1"
api-key  = "${OUTRIG_TEST_PROGRAM_KEY}"

[models.fast]
provider       = "local"
identifier     = "fast-wire-name"
context-window = 32000
max-tokens     = 4000

[models.slow]
provider   = "local"
identifier = "slow-wire-name"

[models.either]
alias = ["fast", "slow"]

[models.chain]
alias = ["either", "fast"]

[workspace]
host-path = "work/../ws"

[events]
mode = "record"
"#;

/// `outrig.config()` shows each model with its effective identifier and window, each alias with
/// its candidates in order, each provider's style and base URL, the defaults, the resolved
/// workspace and the events mode -- frozen -- and holds no key nor the variable naming it.
#[tokio::test]
async fn the_view_holds_the_configuration_and_no_secret() {
    let case = Case::new();
    std::fs::create_dir(case.path().join("ws")).unwrap();
    let config = Config::load_from_str(CONFIG).unwrap();
    case.write(
        "views.py",
        r#"
        import dataclasses, outrig
        view = outrig.config()
        print(sorted(view.models), sorted(view.aliases), sorted(view.providers))
        print(view.models["fast"])
        print(view.models["slow"])
        print(view.aliases["chain"])
        print(view.providers["local"])
        print(view.default_model, view.workspace, view.events)
        try:
            view.models["new"] = None
        except TypeError:
            print("read-only")
        try:
            view.default_model = "other"
        except dataclasses.FrozenInstanceError:
            print("frozen")
        print(repr(view))
        "#,
    );
    let program = case
        .start_with("views.py", &[], config, None, |cmd| {
            cmd.env("OUTRIG_TEST_PROGRAM_KEY", "sk-the-secret-value");
        })
        .await;
    let ran = case.finish(program).await;
    assert_eq!(ran.status.code(), Some(0), "{}", ran.stderr);
    let workspace = case.path().join("work/../ws");
    let lines: Vec<&str> = ran.stdout.lines().collect();
    assert_eq!(
        lines[..8],
        [
            "['fast', 'slow'] ['chain', 'either'] ['local']",
            "Model(name='fast', provider='local', identifier='fast-wire-name', \
             context_window=32000, max_tokens=4000)",
            "Model(name='slow', provider='local', identifier='slow-wire-name', \
             context_window=None, max_tokens=None)",
            "Alias(name='chain', candidates=('fast', 'slow'))",
            "Provider(name='local', style='chat-completions', base_url='http://127.0.0.1:1/v1')",
            &format!("fast {} Events(mode='record')", workspace.display()),
            "read-only",
            "frozen",
        ]
    );
    for secret in ["sk-the-secret-value", "OUTRIG_TEST_PROGRAM_KEY"] {
        assert!(!ran.stdout.contains(secret), "{secret} in {}", ran.stdout);
    }
}

/// Without a configured workspace, the view's is `None`; a row whose identifier the
/// configuration leaves out shows its own name, which is what OutRig would send.
#[test]
fn the_view_falls_back_to_the_rows_name_and_no_workspace() {
    let config = Config::load_from_str(
        r#"
        [providers.local]
        style    = "anthropic"
        base-url = "http://127.0.0.1:1"
        api-key  = "${OUTRIG_TEST_PROGRAM_KEY}"

        [models.bare]
        provider = "local"
        "#,
    )
    .unwrap();
    let view = view(&config, Path::new("/repo")).unwrap();
    assert_eq!(
        view,
        json!({
            "models": {"bare": {"provider": "local", "identifier": "bare",
                                "context_window": null, "max_tokens": null}},
            "aliases": {},
            "providers": {"local": {"style": "anthropic", "base_url": "http://127.0.0.1:1"}},
            "default_model": null,
            "workspace": null,
            "events": {"mode": "off"},
        })
    );
}
