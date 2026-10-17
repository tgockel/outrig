//! What the crate's tests need to drive the real interpreter.
//!
//! `Interpreter::start` needs a container with the payload mounted. Running
//! the same program with the same arguments on the host needs neither podman
//! nor an image, so the host half and the agent loop are both tested against
//! the real interpreter this way.

use std::future::Future;
use std::io::Read;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use nix::sys::resource::{Resource, getrlimit, setrlimit};

use serde_json::{Value, json};
use tokio::io::{
    AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream, Lines, ReadHalf, WriteHalf,
};
use tokio::process::Child;

use super::host::{ARGS, ExecId, Interpreter, InterpreterError, Outcome, PRIMARY, Report};
use super::payload;
use super::relay::FIXTURE_SOURCE;
use crate::events::Events;

/// How long any one step may take before a test fails rather than hangs.
const TIMEOUT: Duration = Duration::from_secs(20);

/// `step`, or a panic once it has taken longer than [`TIMEOUT`].
/// How the hosted request `request` ended, from its `hosted.ended` among `events`.
pub(crate) fn ended(
    events: &[Arc<crate::harness::event::Event>],
    request: crate::harness::event::RequestId,
) -> (
    crate::harness::event::HostedOutcome,
    Option<crate::harness::event::HostedReason>,
) {
    events
        .iter()
        .find_map(|event| match &event.payload {
            crate::harness::event::Payload::HostedEnded {
                request: r,
                outcome,
                reason,
                ..
            } if *r == request => Some((*outcome, *reason)),
            _ => None,
        })
        .unwrap_or_else(|| panic!("{request} has not ended"))
}

pub(crate) async fn within<F: Future>(step: F) -> F::Output {
    tokio::time::timeout(TIMEOUT, step)
        .await
        .unwrap_or_else(|_| panic!("a step took longer than {TIMEOUT:?}"))
}

/// Wait for the slot an execution nobody was waiting for held to come back,
/// polling, since nothing on the host announces it.
pub(crate) async fn slot_freed(interpreter: &Interpreter) {
    within(async {
        while interpreter.abandoned().is_some() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
}

/// Whether no process is left in the process group `pgid`: `kill(-pgid, 0)` failing with
/// `ESRCH`, and nothing else, since a zombie member still answers.
pub(crate) fn group_gone(pgid: nix::unistd::Pid) -> bool {
    nix::sys::signal::kill(nix::unistd::Pid::from_raw(-pgid.as_raw()), None)
        == Err(nix::errno::Errno::ESRCH)
}

/// Wait up to `within` for the process group `pgid` to be gone, failing with what is left in it.
pub(crate) fn wait_group_gone(pgid: nix::unistd::Pid, within: Duration) {
    let deadline = Instant::now() + within;
    while !group_gone(pgid) {
        assert!(
            Instant::now() < deadline,
            "group {pgid} still holds {:?} after {within:?}",
            super::supervisor::group_members(pgid)
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Polls `check` until it answers, failing after [`TIMEOUT`] with what it was waiting for.
pub(crate) fn eventually<T>(
    mut check: impl FnMut() -> Option<T>,
    waited_for: impl FnOnce() -> String,
) -> T {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        if let Some(answer) = check() {
            return answer;
        }
        assert!(
            Instant::now() < deadline,
            "no {} within {TIMEOUT:?}",
            waited_for()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// A path nothing exists at until someone says so: the test, or a program under test that
/// creates it to report where it has got to.
pub(crate) struct Flag {
    _dir: tempfile::TempDir,
    path: PathBuf,
}

impl Flag {
    pub(crate) fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("flag");
        Self { _dir: dir, path }
    }

    /// The path as a Python string literal.
    pub(crate) fn py(&self) -> String {
        format!("{:?}", self.path.to_str().expect("a UTF-8 temp path"))
    }

    pub(crate) fn raise(&self) {
        std::fs::write(&self.path, b"").expect("raise the flag");
    }

    pub(crate) fn exists(&self) -> bool {
        self.path.exists()
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// Block until the flag exists, failing after [`TIMEOUT`].
    pub(crate) fn wait(&self) {
        eventually(
            || self.exists().then_some(()),
            || format!("flag {}", self.path.display()),
        );
    }

    /// Python that awaits the flag, failing rather than hanging if it never appears.
    pub(crate) fn awaited(&self) -> String {
        self.waited_with("await asyncio.sleep(0.01)")
    }

    /// The same wait, holding the kernel's loop the whole time.
    pub(crate) fn blocked(&self) -> String {
        self.waited_with("time.sleep(0.01)")
    }

    fn waited_with(&self, pause: &str) -> String {
        py(&format!(
            r#"
            import os, time
            deadline = time.monotonic() + {timeout}
            while not os.path.exists({path}):
                if time.monotonic() > deadline:
                    raise TimeoutError('the flag never appeared')
                {pause}
            "#,
            path = self.py(),
            timeout = TIMEOUT.as_secs(),
        ))
    }
}

/// A clean run that printed `output` and nothing else.
pub(crate) fn ok(output: &str) -> Outcome {
    Outcome::Ok(Report {
        output: output.to_string(),
        ..Report::default()
    })
}

/// The `HOME` an interpreter run on the host gets unless its test gives one of
/// its own: one directory per user under the system's temporary directory.
/// The interpreter makes the environment pip installs into under `HOME` as it
/// starts, and that must never land in the real home of whoever runs the tests.
/// Nor may the user site the tests find to show it is not read, so the
/// harnesses also clear `PYTHONUSERBASE`, which `site` honors even under `-I`.
pub(crate) fn host_home() -> PathBuf {
    std::env::temp_dir().join(format!("outrig-test-home-{}", nix::unistd::getuid()))
}

/// The payload's interpreter, started as `Interpreter::start` starts it but on
/// the host, with `agent` as its argument when there is one.
pub(super) async fn spawn(agent: Option<&str>) -> Child {
    spawn_with(agent, None).await
}

/// [`spawn`], with `rpyc` -- the vendored RPyC's directory -- after the agent,
/// as a session with bindings starts it.
async fn spawn_with(agent: Option<&str>, rpyc: Option<&Path>) -> Child {
    let python = payload::host_python()
        .await
        .expect("the payload this build embedded");
    tokio::process::Command::new(python)
        .args(ARGS)
        .args(agent)
        .args(rpyc)
        .env("HOME", host_home())
        .env_remove("PYTHONUSERBASE")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("the interpreter starts")
}

/// A started interpreter addressing the primary agent.
pub(crate) async fn start_on_host() -> Interpreter {
    start_on_host_with(Events::off()).await
}

/// [`start_on_host`], recording to `events`.
pub(crate) async fn start_on_host_with(events: Events) -> Interpreter {
    start_on_host_rpyc(events, None).await
}

/// [`start_on_host_with`], importing RPyC from `rpyc` when given: an interpreter
/// that can reach a session's bindings.
pub(crate) async fn start_on_host_rpyc(events: Events, rpyc: Option<&Path>) -> Interpreter {
    within(Interpreter::from_child(
        spawn_with(Some(PRIMARY), rpyc).await,
        events,
    ))
    .await
    .unwrap_or_else(|e| panic!("{e}"))
}

/// How a test starts the interpreter on the host. By default: in the test's
/// working directory, with [`host_home`] as `HOME`, under the ceiling it sets
/// itself.
#[derive(Default)]
pub(crate) struct Start<'a> {
    /// A soft `RLIMIT_DATA` already in place when it starts.
    pub(crate) ceiling: Option<u64>,
    /// Its working directory, which it takes for the workspace.
    pub(crate) dir: Option<&'a Path>,
    /// Its `HOME`, under which pip installs.
    pub(crate) home: Option<&'a Path>,
}

/// The payload's `python3` as the host-side harnesses run it: `HOME` set for
/// tests and `PYTHONUSERBASE` cleared (see [`host_home`]), every stream piped,
/// and a process group of its own, so a harness's `Drop` reaches the children
/// it starts.
pub(crate) fn python_command(home: Option<&Path>) -> Command {
    let mut command = Command::new(python());
    command
        .env("HOME", home.map_or_else(host_home, Path::to_path_buf))
        .env_remove("PYTHONUSERBASE")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    command
}

/// The interpreter as `Interpreter::start` starts it, on the host, per `start`.
pub(crate) fn interpreter_command(start: &Start) -> Command {
    let mut command = python_command(start.home);
    command.args(ARGS).arg(PRIMARY);
    if let Some(dir) = start.dir {
        command.current_dir(dir);
    }
    if let Some(bytes) = start.ceiling {
        let (_, hard) = getrlimit(Resource::RLIMIT_DATA).expect("RLIMIT_DATA");
        // SAFETY: the closure runs between fork and exec, and makes one
        // async-signal-safe call that neither allocates nor takes a lock.
        unsafe {
            command.pre_exec(move || {
                setrlimit(Resource::RLIMIT_DATA, bytes, hard).map_err(std::io::Error::from)
            });
        }
    }
    command
}

/// Everything `pipe` yields, gathered on a thread of its own as it arrives.
pub(crate) fn capture(mut pipe: impl Read + Send + 'static) -> Arc<Mutex<String>> {
    let sink = Arc::new(Mutex::new(String::new()));
    let into = Arc::clone(&sink);
    std::thread::spawn(move || {
        let mut buf = [0; 4096];
        while let Ok(n @ 1..) = pipe.read(&mut buf) {
            into.lock()
                .expect("stderr lock")
                .push_str(&String::from_utf8_lossy(&buf[..n]));
        }
    });
    sink
}

/// The embedded payload's interpreter, unpacked once for the whole binary.
pub(crate) fn python() -> &'static Path {
    static PYTHON: OnceLock<PathBuf> = OnceLock::new();
    PYTHON.get_or_init(|| {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("a runtime to unpack the payload on");
        runtime
            .block_on(payload::host_python())
            .expect("the payload this build embedded")
    })
}

/// Python written indented inside a Rust test, dedented.
pub(crate) fn py(source: &str) -> String {
    let lines: Vec<&str> = source
        .lines()
        .skip_while(|line| line.trim().is_empty())
        .collect();
    let indent = lines
        .iter()
        .filter(|line| !line.trim().is_empty())
        .map(|line| line.len() - line.trim_start().len())
        .min()
        .unwrap_or(0);
    lines
        .iter()
        .map(|line| line.get(indent..).unwrap_or(""))
        .collect::<Vec<_>>()
        .join("\n")
}

// ---------------------------------------------------------------------------- a fake transport

/// What the fake transport says about itself once it closes.
pub(crate) const HUNG_UP: &str = "the fake hung up";

/// The interpreter's end of a transport, scripted by the test.
pub(crate) struct Fake {
    requests: Lines<BufReader<ReadHalf<DuplexStream>>>,
    pub(crate) replies: WriteHalf<DuplexStream>,
}

pub(crate) type HostEnd = (ReadHalf<DuplexStream>, WriteHalf<DuplexStream>);

impl Fake {
    pub(crate) fn pair() -> (Self, HostEnd) {
        let (host, fake) = tokio::io::duplex(1 << 16);
        let (requests, replies) = tokio::io::split(fake);
        let fake = Self {
            requests: BufReader::new(requests).lines(),
            replies,
        };
        (fake, tokio::io::split(host))
    }

    /// A handle connected to a fake that has greeted.
    pub(crate) async fn connected() -> (Interpreter, Self) {
        Self::connected_with(Events::off()).await
    }

    /// [`Fake::connected`], recording to `events`. The host's request to
    /// observe, which recording sends first, is read here.
    pub(crate) async fn connected_with(events: Events) -> (Interpreter, Self) {
        let (mut fake, host) = Self::pair();
        fake.send(json!({"t": "ready", "agent": PRIMARY, "version": "3.13"}))
            .await;
        let observing = events.is_on();
        let interpreter = within(connect_with(host, events))
            .await
            .unwrap_or_else(|e| panic!("{e}"));
        if observing {
            fake.expect("observe").await;
        }
        (interpreter, fake)
    }

    pub(crate) async fn send(&mut self, message: Value) {
        self.replies
            .write_all(format!("{message}\n").as_bytes())
            .await
            .expect("the host reads it");
    }

    pub(crate) async fn next(&mut self) -> Value {
        let line = within(self.requests.next_line())
            .await
            .expect("the host's requests")
            .expect("a request");
        serde_json::from_str(&line).expect("a request is JSON")
    }

    /// Read the next request, which must be of `kind`.
    pub(crate) async fn expect(&mut self, kind: &str) -> Value {
        let request = self.next().await;
        assert_eq!(request["t"], kind, "expected {kind}, got {request}");
        request
    }

    /// Read the next request, which must be execution `id`'s.
    pub(crate) async fn exec(&mut self, id: ExecId) {
        let request = self.expect("exec").await;
        assert_eq!(
            request["id"],
            json!(id),
            "expected execution {id}, got {request}"
        );
    }

    /// Answer the next request, which must be an inventory, with an empty one.
    pub(crate) async fn answer_inventory(&mut self) {
        let request = self.expect("inv").await;
        self.inventory(&request["id"]).await;
    }

    /// An empty inventory, as the answer to request `id`.
    pub(crate) async fn inventory(&mut self, id: &Value) {
        self.send(json!({
            "t": "inv", "agent": PRIMARY, "id": id, "globals": [], "total": 0, "more": 0
        }))
        .await;
    }

    pub(crate) async fn result(&mut self, id: ExecId, fields: Value) {
        let mut result = json!({
            "t": "result", "agent": PRIMARY, "id": id,
            "output": "", "dropped": 0, "error": null, "background": [],
        });
        for (key, value) in fields.as_object().expect("an object") {
            result[key] = value.clone();
        }
        self.send(result).await;
    }

    pub(crate) async fn ok(&mut self, id: ExecId, output: &str) {
        self.result(id, json!({"status": "ok", "output": output}))
            .await;
    }
}

pub(crate) async fn connect(host: HostEnd) -> Result<Interpreter, InterpreterError> {
    connect_with(host, Events::off()).await
}

/// [`connect`], recording to `events`.
pub(crate) async fn connect_with(
    (replies, requests): HostEnd,
    events: Events,
) -> Result<Interpreter, InterpreterError> {
    Interpreter::connect(replies, requests, async { HUNG_UP.to_string() }, events).await
}

/// An inventory, answered by the fake. The next request the fake sees must be
/// it, and once the host has its reply it has read every line before it.
pub(crate) async fn round_trip(interpreter: &Interpreter, fake: &mut Fake) {
    let (inventory, ()) = tokio::join!(within(interpreter.inventory()), fake.answer_inventory());
    inventory.expect("an inventory");
}

/// An image that ships no Python, so only the payload can answer.
#[cfg(feature = "e2e")]
pub(crate) const ALPINE: &str = "docker.io/library/alpine:latest";

/// An image whose own Python is the payload's version, so the two would share
/// a user site.
#[cfg(feature = "e2e")]
pub(crate) const PYTHON_ALPINE: &str = "docker.io/library/python:3.13-alpine";

/// Make sure `image` is present locally: pulled once per test binary, since
/// every e2e test asks and a pull of a present image is still a registry
/// round trip.
#[cfg(feature = "e2e")]
pub(crate) async fn pull(image: &str) {
    static PULLED: Mutex<Vec<String>> = Mutex::new(Vec::new());
    if PULLED.lock().unwrap().iter().any(|pulled| pulled == image) {
        return;
    }
    crate::image::pull_image(&crate::image::ImageTag::new(image))
        .await
        .unwrap_or_else(|e| panic!("{e}"));
    PULLED.lock().unwrap().push(image.to_owned());
}

// ---------------------------------------------------------------------------- pip

/// Python binding `wheel(directory, module, script=None, requires=None, tag=...,
/// version="1.0")`, which writes a wheel, pure Python unless `tag` says otherwise, whose one
/// module holds `ANSWER = 42` and a `main()` that prints it, as the console
/// script `script` when one is named and requiring the distribution `requires`
/// when one is, and returns its path; `project(directory, name, tag=None,
/// build=None, version="1.0")`, which writes what an index lists for `name` at
/// `version` -- a wheel tagged `tag` if one is given, and a source distribution
/// if `build` is -- and `build` says what the source distribution's in-tree
/// build backend does:
/// `"pure"` builds a pure-Python wheel, as MarkupSafe's source does,
/// `"fails"` raises while reading its metadata, as numpy's does without a
/// compiler, and `"late"` raises building the wheel after its metadata read
/// fine; `COMPILED`, the tag of a compiled wheel built for a platform this
/// interpreter does not match; and `pip(*args, python=None, check=True,
/// **options)`, which runs the `pip` on `PATH`, or `python -m pip`, with
/// `options` for `subprocess.run`, and returns the run -- or, if it fails and
/// `check` is true, raises with everything it said. A wheel installed with
/// `--no-index` needs no network.
pub(crate) const PIP_PROBE: &str = r#"
import io, os, subprocess, tarfile, tempfile, zipfile

COMPILED = "cp313-cp313-manylinux_2_17_x86_64"

def wheel(directory, module, script=None, requires=None, tag="py3-none-any", version="1.0"):
    dist = f"{module}-{version}.dist-info"
    path = os.path.join(directory, f"{module}-{version}-{tag}.whl")
    metadata = f"Metadata-Version: 2.1\nName: {module}\nVersion: {version}\n"
    if requires:
        metadata += f"Requires-Dist: {requires}\n"
    with zipfile.ZipFile(path, "w") as z:
        z.writestr(f"{module}.py", "ANSWER = 42\n\ndef main():\n    print(ANSWER)\n")
        z.writestr(f"{dist}/METADATA", metadata)
        z.writestr(
            f"{dist}/WHEEL",
            f"Wheel-Version: 1.0\nGenerator: outrig-tests\nRoot-Is-Purelib: true\nTag: {tag}\n",
        )
        if script:
            z.writestr(f"{dist}/entry_points.txt", f"[console_scripts]\n{script} = {module}:main\n")
        z.writestr(f"{dist}/RECORD", "")
    return path

def pip(*args, python=None, check=True, **options):
    command = [python, "-m", "pip"] if python else ["pip"]
    run = subprocess.run(
        [*command, "--isolated", "--disable-pip-version-check", "--no-cache-dir", *args],
        capture_output=True,
        text=True,
        **options,
    )
    if check and run.returncode:
        raise RuntimeError(f"pip {args} exited {run.returncode}:\n{run.stdout}{run.stderr}")
    return run

BACKEND = """
import os, shutil

BUILD = "BUILD"

def prepare_metadata_for_build_wheel(metadata_directory, config_settings=None):
    if BUILD == "fails":
        raise RuntimeError("the build failed, as one with no compiler does")
    dist = "NAME-VERSION.dist-info"
    os.makedirs(os.path.join(metadata_directory, dist))
    with open(os.path.join(metadata_directory, dist, "METADATA"), "w") as f:
        f.write("Metadata-Version: 2.1\\nName: NAME\\nVersion: VERSION\\n")
    return dist

def build_wheel(wheel_directory, config_settings=None, metadata_directory=None):
    if BUILD != "pure":
        raise RuntimeError("the build failed, as one with no compiler does")
    name = "NAME-VERSION-py3-none-any.whl"
    shutil.copy(os.path.join(os.path.dirname(__file__), "built.whl"),
                os.path.join(wheel_directory, name))
    return name
"""

def project(directory, name, tag=None, build=None, version="1.0"):
    if tag:
        wheel(directory, name, tag=tag, version=version)
    if build:
        backend = BACKEND.replace("NAME", name).replace("VERSION", version)
        members = [
            ("pyproject.toml", b'[build-system]\nrequires = []\nbuild-backend = "backend"\n'
                               b'backend-path = ["."]\n'),
            ("backend.py", backend.replace('"BUILD"', repr(build)).encode()),
        ]
        if build == "pure":
            with tempfile.TemporaryDirectory() as built:
                with open(wheel(built, name, version=version), "rb") as f:
                    members.append(("built.whl", f.read()))
        with tarfile.open(os.path.join(directory, f"{name}-{version}.tar.gz"), "w:gz") as t:
            for member, data in members:
                info = tarfile.TarInfo(f"{name}-{version}/{member}")
                info.size = len(data)
                t.addfile(info, io.BytesIO(data))
"#;

/// Builds, into the directory it is given, the wheels and the source distribution the install
/// tests point pip at: `pure` and `other`, pure-Python wheels whose module holds `ANSWER = 42`;
/// `universal`, a `py2.py3-none-any` wheel whose WHEEL file lists the two tags on two lines, as
/// `bdist_wheel` writes it; `compiled`, tagged for this interpreter and platform, which pip
/// accepts; `mislabeled`,
/// tagged pure Python with an extension module among its files; `needs_compiled`, pure and
/// requiring `compiled`; `sdistonly`, a source distribution and nothing else; `evil`, a source
/// distribution whose build backend marks the file `OUTRIG_CANARY` names when it runs; and
/// `needs_url`, pure and depending on `evil` by URL. The directory the wheels end up in is the
/// second argument, for that URL.
///
/// For the session's bindings: `verfix` 1.0 to 4.0, each whose `VERSION` is its own; and
/// `recorder`, whose factories write what they were given to the file their `out` argument names,
/// as JSON -- `make` its arguments, working directory, environment, pid and the `verfix` it
/// imported; `failing` raises; and `spawning` also starts `sleep 1000` in its process group and
/// records that child's pid.
const WHEELS: &str = r#"
import io, os, pathlib, sys, sysconfig, tarfile, zipfile

out, final, fixture = sys.argv[1], sys.argv[2], sys.argv[3]
plat = sysconfig.get_platform().replace("-", "_").replace(".", "_")
cp = f"cp{sys.version_info.major}{sys.version_info.minor}"


def wheel(name, tag, tags=None, purelib=True, requires=None, extra=(), version="1.0",
          body="ANSWER = 42\n"):
    """A wheel named for `tag`, whose WHEEL file lists `tags` -- one line per Python tag, as
    `bdist_wheel` writes a compressed tag -- or `tag` alone."""
    dist = f"{name}-{version}.dist-info"
    metadata = f"Metadata-Version: 2.1\nName: {name}\nVersion: {version}\n"
    if requires:
        metadata += f"Requires-Dist: {requires}\n"
    with zipfile.ZipFile(os.path.join(out, f"{name}-{version}-{tag}.whl"), "w") as z:
        z.writestr(f"{name}.py", body)
        for member, data in extra:
            z.writestr(member, data)
        z.writestr(f"{dist}/METADATA", metadata)
        z.writestr(
            f"{dist}/WHEEL",
            f"Wheel-Version: 1.0\nGenerator: outrig-tests\n"
            f"Root-Is-Purelib: {'true' if purelib else 'false'}\n"
            + "".join(f"Tag: {t}\n" for t in tags or [tag]),
        )
        z.writestr(f"{dist}/RECORD", "")


wheel("pure", "py3-none-any")
# The hosted-call fixture, as a session installs a binding's requirement.
wheel("outrig_fixture", "py3-none-any", body=pathlib.Path(fixture).read_text())
wheel("other", "py3-none-any")
wheel("universal", "py2.py3-none-any", tags=["py2-none-any", "py3-none-any"])
wheel("compiled", f"{cp}-{cp}-{plat}", purelib=False)
wheel("mislabeled", "py3-none-any", extra=[("mislabeled_ext.so", b"\x7fELF, or so it says")])
wheel("needs_compiled", "py3-none-any", requires="compiled")
for v in range(1, 5):
    wheel("verfix", "py3-none-any", version=f"{v}.0", body=f"VERSION = '{v}.0'\n")
wheel("recorder", "py3-none-any", body="""
import json, os, subprocess


class Recorded:
    pass


def _write(out, record):
    with open(out + ".part", "w") as f:
        json.dump(record, f)
    os.replace(out + ".part", out)


def make(*args, out, **kwargs):
    try:
        import verfix
        version = verfix.VERSION
    except ImportError:
        version = None
    _write(out, {"args": list(args), "kwargs": kwargs, "cwd": os.getcwd(),
                 "env": dict(os.environ), "pid": os.getpid(), "verfix": version})
    return Recorded()


def failing(*args, **kwargs):
    raise ValueError("the recorder was told to fail")


def spawning(*, out):
    child = subprocess.Popen(["sleep", "1000"])
    _write(out, {"pid": os.getpid(), "child": child.pid})
    return Recorded()
""")

def sdist(name, members):
    with tarfile.open(os.path.join(out, f"{name}-1.0.tar.gz"), "w:gz") as t:
        for member, data in members:
            info = tarfile.TarInfo(f"{name}-1.0/{member}")
            info.size = len(data)
            t.addfile(info, io.BytesIO(data))


sdist("sdistonly", [
    ("PKG-INFO", b"Metadata-Version: 2.1\nName: sdistonly\nVersion: 1.0\n"),
    ("sdistonly.py", b"ANSWER = 1\n"),
])
# A source distribution whose in-tree build backend leaves a mark when it runs -- the code a
# build executes on the host -- and a pure wheel that depends on it by URL, where it will lie
# once the wheels are in place.
sdist("evil", [
    ("PKG-INFO", b"Metadata-Version: 2.1\nName: evil\nVersion: 1.0\n"),
    ("pyproject.toml", b'[build-system]\nrequires = []\nbuild-backend = "evil_backend"\n'
                       b'backend-path = ["."]\n[project]\nname = "evil"\nversion = "1.0"\n'),
    ("evil_backend.py", b"import os\nopen(os.environ.get('OUTRIG_CANARY', os.devnull), 'w').close()\n"),
])
wheel("needs_url", "py3-none-any", requires=f"evil @ {pathlib.Path(final, 'evil-1.0.tar.gz').as_uri()}")
"#;

/// The directory of test wheels [`WHEELS`] builds, made once per content under the system's
/// temporary directory, for pip's `--find-links`.
pub(crate) fn wheel_links() -> &'static Path {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        let key = blake3::hash(format!("{WHEELS}\0{FIXTURE_SOURCE}").as_bytes());
        let dir = std::env::temp_dir().join(format!(
            "outrig-test-wheels-{}-{}",
            nix::unistd::getuid(),
            &key.to_hex()[..16]
        ));
        if dir.join("done").is_file() {
            return dir;
        }
        let stage = tempfile::Builder::new()
            .prefix(".outrig-wheels-")
            .tempdir_in(std::env::temp_dir())
            .expect("a staging directory");
        let fixture = tempfile::NamedTempFile::new().expect("a file for the fixture's source");
        std::fs::write(fixture.path(), FIXTURE_SOURCE).expect("write the fixture's source");
        let output = Command::new(python())
            .args(["-I", "-c", WHEELS])
            .arg(stage.path())
            .arg(&dir)
            .arg(fixture.path())
            .env("HOME", host_home())
            .output()
            .expect("the payload's Python runs");
        assert!(
            output.status.success(),
            "building the test wheels failed: {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        std::fs::write(stage.path().join("done"), b"").expect("mark the wheels done");
        match std::fs::rename(stage.path(), &dir) {
            Ok(()) => {}
            Err(_) if dir.join("done").is_file() => {}
            Err(e) => panic!("move the wheels to {}: {e}", dir.display()),
        }
        dir
    })
}

/// pip's environment for a test install: no index, `links` for `--find-links` -- as a `file://`
/// URI, since pip splits the variable on whitespace and a temporary directory may hold a space
/// -- and no configuration file, so a developer's `pip.conf` reaches nothing.
pub(crate) fn pip_env(links: &Path) -> Vec<(std::ffi::OsString, std::ffi::OsString)> {
    [
        ("PIP_NO_INDEX", std::ffi::OsString::from("1")),
        ("PIP_FIND_LINKS", std::ffi::OsString::from(file_uri(links))),
        ("PIP_CONFIG_FILE", std::ffi::OsString::from("/dev/null")),
    ]
    .into_iter()
    .map(|(k, v)| (std::ffi::OsString::from(k), v))
    .collect()
}

/// `path`, absolute, as a `file://` URI with every byte outside the unreserved set and `/`
/// percent-encoded, as `pathlib.Path.as_uri` writes one.
pub(crate) fn file_uri(path: &Path) -> String {
    let mut uri = String::from("file://");
    for byte in path.as_os_str().as_encoded_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => {
                uri.push(char::from(*byte));
            }
            other => uri.push_str(&format!("%{other:02X}")),
        }
    }
    uri
}
