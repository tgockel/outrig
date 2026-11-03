//! `outrig run-new <script.py>` with sessions, end to end: the binary, a program entering sessions
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

    /// `outrig --session-root <case> run-new <args>`, in the repository, in a process group of its
    /// own, its stdout and stderr piped.
    fn spawn(&self, args: &[&str]) -> Child {
        // An empty global config, which exists, as `--global-config` must.
        self.spawn_with(Path::new("/dev/null"), args)
    }

    /// [`Case::spawn`], with `global` as the global configuration.
    fn spawn_with(&self, global: &Path, args: &[&str]) -> Child {
        Command::new(env!("CARGO_BIN_EXE_outrig"))
            .arg("--global-config")
            .arg(global)
            .arg("--session-root")
            .arg(self.sessions())
            .arg("run-new")
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

/// How long a program whose agent runs on a real model has to finish: a few of its rounds.
const REAL_MODEL_TIMEOUT: Duration = Duration::from_secs(600);

/// The global configuration a real-model test runs on, `OUTRIG_E2E_GLOBAL_CONFIG`, when
/// `OUTRIG_E2E_REAL_API=1` opts into them; otherwise `None`, saying that `test` is skipped.
fn real_model(test: &str) -> Option<PathBuf> {
    if std::env::var("OUTRIG_E2E_REAL_API").ok().as_deref() != Some("1") {
        eprintln!(
            "[outrig-e2e] skipping {test} (set OUTRIG_E2E_REAL_API=1 and \
             OUTRIG_E2E_GLOBAL_CONFIG to enable)"
        );
        return None;
    }
    let global = std::env::var("OUTRIG_E2E_GLOBAL_CONFIG").expect(
        "OUTRIG_E2E_REAL_API=1 set without OUTRIG_E2E_GLOBAL_CONFIG -- name the global \
         configuration that declares the model",
    );
    Some(global.into())
}

/// The requirements' program, given the directory of a project to read as its argument: a
/// `ProjectReader` in a session where that directory is mounted read-only, at its own path,
/// answers what the project does, and the program prints the answer on one line and then the
/// session's verdict.
const READER: &str = r#"
import asyncio, sys
import outrig

class ProjectReader(outrig.Agent):
    """You read the project under the directory `root` names, and answer questions about it from
    what its files say. Start with its README and its manifests."""

    @outrig.agent
    async def explain(self, question: str) -> str:
        """Answer `question` about the project in a few sentences of plain text."""
        ...

async def main():
    root = sys.argv[1]
    async with outrig.Session(mounts=[outrig.Mount(root)]) as session:
        async with ProjectReader(root=root) as agent:
            answer = await agent.explain("What does this project do?")
    print(" ".join(answer.split()), flush=True)
    print(session.shutdown_report.verdict, flush=True)

asyncio.run(main())
"#;

/// The requirements' program against a real model, opted into with `OUTRIG_E2E_REAL_API=1` and
/// `OUTRIG_E2E_GLOBAL_CONFIG` naming a global configuration whose providers and `default-model`
/// it runs on, their keys in the environment: asked about this repository, mounted read-only so
/// the model cannot change it, its agent answers, and the program exits 0 with a clean report.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_requirements_reader_answers_about_this_repository() {
    let Some(global) = real_model("the_requirements_reader_answers_about_this_repository") else {
        return;
    };
    pull();
    let case = Case::new("");
    case.write("reader.py", READER);
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("this repository");
    let child = case.spawn_with(
        &global,
        &["reader.py", repository.to_str().expect("a UTF-8 path")],
    );
    let out = timeout(REAL_MODEL_TIMEOUT, child.wait_with_output())
        .await
        .expect("the program ended in time")
        .expect("wait");
    let (stdout, stderr) = (
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    assert!(out.status.success(), "{}\n{stdout}\n{stderr}", out.status);
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(
        lines.len(),
        2,
        "an answer, then the verdict: {stdout}\n{stderr}"
    );
    assert!(!lines[0].trim().is_empty(), "an answer: {stdout}");
    assert_eq!(lines[1], "clean", "{stdout}\n{stderr}");
}

/// The host-program exit criterion's program: a `Task` the program holds, injected as `task`,
/// whose numbers an `Adder` fetches and adds, reporting its progress there, and answers with a
/// `Total` the program declares. It prints the session's id, then the answer and the progress,
/// then waits for `SIGTERM`, which cancels it, and prints the verdict of the report it read.
const TASKED: &str = r#"
import asyncio, dataclasses, json, signal
import outrig

class Task:
    """The work this program hands its agent: its inputs, and where progress is reported."""

    def __init__(self):
        self.progress = []

    def report_progress(self, summary: str) -> None:
        """Record `summary`, a line saying what has been done so far."""
        self.progress.append(summary)

    def fetch_input(self, name: str) -> str:
        """The input `name`, as text: `numbers` is the numbers to add, comma-separated."""
        if name != "numbers":
            raise KeyError(f"no input {name!r}: the one input is 'numbers'")
        return "17, 25, 58"

@dataclasses.dataclass
class Total:
    numbers: list[int]
    total: int

class Adder(outrig.Agent):
    """You add numbers for the program that made you. Its `task` object, a binding in your
    Python, holds the work: `task.fetch_input("numbers")` returns the numbers, and
    `task.report_progress(summary)` records a line of progress -- call it once you have read
    them."""

    @outrig.agent
    async def add(self, request: str) -> Total:
        """Do `request`, and return the numbers you added and their sum."""
        ...

async def main():
    asyncio.get_running_loop().add_signal_handler(signal.SIGTERM, asyncio.current_task().cancel)
    task, session = Task(), outrig.Session()
    try:
        async with session:
            session.inject("task", task)
            print(json.dumps({"session": session.id}), flush=True)
            async with Adder() as agent:
                total = await agent.add("Add the task's numbers, reporting your progress.")
            print(json.dumps({"total": total.total, "numbers": total.numbers,
                              "progress": task.progress}), flush=True)
            await asyncio.sleep(600)
    except asyncio.CancelledError:
        pass
    print(json.dumps({"verdict": session.shutdown_report.verdict}), flush=True)

asyncio.run(main())
"#;

/// The host-program exit criterion against a real model, opted into as the reader's test is:
/// the `task` the program injects is used by its agent as plain code -- `fetch_input` and
/// `report_progress` each called, evented under the binding -- the agent answers with the
/// program's own `Total`, and `SIGTERM` then ends the program, which reads its session's report,
/// with no container left.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_tasked_agent_uses_the_object_its_program_injected() {
    let Some(global) = real_model("a_tasked_agent_uses_the_object_its_program_injected") else {
        return;
    };
    pull();
    let case = Case::new("[events]\nmode = \"record\"\n");
    case.write("tasked.py", TASKED);
    let mut child = case.spawn_with(&global, &["tasked.py"]);
    let mut lines = BufReader::new(child.stdout.take().expect("piped")).lines();
    let mut read = Vec::new();
    timeout(REAL_MODEL_TIMEOUT, async {
        while read.len() < 2 {
            match lines.next_line().await.expect("read stdout") {
                Some(line) => read.push(line),
                None => break,
            }
        }
    })
    .await
    .expect("the answer in time");
    if read.len() < 2 {
        let (status, stderr, _) = finish(child).await;
        panic!("{status}: {read:?}\n{stderr}");
    }
    kill(Pid::from_raw(child.id().unwrap() as i32), Signal::SIGTERM).unwrap();
    while let Some(line) = timeout(TEST_TIMEOUT, lines.next_line())
        .await
        .expect("the program ended in time")
        .expect("read stdout")
    {
        read.push(line);
    }
    let (status, stderr, _) = finish(child).await;
    assert_eq!(status.code(), Some(0), "{read:?}\n{stderr}");
    let json: Vec<serde_json::Value> = read
        .iter()
        .map(|line| serde_json::from_str(line).unwrap_or_else(|e| panic!("{e}: {line}")))
        .collect();
    let id = json[0]["session"].as_str().expect("the session's id");
    assert_eq!(json[1]["total"], 100, "{read:?}");
    assert_eq!(
        json[1]["numbers"],
        serde_json::json!([17, 25, 58]),
        "{read:?}"
    );
    assert!(
        json[1]["progress"]
            .as_array()
            .is_some_and(|lines| !lines.is_empty()),
        "progress reported: {read:?}"
    );
    assert_eq!(json[2], serde_json::json!({"verdict": "clean"}), "{read:?}");
    assert_eq!(containers_of(id), "");
    let log = case.sessions().join(id).join("logs/events.jsonl");
    let members: std::collections::BTreeSet<String> = std::fs::read_to_string(log)
        .expect("the log")
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("a record"))
        .filter(|record| record["type"] == "org.outrig.hosted.received")
        .filter(|record| record["data"]["binding"] == "task")
        .filter_map(|record| record["data"]["member"].as_str().map(String::from))
        .collect();
    assert!(
        members.contains("fetch_input") && members.contains("report_progress"),
        "{members:?}"
    );
}
