//! Host programs that own sessions, against the real payload: a program run as `outrig run`
//! runs it, whose sessions the engine starts on the host -- an interpreter with no container,
//! as `SessionBuilder::start_on_host` starts one -- or, in the e2e module, in containers.
//!
//! What a test does inside a session it reaches through the session's interpreter, which the
//! hooks here hand it by the program's number for the session: it opens a kernel there and runs
//! code in it, as an agent would.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use crate::harness::{Progress, Session, SessionBuilder};
use crate::python::host::Interpreter;
use crate::python::program::Adjust;

/// How a test starts a program and its sessions.
#[derive(Default)]
pub(crate) struct Testing {
    /// The program's working directory, which its script is found from.
    pub(crate) cwd: Option<PathBuf>,
    /// Adjustments to the program's command: its environment, its stdio.
    pub(crate) adjust: Option<Adjust>,
    /// The `outrig` package the program imports, in place of this build's.
    pub(crate) package: Option<PathBuf>,
    pub(crate) sessions: Sessions,
}

impl fmt::Debug for Testing {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Testing").finish_non_exhaustive()
    }
}

/// What a test changes about the program's launch.
pub(crate) struct Launching {
    pub(crate) cwd: Option<PathBuf>,
    pub(crate) adjust: Option<Adjust>,
    pub(crate) package: Option<PathBuf>,
}

impl Testing {
    pub(crate) fn split(self) -> (Launching, Sessions) {
        let Self {
            cwd,
            adjust,
            package,
            sessions,
        } = self;
        (
            Launching {
                cwd,
                adjust,
                package,
            },
            sessions,
        )
    }
}

/// How a test starts each session a program asks for.
#[derive(Default)]
pub(crate) struct Sessions {
    /// On the host, in place of a container.
    pub(crate) on_host: bool,
    /// Where bindings install and remember approvals, in place of the user's own.
    pub(crate) places: Option<Places>,
    /// Why each session's shutdown is to say it is not proven stopped.
    pub(crate) fail_stop: Option<String>,
    /// Each started session's interpreter.
    pub(crate) live: Live,
    /// Told of each step of each session's start, with the program's number for the session.
    pub(crate) progress: Option<Arc<dyn Fn(u64, Progress) + Send + Sync>>,
    /// Told, with the program's number for the session, when a start that did not finish begins
    /// to wait for what it made to be stopped.
    pub(crate) settling: Option<Arc<dyn Fn(u64) + Send + Sync>>,
}

/// Where a session's bindings install and remember approvals.
pub(crate) struct Places {
    pub(crate) packages: PathBuf,
    pub(crate) pip_env: Vec<(OsString, OsString)>,
    pub(crate) approvals: PathBuf,
}

impl Sessions {
    /// Whether sessions start on the host.
    pub(crate) fn on_host(&self) -> bool {
        self.on_host
    }

    /// `builder`, for the session the program numbered `handle`, whose id is `id` and whose log
    /// directory is `logs`, as this test starts it.
    pub(crate) fn adjust(
        &self,
        mut builder: SessionBuilder,
        handle: u64,
        id: &str,
        logs: &Path,
    ) -> SessionBuilder {
        if let Some(reason) = &self.fail_stop {
            builder = builder.fail_stop(reason.clone());
        }
        if let Some(places) = &self.places {
            builder =
                builder.binding_places(&places.packages, places.pip_env.clone(), &places.approvals);
        }
        if self.on_host {
            builder = builder.host_id(id).log_in(logs);
        }
        if let Some(progress) = &self.progress {
            let progress = Arc::clone(progress);
            builder = builder.progress(move |step| progress(handle, step));
        }
        builder
    }

    /// `session` started, as the program numbered `handle`.
    pub(crate) fn started(&self, handle: u64, session: &Session) {
        self.live
            .0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(handle, session.interpreter().clone());
    }

    /// See [`Sessions::settling`].
    pub(crate) fn settling(&self, handle: u64) {
        if let Some(settling) = &self.settling {
            settling(handle);
        }
    }
}

/// Each started session's interpreter, by the program's number for the session.
#[derive(Clone, Default)]
pub(crate) struct Live(pub(crate) Arc<Mutex<BTreeMap<u64, Interpreter>>>);

impl fmt::Debug for Live {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Live")
    }
}

#[cfg(test)]
mod code {
    use std::os::unix::process::ExitStatusExt as _;
    use std::process::ExitStatus;

    use super::super::{LeftOpen, ProgramExit, SessionEnd};
    use crate::harness::Stopped;

    fn exit(raw: i32, left_open: Vec<LeftOpen>) -> ProgramExit {
        ProgramExit {
            status: ExitStatus::from_raw(raw),
            left_open,
        }
    }

    /// The owner exits with the program's status, or 128 plus the number of the signal that
    /// ended it, as a shell reports one: `sys.exit(1)` and a death by a signal are never the same.
    #[test]
    fn the_code_is_the_programs_or_128_plus_its_signal() {
        let code = |raw| exit(raw, Vec::new()).code();
        assert_eq!(code(0), 0);
        assert_eq!(code(2 << 8), 2);
        assert_eq!(code(1 << 8), 1);
        assert_eq!(code(2), 130);
        assert_eq!(code(15), 143);
        assert_eq!(code(9 | 0x80), 137, "a core dump is still the signal's");
    }

    /// A session the engine stopped for the program that is not proven stopped outranks the
    /// program's own status with 3: something it started may still be running. One proven
    /// stopped leaves the program's status as it is.
    #[test]
    fn a_session_left_open_and_not_proven_stopped_exits_3() {
        let left = |stopped| LeftOpen {
            id: "20261020T000000-abcd".to_string(),
            end: SessionEnd::NotStarted(stopped),
        };
        let not_proven = Stopped::NotProven {
            reason: "the primary container: timed out".to_string(),
        };
        assert_eq!(exit(2 << 8, vec![left(Stopped::Proven)]).code(), 2);
        assert_eq!(exit(2 << 8, vec![left(not_proven.clone())]).code(), 3);
        assert_eq!(
            exit(9, vec![left(Stopped::Proven), left(not_proven)]).code(),
            3
        );
    }
}

/// A program and the sessions it asks for, on the host: a directory holding its scripts, which
/// is its working directory and where its streams are written, beside a session root and a
/// global configuration.
struct Bed {
    _root: tempfile::TempDir,
    /// The program's directory.
    dir: PathBuf,
    sessions: PathBuf,
    repo: PathBuf,
    global: PathBuf,
    packages: PathBuf,
    approvals: PathBuf,
}

/// How a program ended, and what it wrote.
struct Ran {
    exit: crate::harness::ProgramExit,
    stdout: String,
    stderr: String,
}

impl Ran {
    /// Each line the program printed, as JSON.
    fn json(&self) -> Vec<serde_json::Value> {
        self.stdout
            .lines()
            .map(|line| {
                serde_json::from_str(line)
                    .unwrap_or_else(|e| panic!("{e}: {line:?}; stderr {}", self.stderr))
            })
            .collect()
    }
}

/// `[bindings.fx]`: the hosted-call fixture, installed from the test wheels.
const FIXTURE: &str = r#"
[bindings.fx]
description = "the fixture"
requires    = ["outrig_fixture"]
factory     = "outrig_fixture:make"
"#;

/// A binding whose factory raises, which fails a session's start once its bindings start.
const FAILING: &str = r#"
[bindings.fails]
description = "a factory that raises"
requires    = ["outrig_fixture"]
factory     = "outrig_fixture:make_failing"
"#;

/// What every program's scripts import to wait on, and raise, a flag in their directory.
const WAIT: &str = r#"
import asyncio, os, time

async def flag(name, timeout=30):
    deadline = time.monotonic() + timeout
    while not os.path.exists(name):
        if time.monotonic() > deadline:
            raise TimeoutError(f"{name} never appeared")
        await asyncio.sleep(0.01)

def raise_flag(name):
    open(name, "w").close()
"#;

impl Bed {
    fn new() -> Self {
        let root = tempfile::tempdir().expect("tempdir");
        let path = |name: &str| {
            let dir = root.path().join(name);
            std::fs::create_dir_all(&dir).expect("a directory");
            dir
        };
        let (dir, sessions, repo, home, packages, approvals) = (
            path("program"),
            path("sessions"),
            path("repo"),
            path("home"),
            path("packages"),
            path("approvals"),
        );
        std::fs::write(dir.join("wait.py"), crate::python::testing::py(WAIT)).expect("wait.py");
        Self {
            global: home.join("config.toml"),
            _root: root,
            dir,
            sessions,
            repo,
            packages,
            approvals,
        }
    }

    /// Write `source`, dedented, as the script `name`.
    fn write(&self, name: &str, source: &str) {
        std::fs::write(self.dir.join(name), crate::python::testing::py(source)).expect("write");
    }

    /// The configuration: the bed's session root, `extra`, and the fixture binding when
    /// `fixture`, all in the global file, which needs no approval.
    fn config(&self, extra: &str, fixture: bool) -> crate::config::Config {
        let fixture = if fixture { FIXTURE } else { "" };
        std::fs::write(
            &self.global,
            format!(
                "session-root = {:?}\n{extra}\n{fixture}",
                self.sessions.to_str().expect("a UTF-8 path")
            ),
        )
        .expect("write the global config");
        crate::config::Config::load(&self.repo, Some(&self.global))
            .unwrap_or_else(|e| panic!("{e}"))
    }

    fn flag(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }

    fn raise(&self, name: &str) {
        std::fs::write(self.flag(name), b"").expect("raise the flag");
    }

    /// Wait, without holding the runtime, for the program to raise `name`.
    async fn raised(&self, name: &str) {
        let flag = self.flag(name);
        crate::python::testing::within(async {
            while !flag.exists() {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await;
    }

    /// The sessions the bed's programs start: on the host, the fixture's packages installed here.
    fn sessions(&self) -> Sessions {
        Sessions {
            on_host: true,
            ..self.in_containers()
        }
    }

    /// The sessions the bed's programs start in containers, the fixture's packages installed
    /// here.
    fn in_containers(&self) -> Sessions {
        Sessions {
            places: Some(Places {
                packages: self.packages.clone(),
                pip_env: crate::python::testing::pip_env(wheels()),
                approvals: self.approvals.clone(),
            }),
            ..Sessions::default()
        }
    }

    /// Start the script `name`, with `program` adjusting what the bed sets: its working directory
    /// the bed's, the test `HOME`, its stdout and stderr to files in the bed.
    async fn start(
        &self,
        name: &str,
        config: crate::config::Config,
        sessions: Sessions,
        program: impl FnOnce(crate::harness::HostProgram) -> crate::harness::HostProgram,
    ) -> crate::harness::RunningProgram {
        let dir = self.dir.clone();
        let adjust = move |cmd: &mut tokio::process::Command| {
            let file = |name: &str| std::fs::File::create(dir.join(name)).expect("a stream file");
            cmd.env("HOME", crate::python::testing::host_home())
                .env_remove("PYTHONUSERBASE")
                .stdin(std::process::Stdio::null())
                .stdout(file("stdout"))
                .stderr(file("stderr"));
        };
        let testing = Testing {
            cwd: Some(self.dir.clone()),
            adjust: Some(Box::new(adjust)),
            package: None,
            sessions,
        };
        let host = crate::harness::HostProgram::new(config, name)
            .repo_root(&self.repo)
            .testing(testing);
        crate::python::testing::within(program(host).start())
            .await
            .unwrap_or_else(|e| panic!("{e}"))
    }

    /// Wait for `program` and every session it left open, and read what it wrote.
    async fn finish(&self, mut program: crate::harness::RunningProgram) -> Ran {
        let exit = tokio::time::timeout(std::time::Duration::from_secs(60), program.wait())
            .await
            .expect("the program and its sessions ended within a minute")
            .expect("wait");
        let read = |name: &str| std::fs::read_to_string(self.dir.join(name)).unwrap_or_default();
        Ran {
            exit,
            stdout: read("stdout"),
            stderr: read("stderr"),
        }
    }

    /// The session directories under the bed's session root.
    fn session_dirs(&self) -> Vec<PathBuf> {
        let mut dirs: Vec<PathBuf> = std::fs::read_dir(&self.sessions)
            .expect("read the session root")
            .map(|entry| entry.expect("an entry").path())
            .collect();
        dirs.sort();
        dirs
    }
}

/// The test wheels' directory, built off the test's runtime: building them unpacks the payload
/// on a runtime of its own, which cannot start inside another.
fn wheels() -> &'static Path {
    std::thread::scope(|scope| {
        scope
            .spawn(crate::python::testing::wheel_links)
            .join()
            .expect("the wheels build")
    })
}

/// The interpreter of the session `program` numbered `handle`.
fn interpreter(program: &crate::harness::RunningProgram, handle: u64) -> Interpreter {
    program
        .live
        .0
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(&handle)
        .cloned()
        .unwrap_or_else(|| panic!("no session {handle} has started"))
}

/// Open kernel `agent` in `interpreter`, as an agent the session runs, and run `source` there,
/// returning once the source has raised the flag `done` in the bed.
async fn run_in(bed: &Bed, interpreter: &Interpreter, agent: &str, source: &str, done: &str) {
    crate::python::testing::within(interpreter.open(agent))
        .await
        .unwrap_or_else(|e| panic!("{e}"));
    let source = format!(
        "{}\nopen({:?}, 'w').close()",
        crate::python::testing::py(source),
        bed.flag(done).to_str().expect("a UTF-8 path")
    );
    interpreter.send_line(serde_json::json!({"t": "exec", "agent": agent, "id": 1, "src": source}));
    bed.raised(done).await;
}

/// A session a program enters is one with no primary agent: construction asks for nothing, entry
/// returns the session and makes it the current one, and the session is starting, then idle,
/// then closing, then reported, with no agent of its own. An exit with nothing running is clean;
/// a body that raises still shuts the session down, its exception propagating with the report
/// set.
#[tokio::test]
async fn a_program_enters_a_session_with_no_primary_and_leaves_it_clean() {
    let bed = Bed::new();
    bed.write(
        "enters.py",
        r#"
        import asyncio, json, outrig, wait

        async def main():
            session = outrig.Session()
            wait.raise_flag("constructed")
            await wait.flag("go")
            records = session.events()
            async with session as entered:
                assert entered is session
                assert outrig.current_session() is session
                print(json.dumps({"id": session.id}), flush=True)
            seen = [record async for record in records]
            report = session.shutdown_report
            print(json.dumps({
                "verdict": report.verdict,
                "stopped": report.stopped.state,
                "executions": len(report.executions),
                "closed_by": report.closed_by.by,
                "states": [r.data["state"] for r in seen if r.type == "session.state"],
                "types": sorted({r.type for r in seen}),
            }))
            raising = outrig.Session()
            try:
                async with raising:
                    raise ValueError("the body")
            except ValueError as e:
                print(json.dumps({"raised": str(e), "verdict": raising.shutdown_report.verdict}))

        asyncio.run(main())
        "#,
    );
    let program = bed
        .start("enters.py", bed.config("", false), bed.sessions(), |p| p)
        .await;
    bed.raised("constructed").await;
    assert!(
        program.live.0.lock().unwrap().is_empty() && bed.session_dirs().is_empty(),
        "a session was started before entry"
    );
    bed.raise("go");
    let ran = bed.finish(program).await;
    assert_eq!(ran.exit.code(), 0, "{}", ran.stderr);
    assert!(
        ran.exit.left_open().is_empty(),
        "{:?}",
        ran.exit.left_open()
    );
    let lines = ran.json();
    let id = lines[0]["id"].as_str().expect("an id");
    assert!(bed.sessions.join(id).join("logs").is_dir(), "{id}");
    assert_eq!(bed.session_dirs().len(), 2, "one directory a session");
    assert_eq!(lines[1]["verdict"], "clean", "{}", ran.stdout);
    assert_eq!(lines[1]["stopped"], "proven");
    assert_eq!(lines[1]["executions"], 0);
    assert_eq!(lines[1]["closed_by"], "owner");
    assert_eq!(
        lines[1]["states"],
        serde_json::json!(["starting", "idle", "closing", "reported"])
    );
    let types: Vec<&str> = lines[1]["types"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t.as_str().unwrap())
        .collect();
    assert!(
        !types.contains(&"agent.started") && !types.contains(&"agent.stopped"),
        "{types:?}"
    );
    assert!(types.contains(&"session.report"), "{types:?}");
    assert_eq!(
        lines[2],
        serde_json::json!({"raised": "the body", "verdict": "clean"})
    );
}

/// A stop the engine cannot prove raises `outrig.ShutdownException` in the task that entered the
/// session, once the report is set: the exception carries the report, is an `Exception` and no
/// `AgentError`, and keeps an exception the body raised as its `__context__`.
#[tokio::test]
async fn a_stop_not_proven_raises_in_the_programs_task() {
    let bed = Bed::new();
    bed.write(
        "unproven.py",
        r#"
        import asyncio, json, outrig

        async def main():
            plain = outrig.Session()
            try:
                async with plain:
                    pass
            except outrig.ShutdownException as e:
                print(json.dumps({
                    "same": e.report is plain.shutdown_report,
                    "verdict": e.report.verdict,
                    "reason": e.report.stopped.reason,
                    "message": str(e),
                    "exception": isinstance(e, Exception),
                    "agent_error": type(e).__mro__[1].__name__,
                    "task": asyncio.current_task().get_name(),
                }))
            raising = outrig.Session()
            try:
                async with raising:
                    raise ValueError("the body")
            except outrig.ShutdownException as e:
                print(json.dumps({
                    "context": type(e.__context__).__name__,
                    "said": str(e.__context__),
                    "set": raising.shutdown_report is e.report,
                }))

        asyncio.run(main(), debug=False)
        "#,
    );
    let sessions = Sessions {
        fail_stop: Some("a container stop the test failed".to_string()),
        ..bed.sessions()
    };
    let program = bed
        .start("unproven.py", bed.config("", false), sessions, |p| p)
        .await;
    let ran = bed.finish(program).await;
    assert_eq!(
        ran.exit.code(),
        0,
        "the program caught both: {}",
        ran.stderr
    );
    let lines = ran.json();
    assert_eq!(lines[0]["same"], true, "{}", ran.stdout);
    assert_eq!(lines[0]["verdict"], "not_proven_stopped");
    assert_eq!(lines[0]["reason"], "a container stop the test failed");
    assert_eq!(
        lines[0]["message"],
        "the session is not proven stopped: a container stop the test failed"
    );
    assert_eq!(lines[0]["exception"], true);
    assert_eq!(
        lines[0]["agent_error"], "Exception",
        "a direct subclass of Exception"
    );
    assert_eq!(
        lines[1],
        serde_json::json!({"context": "ValueError", "said": "the body", "set": true})
    );
}

/// `outrig.current_session()` is the innermost session entered in the running context: set at
/// entry, seen by a task and a `to_thread` call started there, not by a `threading.Thread`, and
/// restored to the outer one at exit, a raising body's included. Outside every session it raises.
#[tokio::test]
async fn the_current_session_is_the_innermost_entered_here() {
    let bed = Bed::new();
    bed.write(
        "nested.py",
        r#"
        import asyncio, json, outrig, threading

        def current():
            try:
                return outrig.current_session()
            except RuntimeError as e:
                return str(e)

        async def main():
            seen = {"before": current()}
            async with outrig.Session() as outer:
                seen["entered"] = current() is outer
                async def look():
                    return current()
                seen["task"] = await asyncio.create_task(look()) is outer
                seen["to_thread"] = await asyncio.to_thread(current) is outer
                found = []
                thread = threading.Thread(target=lambda: found.append(current()))
                thread.start()
                thread.join()
                seen["thread"] = found[0]
                async with outrig.Session() as inner:
                    seen["inner"] = current() is inner
                seen["restored"] = current() is outer
                try:
                    async with outrig.Session():
                        raise ValueError
                except ValueError:
                    pass
                seen["after_raise"] = current() is outer
            seen["after"] = current()
            print(json.dumps(seen))

        asyncio.run(main())
        "#,
    );
    let program = bed
        .start("nested.py", bed.config("", false), bed.sessions(), |p| p)
        .await;
    let ran = bed.finish(program).await;
    assert_eq!(ran.exit.code(), 0, "{}", ran.stderr);
    let outside = "no outrig.Session is entered here: enter one with \
                   `async with outrig.Session()`, or pass the session where it is needed";
    assert_eq!(
        ran.json()[0],
        serde_json::json!({
            "before": outside,
            "entered": true,
            "task": true,
            "to_thread": true,
            "thread": outside,
            "inner": true,
            "restored": true,
            "after_raise": true,
            "after": outside,
        })
    );
}

/// A session's events start with `starting`, then `idle`. A reader that stops while more events
/// arrive than it holds -- here eight -- resumes to a `Missed` counting what it lost, the records
/// after it numbered on from there; after the session's exit it reads what was held and the
/// iterator ends, at `reported`. One iterator a session: a second `events()` raises.
#[tokio::test]
async fn a_reader_that_falls_behind_resumes_to_what_it_missed() {
    let bed = Bed::new();
    bed.write(
        "behind.py",
        r#"
        import asyncio, json, outrig, outrig._host, wait

        outrig._host.EVENT_CAPACITY = 8

        async def main():
            session = outrig.Session()
            records = session.events()
            async with session:
                first = [await anext(records), await anext(records)]
                print(json.dumps([[r.type, r.data, r.id] for r in first]), flush=True)
                wait.raise_flag("reading-stopped")
                await wait.flag("calls-made")
            rest = [record async for record in records]
            gap = rest[0]
            print(json.dumps({
                "missed": gap.count if isinstance(gap, outrig.Missed) else None,
                "next": rest[1].id,
                "last": [rest[-1].type, rest[-1].data],
                "held": len(rest) - 1,
                "report_missed": list(session.shutdown_report.events.missed),
            }))
            try:
                session.events()
            except RuntimeError as e:
                print(json.dumps({"again": str(e)}))

        asyncio.run(main())
        "#,
    );
    let program = bed
        .start("behind.py", bed.config("", true), bed.sessions(), |p| p)
        .await;
    bed.raised("reading-stopped").await;
    let interpreter = interpreter(&program, 1);
    run_in(
        &bed,
        &interpreter,
        "a",
        "for _ in range(10):\n    fx.thread_name()",
        "a-done",
    )
    .await;
    bed.raise("calls-made");
    let ran = bed.finish(program).await;
    assert_eq!(ran.exit.code(), 0, "{}", ran.stderr);
    let lines = ran.json();
    assert_eq!(lines[0][0][0], "session.state", "{}", ran.stdout);
    assert_eq!(lines[0][0][1], serde_json::json!({"state": "starting"}));
    assert_eq!(lines[0][1][1], serde_json::json!({"state": "idle"}));
    let last_read = lines[0][1][2].as_u64().unwrap();
    let missed = lines[1]["missed"].as_u64().expect("a Missed first");
    assert!(missed > 0, "{}", ran.stdout);
    assert_eq!(lines[1]["next"].as_u64().unwrap(), last_read + missed + 1);
    assert_eq!(
        lines[1]["last"],
        serde_json::json!(["session.state", {"state": "reported"}])
    );
    assert_eq!(lines[1]["held"], 8, "what was held was read");
    assert_eq!(
        lines[1]["report_missed"],
        serde_json::json!([missed]),
        "the report counts what this reader lost"
    );
    assert_eq!(
        lines[2]["again"],
        "a session's events are read through one iterator, already made"
    );
}

/// `close_admission()` returns once the gate is closed: a call a kernel makes of a binding
/// afterward is refused, admission closed, while one already running when it closed finishes.
/// The report lists both, with the binding's process group proven empty, whether or not the
/// program read the session's events.
#[tokio::test]
async fn close_admission_returns_once_later_calls_are_refused() {
    for reading in [true, false] {
        let bed = Bed::new();
        bed.write(
            "closes.py",
            r#"
            import asyncio, json, outrig, sys, wait

            async def main():
                session = outrig.Session()
                if sys.argv[1] == "read":
                    records = session.events()
                    async def drain():
                        return [r async for r in records]
                    reader = asyncio.create_task(drain())
                async with session:
                    wait.raise_flag("entered")
                    await wait.flag("calling")
                    await session.close_admission()
                    wait.raise_flag("closed")
                    await wait.flag("finished")
                report = session.shutdown_report
                print(json.dumps({
                    "verdict": report.verdict,
                    "hosted": [[c.agent, c.binding, c.outcome, c.reason] for c in report.hosted],
                    "bindings": [list(report.bindings.names), report.bindings.group_empty],
                }))

            asyncio.run(main())
            "#,
        );
        let argv = if reading { "read" } else { "ignore" };
        let program = bed
            .start("closes.py", bed.config("", true), bed.sessions(), |p| {
                p.args([argv])
            })
            .await;
        bed.raised("entered").await;
        let interpreter = interpreter(&program, 1);
        let (started, release) = (bed.flag("started"), bed.flag("release"));
        crate::python::testing::within(interpreter.open("a"))
            .await
            .unwrap_or_else(|e| panic!("{e}"));
        interpreter.send_line(serde_json::json!({
            "t": "exec", "agent": "a", "id": 1,
            "src": format!("fx.record(started={started:?}, until={release:?})"),
        }));
        bed.raised("started").await;
        bed.raise("calling");
        bed.raised("closed").await;
        run_in(
            &bed,
            &interpreter,
            "b",
            "try:\n    fx.thread_name()\nexcept Exception as e:\n    refused = type(e).__name__",
            "b-done",
        )
        .await;
        bed.raise("release");
        bed.raise("finished");
        let ran = bed.finish(program).await;
        assert_eq!(ran.exit.code(), 0, "{argv}: {}", ran.stderr);
        let report = &ran.json()[0];
        assert_eq!(report["verdict"], "clean", "{argv}: {report}");
        assert_eq!(
            report["hosted"],
            serde_json::json!([
                ["b", "fx", "refused", "admission_closed"],
                ["a", "fx", "returned", null],
            ]),
            "{argv}: in the order the outcomes came"
        );
        assert_eq!(
            report["bindings"],
            serde_json::json!([["fx"], true]),
            "{argv}"
        );
    }
}

/// What `program` left open, by session id: whether it was shut down, and its verdict.
fn left_open(exit: &crate::harness::ProgramExit) -> Vec<(String, String)> {
    exit.left_open()
        .iter()
        .map(|left| {
            let end = match &left.end {
                crate::harness::SessionEnd::ShutDown(report) => {
                    format!("shut down: {:?}", report.verdict())
                }
                crate::harness::SessionEnd::NotStarted(stopped) => {
                    format!("not started: {stopped:?}")
                }
            };
            (left.id.clone(), end)
        })
        .collect()
}

/// A program that ends with a session open -- returning, raising, or killed with `SIGKILL` --
/// leaves it to the engine: admission closed, the session shut down with the default drain,
/// its report the program's exit carries, the binding's group proven empty. The status is the
/// program's own, the report being proven stopped.
#[tokio::test]
async fn the_engine_shuts_down_what_a_program_left_open() {
    for (how, status) in [("return", 0), ("raise", 1), ("kill", 137)] {
        let bed = Bed::new();
        bed.write(
            "leaves.py",
            r#"
            import asyncio, json, os, outrig, sys, time, wait

            async def main():
                session = outrig.Session()
                await session.__aenter__()
                print(json.dumps({"id": session.id, "pid": os.getpid()}), flush=True)
                if sys.argv[1] == "raise":
                    raise RuntimeError("left with a session open")
                if sys.argv[1] == "kill":
                    wait.raise_flag("entered")
                    time.sleep(60)

            asyncio.run(main())
            "#,
        );
        let program = bed
            .start("leaves.py", bed.config("", true), bed.sessions(), |p| {
                p.args([how])
            })
            .await;
        if how == "kill" {
            bed.raised("entered").await;
            let pid: i32 = serde_json::from_str::<serde_json::Value>(
                std::fs::read_to_string(bed.flag("stdout"))
                    .unwrap()
                    .lines()
                    .next()
                    .unwrap(),
            )
            .unwrap()["pid"]
                .as_i64()
                .unwrap() as i32;
            nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(pid),
                nix::sys::signal::Signal::SIGKILL,
            )
            .expect("kill the program");
        }
        let started = std::time::Instant::now();
        let ran = bed.finish(program).await;
        assert!(
            started.elapsed() < std::time::Duration::from_secs(30),
            "{how}: {:?}",
            started.elapsed()
        );
        assert_eq!(ran.exit.code(), status, "{how}: {}", ran.stderr);
        let id = ran.json()[0]["id"].as_str().unwrap().to_string();
        assert_eq!(
            left_open(&ran.exit),
            [(id, "shut down: Clean".to_string())],
            "{how}"
        );
        let crate::harness::SessionEnd::ShutDown(report) = &ran.exit.left_open()[0].end else {
            unreachable!()
        };
        let bindings = report.bindings.as_ref().expect("the fixture's process");
        assert!(bindings.group_empty, "{how}: {bindings:?}");
        assert_eq!(report.closed_by, crate::harness::ClosedBy::Owner, "{how}");
    }
}

/// Two sessions entered at once in one program each start, each runs a kernel the test opens,
/// and each stops with a report of its own; two entered one after the other do the same, the
/// second under a second `asyncio.run`.
#[tokio::test]
async fn sessions_run_side_by_side_and_one_after_another() {
    let bed = Bed::new();
    bed.write(
        "two.py",
        r#"
        import asyncio, json, outrig, wait

        async def held(name):
            async with outrig.Session() as session:
                wait.raise_flag(f"{name}-entered")
                await wait.flag("ran")
            return [session.id, session.shutdown_report.verdict,
                    [[c.agent, c.outcome] for c in session.shutdown_report.hosted]]

        async def side_by_side():
            return await asyncio.gather(held("one"), held("two"))

        async def once():
            async with outrig.Session() as session:
                pass
            return [session.id, session.shutdown_report.verdict]

        print(json.dumps(asyncio.run(side_by_side())), flush=True)
        print(json.dumps([asyncio.run(once()), asyncio.run(once())]))
        "#,
    );
    let program = bed
        .start("two.py", bed.config("", true), bed.sessions(), |p| p)
        .await;
    bed.raised("one-entered").await;
    bed.raised("two-entered").await;
    let (first, second) = (interpreter(&program, 1), interpreter(&program, 2));
    assert!(
        first.version() == second.version(),
        "two interpreters of the one payload"
    );
    run_in(&bed, &first, "a", "fx.thread_name()", "a-ran").await;
    run_in(&bed, &second, "b", "fx.thread_name()", "b-ran").await;
    bed.raise("ran");
    let ran = bed.finish(program).await;
    assert_eq!(ran.exit.code(), 0, "{}", ran.stderr);
    let lines = ran.json();
    let ids: Vec<&str> = lines[0]
        .as_array()
        .unwrap()
        .iter()
        .chain(lines[1].as_array().unwrap())
        .map(|session| session[0].as_str().unwrap())
        .collect();
    let distinct: std::collections::BTreeSet<_> = ids.iter().collect();
    assert_eq!(distinct.len(), 4, "a session id each: {ids:?}");
    assert_eq!(lines[0][0][1], "clean");
    assert_eq!(lines[0][1][1], "clean");
    assert_eq!(lines[1][0][1], "clean");
    assert_eq!(lines[1][1][1], "clean");
    assert_eq!(bed.session_dirs().len(), 4);
}

/// `[events] mode = "record"` writes the session's events to `events.jsonl` in its directory
/// under the session root, readable by its owner alone, each line what the program read as a
/// record; `off` writes no such file, and the program's own stream runs all the same.
#[tokio::test]
async fn a_session_records_its_events_only_when_the_configuration_asks() {
    use std::os::unix::fs::PermissionsExt as _;

    for mode in ["record", "off"] {
        let bed = Bed::new();
        bed.write(
            "records.py",
            r#"
            import asyncio, json, outrig

            async def main():
                session = outrig.Session()
                records = session.events()
                async with session:
                    pass
                seen = [[r.id, r.type] async for r in records]
                print(json.dumps({"id": session.id, "seen": seen}))

            asyncio.run(main())
            "#,
        );
        let config = bed.config(&format!("[events]\nmode = {mode:?}\n"), false);
        let program = bed.start("records.py", config, bed.sessions(), |p| p).await;
        let ran = bed.finish(program).await;
        assert_eq!(ran.exit.code(), 0, "{mode}: {}", ran.stderr);
        let line = &ran.json()[0];
        let logs = bed.sessions.join(line["id"].as_str().unwrap()).join("logs");
        let log = logs.join("events.jsonl");
        let seen = line["seen"].as_array().unwrap();
        assert_eq!(seen.first().unwrap()[1], "session.state");
        if mode == "off" {
            assert!(!log.exists(), "off wrote {}", log.display());
            assert!(logs.is_dir(), "the session's directory is there");
            continue;
        }
        let mode_bits = std::fs::metadata(&log).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode_bits, 0o600, "owner-only");
        let recorded = crate::events::recorded(&logs);
        let recorded: Vec<serde_json::Value> = recorded
            .iter()
            .map(|record| {
                let kind = record["type"].as_str().unwrap();
                serde_json::json!([
                    record["id"].as_str().unwrap().parse::<u64>().unwrap(),
                    kind.strip_prefix("org.outrig.").unwrap()
                ])
            })
            .collect();
        assert_eq!(
            &recorded, seen,
            "the file and the program read the same records"
        );
        let source = format!("/outrig/session/{}", line["id"].as_str().unwrap());
        assert_eq!(crate::events::recorded(&logs)[0]["source"], source.as_str());
    }
}

/// What a program asks to mount is checked at entry, before anything starts: a host directory
/// mounted twice -- among its own mounts, or as the workspace and a mount -- a container path
/// taken twice, a configured mount's included, and a workspace that is not a directory each fail
/// the entry with `RuntimeError` naming the path, and leave no session behind.
#[tokio::test]
async fn paths_a_program_cannot_mount_fail_its_entry() {
    let bed = Bed::new();
    let shared = bed.dir.join("shared");
    let other = bed.dir.join("other");
    for dir in [&shared, &other] {
        std::fs::create_dir(dir).unwrap();
    }
    bed.write(
        "mounts.py",
        r#"
        import asyncio, json, outrig

        async def refused(**inputs):
            try:
                async with outrig.Session(**inputs):
                    return "entered"
            except RuntimeError as e:
                return str(e)

        async def main():
            print(json.dumps([
                await refused(mounts=["shared", "shared"]),
                await refused(mounts=[outrig.Mount("shared", "/a"), outrig.Mount("shared", "/b")]),
                await refused(workspace="shared", mounts=["shared"]),
                await refused(mounts=[outrig.Mount("other", "/data")]),
                await refused(workspace="missing"),
            ]))

        asyncio.run(main())
        "#,
    );
    let configured = format!(
        "[[workspace.mounts]]\nhost-path = {:?}\ncontainer-path = \"/data\"\n",
        shared.to_str().unwrap()
    );
    let program = bed
        .start(
            "mounts.py",
            bed.config(&configured, false),
            bed.sessions(),
            |p| p,
        )
        .await;
    let ran = bed.finish(program).await;
    assert_eq!(ran.exit.code(), 0, "{}", ran.stderr);
    let said = &ran.json()[0];
    let shared = shared.to_str().unwrap();
    let expect = [
        format!("workspace mount container-path \"{shared}\" is declared more than once"),
        format!("workspace mount host-path \"{shared}\" is mounted more than once"),
        format!("workspace mount host-path \"{shared}\" is mounted more than once"),
        "workspace mount container-path \"/data\" is declared more than once".to_string(),
        format!(
            "workspace host-path \"{}\" does not exist",
            bed.dir.join("missing").display()
        ),
    ];
    assert_eq!(said.as_array().unwrap().len(), expect.len(), "{said}");
    for (said, expect) in said.as_array().unwrap().iter().zip(&expect) {
        assert!(
            said.as_str().unwrap().starts_with(expect.as_str()),
            "{said} is not {expect}"
        );
    }
    assert!(ran.exit.left_open().is_empty());
}

/// With a binding configured, every directory a program's session mounts appears in the
/// container at its host path, as a configuration's must: a mount at another container path, or
/// a workspace the program gives in place of one the configuration mounts at its own host path,
/// fails the entry before anything starts -- no package installed, no factory run.
#[tokio::test]
async fn a_program_with_bindings_mounts_each_directory_at_its_host_path() {
    let bed = Bed::new();
    let (data, configured) = (bed.dir.join("data"), bed.dir.join("configured"));
    for dir in [&data, &configured] {
        std::fs::create_dir(dir).unwrap();
    }
    bed.write(
        "binding_paths.py",
        r#"
        import asyncio, json, outrig

        async def refused(**inputs):
            try:
                async with outrig.Session(**inputs):
                    return "entered"
            except RuntimeError as e:
                return str(e)

        async def main():
            print(json.dumps([
                await refused(mounts=[outrig.Mount("data", "/elsewhere")]),
                await refused(workspace="data"),
            ]))

        asyncio.run(main())
        "#,
    );
    let started = bed.flag("started");
    let sessions = Sessions {
        progress: Some(Arc::new(move |_, _| {
            std::fs::write(&started, b"").expect("raise the flag");
        })),
        ..bed.sessions()
    };
    let workspace = format!(
        "[workspace]\nhost-path = {0:?}\ncontainer-path = {0:?}\n",
        configured.to_str().unwrap()
    );
    let program = bed
        .start(
            "binding_paths.py",
            bed.config(&workspace, true),
            sessions,
            |p| p,
        )
        .await;
    let ran = bed.finish(program).await;
    assert_eq!(ran.exit.code(), 0, "{}", ran.stderr);
    let said = &ran.json()[0];
    let (data, configured) = (data.to_str().unwrap(), configured.to_str().unwrap());
    let expect = [
        format!(
            "workspace.mounts[0].container-path \"/elsewhere\" is not its host path \"{data}\""
        ),
        format!("workspace.container-path \"{configured}\" is not its host path \"{data}\""),
    ];
    assert_eq!(said.as_array().unwrap().len(), expect.len(), "{said}");
    for (said, expect) in said.as_array().unwrap().iter().zip(&expect) {
        assert!(
            said.as_str().unwrap().starts_with(expect.as_str()),
            "{said} does not start {expect:?}"
        );
    }
    assert!(!bed.flag("started").exists(), "a start began");
}

/// A program whose `outrig` package speaks another version of the owner protocol is refused at
/// its greeting: the start fails saying so, and the script never runs.
#[tokio::test]
async fn a_program_speaking_another_protocol_is_refused_before_its_script_runs() {
    let bed = Bed::new();
    let package = bed.dir.join("package");
    let ours = crate::python::payload::package_dir()
        .await
        .expect("the package");
    std::fs::create_dir_all(package.join("outrig")).unwrap();
    for entry in std::fs::read_dir(ours.join("outrig")).unwrap() {
        let entry = entry.unwrap();
        if !entry.file_type().unwrap().is_file() {
            continue;
        }
        let text = std::fs::read_to_string(entry.path()).unwrap();
        let text = text.replace("\nPROTOCOL = 1\n", "\nPROTOCOL = 2\n");
        std::fs::write(package.join("outrig").join(entry.file_name()), text).unwrap();
    }
    bed.write("runs.py", "open('ran', 'w').close()");
    let testing = Testing {
        cwd: Some(bed.dir.clone()),
        package: Some(package),
        ..Testing::default()
    };
    let refused = crate::harness::HostProgram::new(bed.config("", false), "runs.py")
        .repo_root(&bed.repo)
        .testing(testing)
        .start()
        .await
        .expect_err("refused at the greeting");
    assert!(
        refused.to_string().contains(
            "it speaks version 2 of the owner protocol, and this engine speaks version 1"
        ),
        "{refused}"
    );
    assert!(!bed.flag("ran").exists(), "the script ran");
}

/// A progress hook that, when the session's start reaches `at`, raises the flag `reached` and
/// then holds the start until the flag `go-on` is raised: what lets a test end the program, or
/// give up on the entry, while the start is part way, with what it had made still held. The
/// hold is a blocking wait inside the start's poll, so it hands its worker's tasks to another
/// thread meanwhile, which needs a multi-threaded runtime.
fn holding_at(bed: &Bed, at: crate::harness::Step) -> Arc<dyn Fn(u64, Progress) + Send + Sync> {
    let (reached, go_on) = (bed.flag("reached"), bed.flag("go-on"));
    Arc::new(move |_, progress| {
        if progress == Progress::Started(at) {
            std::fs::write(&reached, b"").expect("raise the flag");
            tokio::task::block_in_place(|| {
                crate::python::testing::eventually(
                    || go_on.exists().then_some(()),
                    || "go-on".into(),
                );
            });
        }
    })
}

/// The pid a program printed on its first line.
fn printed_pid(bed: &Bed) -> nix::unistd::Pid {
    let first = std::fs::read_to_string(bed.flag("stdout")).expect("stdout");
    let line: serde_json::Value =
        serde_json::from_str(first.lines().next().expect("a first line")).expect("JSON");
    nix::unistd::Pid::from_raw(line["pid"].as_i64().expect("a pid") as i32)
}

/// A program killed while its session starts, with the session's bindings already up, has the
/// start interrupted where it stands: what it had made is stopped -- the bindings' process group
/// proven empty among it -- before the engine is done, and the session is reported as never
/// started, proven stopped. No session reaches the program, being gone.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_program_killed_while_its_session_starts_has_the_start_interrupted() {
    let bed = Bed::new();
    bed.write(
        "dies.py",
        r#"
        import asyncio, json, os, outrig

        async def main():
            print(json.dumps({"pid": os.getpid()}), flush=True)
            async with outrig.Session():
                print("entered", flush=True)

        asyncio.run(main())
        "#,
    );
    let sessions = Sessions {
        progress: Some(holding_at(&bed, crate::harness::Step::Python)),
        ..bed.sessions()
    };
    let program = bed
        .start("dies.py", bed.config("", true), sessions, |p| p)
        .await;
    bed.raised("reached").await;
    let pid = printed_pid(&bed);
    nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGKILL).expect("kill the program");
    // Reaped by the engine, which has then seen it gone.
    crate::python::testing::eventually(
        || (!std::path::Path::new(&format!("/proc/{pid}")).exists()).then_some(()),
        || format!("the reap of {pid}"),
    );
    bed.raise("go-on");
    let ran = bed.finish(program).await;
    assert_eq!(ran.exit.code(), 137, "{}", ran.stderr);
    assert!(!ran.stdout.contains("entered"), "{}", ran.stdout);
    let left = left_open(&ran.exit);
    assert_eq!(left.len(), 1, "{left:?}");
    assert_eq!(left[0].1, "not started: Proven", "{left:?}");
    assert!(
        bed.sessions.join(&left[0].0).is_dir(),
        "the interrupted session keeps its directory"
    );
}

/// An entry the program gives up on -- cancelled at a `SIGINT` while the engine starts the
/// session -- has the start interrupted too: the cancellation reaches the program once the engine
/// has stopped what the start had made, which the program is told, so nothing is left for its
/// exit. The program's grace is held for that cleanup as for a shutdown it asked for: here the
/// start has handed over a cleanup that runs past the whole grace, and the program, which gave up
/// at the signal, is not stopped for it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_entry_the_program_gives_up_on_interrupts_the_start() {
    use crate::process::Cmd;
    use crate::supervise::{CleanupGuard, Reissue};

    let bed = Bed::new();
    bed.write(
        "gives_up.py",
        r#"
        import asyncio, json, outrig, signal

        async def main():
            session = outrig.Session()
            entering = asyncio.create_task(session.__aenter__())
            asyncio.get_running_loop().add_signal_handler(signal.SIGINT, entering.cancel)
            try:
                await entering
                print(json.dumps({"entered": session.id}))
            except asyncio.CancelledError:
                print(json.dumps({"gave_up": session.id, "report": session.shutdown_report}))

        asyncio.run(main())
        "#,
    );
    let hold = holding_at(&bed, crate::harness::Step::Python);
    let sessions = Sessions {
        // A cleanup of three seconds, handed over before the hold as a step's guard would be.
        progress: Some(Arc::new(move |handle: u64, progress: Progress| {
            if progress == Progress::Started(crate::harness::Step::Python) {
                drop(CleanupGuard::arm(
                    Cmd::new("/bin/sleep").arg("3"),
                    Reissue::Once,
                ));
            }
            hold(handle, progress);
        })),
        ..bed.sessions()
    };
    let program = bed
        .start("gives_up.py", bed.config("", true), sessions, |p| {
            p.grace(std::time::Duration::from_secs(1))
        })
        .await;
    bed.raised("reached").await;
    let signalled = std::time::Instant::now();
    program.interrupt();
    // Long enough for the program's shutdown to be queued before the start goes on.
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    bed.raise("go-on");
    let ran = bed.finish(program).await;
    assert_eq!(
        ran.exit.code(),
        0,
        "stopped while its entry was cleaned up: {}",
        ran.stderr
    );
    assert!(
        signalled.elapsed() >= std::time::Duration::from_secs(2),
        "the cleanup was not waited for: {:?}",
        signalled.elapsed()
    );
    assert_eq!(
        ran.json()[0],
        serde_json::json!({"gave_up": null, "report": null})
    );
    assert!(
        ran.exit.left_open().is_empty(),
        "{:?}",
        left_open(&ran.exit)
    );
}

/// A start that fails on its own has its cleanup awaited before the program is told, and a
/// program that gives up on the entry meanwhile -- at a `SIGINT`, while a cleanup that runs past
/// its whole grace is awaited -- has its grace held for that wait, as for any shutdown it asked
/// for.
#[tokio::test]
async fn a_failed_start_holds_the_grace_of_a_program_that_gives_up_on_it() {
    use crate::process::Cmd;
    use crate::supervise::{CleanupGuard, Reissue};

    let bed = Bed::new();
    bed.write(
        "fails.py",
        r#"
        import asyncio, json, outrig, signal

        async def main():
            session = outrig.Session()
            entering = asyncio.create_task(session.__aenter__())
            asyncio.get_running_loop().add_signal_handler(signal.SIGINT, entering.cancel)
            try:
                await entering
                print(json.dumps({"entered": session.id}))
            except asyncio.CancelledError:
                print(json.dumps({"gave_up": session.id}))

        asyncio.run(main())
        "#,
    );
    let settling = bed.flag("settling");
    let sessions = Sessions {
        // A cleanup of five seconds, handed over as the bindings start, as a step's guard is.
        progress: Some(Arc::new(|_: u64, progress: Progress| {
            if progress == Progress::Started(crate::harness::Step::Bindings) {
                drop(CleanupGuard::arm(
                    Cmd::new("/bin/sleep").arg("5"),
                    Reissue::Once,
                ));
            }
        })),
        settling: Some(Arc::new(move |_| {
            std::fs::write(&settling, b"").expect("raise the flag");
        })),
        ..bed.sessions()
    };
    let program = bed
        .start("fails.py", bed.config(FAILING, false), sessions, |p| {
            p.grace(std::time::Duration::from_secs(1))
        })
        .await;
    bed.raised("settling").await;
    let signalled = std::time::Instant::now();
    program.interrupt();
    let ran = bed.finish(program).await;
    assert_eq!(
        ran.exit.code(),
        0,
        "stopped while its failed start was cleaned up: {}",
        ran.stderr
    );
    assert!(
        signalled.elapsed() >= std::time::Duration::from_secs(2),
        "the cleanup was not waited for: {:?}",
        signalled.elapsed()
    );
    assert_eq!(ran.json()[0], serde_json::json!({"gave_up": null}));
    assert!(
        ran.exit.left_open().is_empty(),
        "{:?}",
        left_open(&ran.exit)
    );
}

/// R6's shape: a program that leaves its session when interrupted, and prints the report itself,
/// exits 0 after a forwarded `SIGINT`, leaving nothing for the engine.
#[tokio::test]
async fn a_program_that_leaves_its_session_on_sigint_exits_0() {
    let bed = Bed::new();
    bed.write(
        "r6.py",
        r#"
        import asyncio, json, outrig, signal, wait

        async def main():
            leave = asyncio.Event()
            asyncio.get_running_loop().add_signal_handler(signal.SIGINT, leave.set)
            async with outrig.Session() as session:
                wait.raise_flag("entered")
                await leave.wait()
            print(json.dumps({"verdict": session.shutdown_report.verdict}))

        asyncio.run(main())
        "#,
    );
    let program = bed
        .start("r6.py", bed.config("", false), bed.sessions(), |p| p)
        .await;
    bed.raised("entered").await;
    program.interrupt();
    let ran = bed.finish(program).await;
    assert_eq!(ran.exit.code(), 0, "{}", ran.stderr);
    assert_eq!(ran.json()[0], serde_json::json!({"verdict": "clean"}));
    assert!(ran.exit.left_open().is_empty());
}

/// The grace is the program's, not the engine's: while a shutdown the program asked for runs,
/// it does not count. Here the shutdown drains a call that takes longer than the whole grace, and
/// the program, which left its session at the signal, still exits 0 with the call `returned`.
#[tokio::test]
async fn the_grace_waits_for_a_shutdown_the_program_asked_for() {
    let bed = Bed::new();
    bed.write(
        "drains.py",
        r#"
        import asyncio, json, outrig, signal, wait

        async def main():
            leave = asyncio.Event()
            asyncio.get_running_loop().add_signal_handler(signal.SIGINT, leave.set)
            async with outrig.Session(grace=30) as session:
                wait.raise_flag("entered")
                await leave.wait()
            report = session.shutdown_report
            print(json.dumps({"verdict": report.verdict,
                              "hosted": [c.outcome for c in report.hosted]}))

        asyncio.run(main())
        "#,
    );
    let program = bed
        .start("drains.py", bed.config("", true), bed.sessions(), |p| {
            p.grace(std::time::Duration::from_secs(1))
        })
        .await;
    bed.raised("entered").await;
    let interpreter = interpreter(&program, 1);
    crate::python::testing::within(interpreter.open("a"))
        .await
        .unwrap_or_else(|e| panic!("{e}"));
    let started = bed.flag("started");
    interpreter.send_line(serde_json::json!({
        "t": "exec", "agent": "a", "id": 1,
        "src": format!("fx.record(seconds=3, started={started:?})"),
    }));
    bed.raised("started").await;
    let signalled = std::time::Instant::now();
    program.interrupt();
    let ran = bed.finish(program).await;
    assert_eq!(
        ran.exit.code(),
        0,
        "killed during its own shutdown: {}",
        ran.stderr
    );
    assert!(signalled.elapsed() >= std::time::Duration::from_secs(2));
    assert_eq!(
        ran.json()[0],
        serde_json::json!({"verdict": "clean", "hosted": ["returned"]})
    );
}

/// A program that ignores the forwarded signal, holding a session, is stopped once the grace
/// passes: `SIGTERM`, which it ignores too, then `SIGKILL`. The engine then shuts its session
/// down, as for any program that ends with one open.
#[tokio::test]
async fn a_program_ignoring_signals_is_killed_after_the_grace_and_its_session_stopped() {
    let bed = Bed::new();
    bed.write(
        "stubborn.py",
        r#"
        import asyncio, outrig, signal, wait

        signal.signal(signal.SIGINT, signal.SIG_IGN)
        signal.signal(signal.SIGTERM, signal.SIG_IGN)

        async def main():
            async with outrig.Session():
                wait.raise_flag("entered")
                await asyncio.sleep(120)

        asyncio.run(main())
        "#,
    );
    let program = bed
        .start("stubborn.py", bed.config("", false), bed.sessions(), |p| {
            p.grace(std::time::Duration::from_secs(1))
        })
        .await;
    bed.raised("entered").await;
    let signalled = std::time::Instant::now();
    program.interrupt();
    let ran = bed.finish(program).await;
    let waited = signalled.elapsed();
    assert_eq!(ran.exit.code(), 137, "{}", ran.stderr);
    assert!(
        waited >= std::time::Duration::from_secs(6),
        "the grace, then five seconds after SIGTERM: {waited:?}"
    );
    let left = left_open(&ran.exit);
    assert_eq!(left.len(), 1, "{left:?}");
    assert_eq!(left[0].1, "shut down: Clean", "{left:?}");
}

/// A second signal ends the grace at once: a program that ignores `SIGINT` but not `SIGTERM` is
/// stopped at the second, well inside a minute's grace.
#[tokio::test]
async fn a_second_signal_ends_the_grace_at_once() {
    let bed = Bed::new();
    bed.write(
        "ignores.py",
        r#"
        import signal, time, wait

        signal.signal(signal.SIGINT, signal.SIG_IGN)
        wait.raise_flag("ready")
        time.sleep(120)
        "#,
    );
    let program = bed
        .start(
            "ignores.py",
            bed.config("", false),
            Sessions::default(),
            |p| p.grace(std::time::Duration::from_secs(60)),
        )
        .await;
    bed.raised("ready").await;
    program.interrupt();
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let signalled = std::time::Instant::now();
    program.interrupt();
    let ran = bed.finish(program).await;
    assert_eq!(ran.exit.code(), 143, "{}", ran.stderr);
    assert!(
        signalled.elapsed() < std::time::Duration::from_secs(5),
        "{:?}",
        signalled.elapsed()
    );
}

/// `outrig.Session` checks its inputs when it is made, which asks the engine for nothing: paths
/// are strings or path-likes, made absolute against the working directory with `..` kept; a
/// container path is absolute; access is one of two; a grace is a number of seconds from 0 to an
/// hour, the most the engine takes. A session is entered once, a failed entry counting. An entry
/// whose request is longer than the engine reads fails with `ValueError` before it is sent,
/// rather than wait for an answer that would never come, and ends the session's events, so a
/// reader already waiting on them is let go.
#[tokio::test]
async fn a_sessions_inputs_are_checked_when_it_is_made() {
    let bed = Bed::new();
    bed.write(
        "inputs.py",
        r#"
        import asyncio, json, outrig

        def refused(make):
            try:
                make()
                return None
            except (TypeError, ValueError) as e:
                return type(e).__name__

        async def drain(records):
            return [record async for record in records]

        async def main():
            out = {name: refused(make) for name, make in {
                "workspace": lambda: outrig.Session(workspace=3),
                "bytes": lambda: outrig.Session(workspace=b"/x"),
                "one_path": lambda: outrig.Session(mounts="/x"),
                "relative_container": lambda: outrig.Mount("x", "rel"),
                "access": lambda: outrig.Mount("x", access="rw"),
                "grace_bool": lambda: outrig.Session(grace=True),
                "grace_negative": lambda: outrig.Session(grace=-1),
                "grace_nan": lambda: outrig.Session(grace=float("nan")),
                "grace_an_hour": lambda: outrig.Session(grace=3600),
                "grace_past_an_hour": lambda: outrig.Session(grace=3600.5),
            }.items()}
            mount = outrig.Mount("../up/m", "/m", "read-write")
            out["mount"] = [str(mount.host_path), str(mount.container_path), mount.access]
            entered = outrig.Session()
            async with entered:
                pass
            failed = outrig.Session(workspace="missing")
            try:
                async with failed:
                    pass
            except RuntimeError:
                out["failed"] = failed.id
            for name, session in (("again", entered), ("failed_again", failed)):
                try:
                    async with session:
                        pass
                except RuntimeError as e:
                    out[name] = str(e)
            too_long = outrig.Session(mounts=[f"/{'m' * 200}/{n}" for n in range(400)])
            reading = asyncio.create_task(drain(too_long.events()))
            await asyncio.sleep(0)
            try:
                async with too_long:
                    pass
            except ValueError as e:
                out["too_long"] = str(e).split(" request of ")[0]
            out["too_long_events"] = await asyncio.wait_for(reading, 10)
            print(json.dumps(out))

        asyncio.run(main())
        "#,
    );
    let program = bed
        .start("inputs.py", bed.config("", false), bed.sessions(), |p| p)
        .await;
    let ran = bed.finish(program).await;
    assert_eq!(ran.exit.code(), 0, "{}", ran.stderr);
    let once = "an outrig.Session is entered once; make another to enter again";
    assert_eq!(
        ran.json()[0],
        serde_json::json!({
            "workspace": "TypeError",
            "bytes": "TypeError",
            "one_path": "TypeError",
            "relative_container": "ValueError",
            "access": "ValueError",
            "grace_bool": "TypeError",
            "grace_negative": "ValueError",
            "grace_nan": "ValueError",
            "grace_an_hour": null,
            "grace_past_an_hour": "ValueError",
            "mount": [format!("{}/../up/m", bed.dir.display()), "/m", "read-write"],
            "failed": null,
            "again": once,
            "failed_again": once,
            "too_long": "a session.start",
            "too_long_events": [],
        })
    );
}

/// Sessions a program enters in real containers, with a live podman.
#[cfg(feature = "e2e")]
mod e2e {
    use super::*;
    use crate::python::testing::{ALPINE, pull};

    /// The bed's configuration, its sessions run in alpine: `top` among the top-level keys,
    /// `tables` after the image's.
    fn in_alpine(bed: &Bed, top: &str, tables: &str, fixture: bool) -> crate::config::Config {
        bed.config(
            &format!(
                "default-image = \"alpine\"\n{top}\n[images.alpine]\nimage-name = {ALPINE:?}\n\
                 {tables}\n"
            ),
            fixture,
        )
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

    /// The id of the one session directory under the bed's session root.
    fn only_session(bed: &Bed) -> String {
        let dirs = bed.session_dirs();
        assert_eq!(dirs.len(), 1, "{dirs:?}");
        dirs[0].file_name().unwrap().to_string_lossy().into_owned()
    }

    /// A session in a container stops proven, and leaves no container behind once the program's
    /// exit says so -- nothing to wait for afterward.
    #[tokio::test]
    async fn a_session_in_a_container_stops_and_leaves_nothing() {
        pull(ALPINE).await;
        let bed = Bed::new();
        bed.write(
            "plain.py",
            r#"
            import asyncio, json, outrig

            async def main():
                async with outrig.Session() as session:
                    pass
                print(json.dumps([session.id, session.shutdown_report.verdict]))

            asyncio.run(main())
            "#,
        );
        let program = bed
            .start(
                "plain.py",
                in_alpine(&bed, "", "", false),
                bed.in_containers(),
                |p| p,
            )
            .await;
        let ran = bed.finish(program).await;
        assert_eq!(ran.exit.code(), 0, "{}", ran.stderr);
        let line = &ran.json()[0];
        assert_eq!(line[1], "clean");
        assert_eq!(containers_of(line[0].as_str().unwrap()), "");
    }

    /// An image that cannot be had fails the entry: the program's `RuntimeError` names it, the
    /// body and `__aexit__` never run, and no container is left for the session.
    #[tokio::test]
    async fn a_bad_image_fails_entry_and_leaves_no_container() {
        let bed = Bed::new();
        bed.write(
            "bad.py",
            r#"
            import asyncio, json, outrig

            class Watched(outrig.Session):
                async def __aexit__(self, *exc):
                    print(json.dumps({"aexit": True}), flush=True)
                    return await super().__aexit__(*exc)

            async def main():
                try:
                    async with Watched():
                        print(json.dumps({"body": True}), flush=True)
                except RuntimeError as e:
                    print(json.dumps({"refused": str(e)}))

            asyncio.run(main())
            "#,
        );
        let config = bed.config(
            "default-image = \"absent\"\n\n[images.absent]\n\
             image-name = \"localhost/outrig-test-absent:none\"\n",
            false,
        );
        let program = bed
            .start("bad.py", config, bed.in_containers(), |p| p)
            .await;
        let ran = bed.finish(program).await;
        assert_eq!(ran.exit.code(), 0, "{}", ran.stderr);
        let lines = ran.json();
        assert_eq!(lines.len(), 1, "only the refusal: {}", ran.stdout);
        let refused = lines[0]["refused"].as_str().expect("refused");
        assert!(refused.contains("outrig-test-absent"), "{refused}");
        assert_eq!(containers_of(&only_session(&bed)), "");
        assert!(ran.exit.left_open().is_empty());
    }

    /// A program killed while its session's container starts -- as podman creates it, and once
    /// it runs and the interpreter is starting in it -- has the start interrupted, and no
    /// container is left for the session the moment the program's exit is reported.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_program_killed_while_its_container_starts_leaves_no_container() {
        pull(ALPINE).await;
        for at in [
            crate::harness::Step::Container,
            crate::harness::Step::Python,
        ] {
            let bed = Bed::new();
            bed.write(
                "dies.py",
                r#"
                import asyncio, json, os, outrig

                async def main():
                    print(json.dumps({"pid": os.getpid()}), flush=True)
                    async with outrig.Session():
                        print("entered", flush=True)

                asyncio.run(main())
                "#,
            );
            let sessions = Sessions {
                progress: Some(holding_at(&bed, at)),
                ..bed.in_containers()
            };
            let program = bed
                .start("dies.py", in_alpine(&bed, "", "", false), sessions, |p| p)
                .await;
            bed.raised("reached").await;
            let pid = printed_pid(&bed);
            nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGKILL)
                .expect("kill the program");
            crate::python::testing::eventually(
                || (!std::path::Path::new(&format!("/proc/{pid}")).exists()).then_some(()),
                || format!("the reap of {pid}"),
            );
            bed.raise("go-on");
            let ran = bed.finish(program).await;
            assert_eq!(ran.exit.code(), 137, "{at:?}: {}", ran.stderr);
            let left = left_open(&ran.exit);
            assert_eq!(left.len(), 1, "{at:?}: {left:?}");
            assert_eq!(left[0].1, "not started: Proven", "{at:?}: {left:?}");
            assert_eq!(containers_of(&left[0].0), "", "{at:?}");
        }
    }

    /// The program's code, run in kernel `agent` of the session it numbered `handle`, writing
    /// what `expression` evaluates to into `name` in the bed, which the session mounts at its host
    /// path; returns it.
    async fn evaluated(
        bed: &Bed,
        program: &crate::harness::RunningProgram,
        handle: u64,
        agent: &str,
        expression: &str,
        name: &str,
    ) -> String {
        let out = bed.flag(&format!("{name}.out"));
        run_in(
            bed,
            &interpreter(program, handle),
            agent,
            &format!(
                "import os\nwith open({out:?}, 'w') as f:\n    f.write(str({expression}))",
                out = out.to_str().unwrap()
            ),
            name,
        )
        .await;
        std::fs::read_to_string(out).expect("what the kernel wrote")
    }

    /// The workspace a session mounts: the configured `[workspace] host-path` when the program
    /// names none; none at all when neither does, the kernel's working directory then the
    /// image's; and the program's own, mounted at its host path, beside a factory binding.
    #[tokio::test]
    async fn the_workspace_is_the_programs_the_configured_one_or_none() {
        pull(ALPINE).await;
        let bed = Bed::new();
        let workspace = bed.dir.join("ws");
        std::fs::create_dir(&workspace).unwrap();
        std::fs::write(workspace.join("marker"), "here").unwrap();
        bed.write(
            "workspaces.py",
            r#"
            import asyncio, os, outrig, sys, wait

            async def main():
                scratch = outrig.Mount(os.getcwd(), access="read-write")
                workspace = sys.argv[1] if len(sys.argv) > 1 else None
                async with outrig.Session(workspace=workspace, mounts=[scratch]):
                    wait.raise_flag("entered")
                    await wait.flag("looked")

            asyncio.run(main())
            "#,
        );
        let configured = format!(
            "[workspace]\nhost-path = {:?}\n",
            workspace.to_str().unwrap()
        );
        let cases: [(&str, crate::config::Config, Vec<String>); 3] = [
            (
                "configured",
                in_alpine(&bed, "", &configured, false),
                vec![],
            ),
            ("none", in_alpine(&bed, "", "", false), vec![]),
            (
                "the program's, beside a binding",
                in_alpine(&bed, "", "", true),
                vec![workspace.to_str().unwrap().to_string()],
            ),
        ];
        for (case, config, args) in cases {
            let _ = std::fs::remove_file(bed.flag("entered"));
            let _ = std::fs::remove_file(bed.flag("looked"));
            let program = bed
                .start("workspaces.py", config, bed.in_containers(), |p| {
                    p.args(args)
                })
                .await;
            bed.raised("entered").await;
            let cwd = evaluated(
                &bed,
                &program,
                1,
                "a",
                "os.getcwd()",
                &format!("{case}-cwd"),
            )
            .await;
            let found = evaluated(
                &bed,
                &program,
                1,
                "b",
                "[p for p in ('/workspace/marker', os.path.join(os.getcwd(), 'marker')) \
                 if os.path.exists(p)]",
                &format!("{case}-marker"),
            )
            .await;
            bed.raise("looked");
            let ran = bed.finish(program).await;
            assert_eq!(ran.exit.code(), 0, "{case}: {}", ran.stderr);
            match case {
                "configured" => {
                    assert_eq!(cwd, "/workspace", "{case}");
                    assert_eq!(
                        found, "['/workspace/marker', '/workspace/marker']",
                        "{case}"
                    );
                }
                "none" => {
                    assert_eq!(cwd, "/", "{case}: the image's own working directory");
                    assert_eq!(found, "[]", "{case}");
                }
                _ => {
                    assert_eq!(cwd, workspace.to_str().unwrap(), "{case}");
                    assert!(
                        found.contains(workspace.join("marker").to_str().unwrap()),
                        "{found}"
                    );
                }
            }
        }
    }

    /// Two sessions entered at once each run in a container of their own, each kernel the test
    /// opens runs in its own, and both stop proven, leaving no container: the engine's
    /// process-wide state -- the reaper, the supervisor's thread, the container cleanup -- serves
    /// both.
    #[tokio::test]
    async fn two_sessions_run_in_containers_at_once() {
        pull(ALPINE).await;
        let bed = Bed::new();
        bed.write(
            "two.py",
            r#"
            import asyncio, json, os, outrig, wait

            async def held(name):
                scratch = outrig.Mount(os.getcwd(), access="read-write")
                async with outrig.Session(mounts=[scratch]) as session:
                    wait.raise_flag(f"{name}-entered")
                    await wait.flag("ran")
                return [session.id, session.shutdown_report.verdict]

            async def main():
                print(json.dumps(await asyncio.gather(held("one"), held("two"))))

            asyncio.run(main())
            "#,
        );
        let program = bed
            .start(
                "two.py",
                in_alpine(&bed, "", "", false),
                bed.in_containers(),
                |p| p,
            )
            .await;
        bed.raised("one-entered").await;
        bed.raised("two-entered").await;
        let first = evaluated(&bed, &program, 1, "a", "os.uname().nodename", "first").await;
        let second = evaluated(&bed, &program, 2, "b", "os.uname().nodename", "second").await;
        assert_ne!(first, second, "one container each");
        bed.raise("ran");
        let ran = bed.finish(program).await;
        assert_eq!(ran.exit.code(), 0, "{}", ran.stderr);
        for session in ran.json()[0].as_array().unwrap() {
            assert_eq!(session[1], "clean");
            assert_eq!(containers_of(session[0].as_str().unwrap()), "");
        }
    }
}
