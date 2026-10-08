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
use crate::events::Events;

/// How long any one step may take before a test fails rather than hangs.
const TIMEOUT: Duration = Duration::from_secs(20);

/// `step`, or a panic once it has taken longer than [`TIMEOUT`].
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

    pub(crate) fn path(&self) -> &Path {
        &self.path
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
    let python = payload::host_dir()
        .await
        .expect("the payload this build embedded")
        .join("bin/python3");
    tokio::process::Command::new(python)
        .args(ARGS)
        .args(agent)
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
    within(Interpreter::from_child(spawn(Some(PRIMARY)).await, events))
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
        let dir = runtime
            .block_on(payload::host_dir())
            .expect("the payload this build embedded");
        dir.join("bin/python3")
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

/// Python binding `wheel(directory, module, script=None, requires=None)`,
/// which writes a pure-Python wheel whose one module holds `ANSWER = 42` and a
/// `main()` that prints it, as the console script `script` when one is named
/// and requiring the distribution `requires` when one is, and returns its
/// path; and `pip(*args, python=None, **options)`, which runs the `pip` on
/// `PATH`, or `python -m pip`, with `options` for `subprocess.run`, and raises
/// with everything it said if it fails. A wheel installed with `--no-index`
/// needs no network.
pub(crate) const PIP_PROBE: &str = r#"
import os, subprocess, zipfile

def wheel(directory, module, script=None, requires=None):
    dist = f"{module}-1.0.dist-info"
    path = os.path.join(directory, f"{module}-1.0-py3-none-any.whl")
    metadata = f"Metadata-Version: 2.1\nName: {module}\nVersion: 1.0\n"
    if requires:
        metadata += f"Requires-Dist: {requires}\n"
    with zipfile.ZipFile(path, "w") as z:
        z.writestr(f"{module}.py", "ANSWER = 42\n\ndef main():\n    print(ANSWER)\n")
        z.writestr(f"{dist}/METADATA", metadata)
        z.writestr(
            f"{dist}/WHEEL",
            "Wheel-Version: 1.0\nGenerator: outrig-tests\nRoot-Is-Purelib: true\nTag: py3-none-any\n",
        )
        if script:
            z.writestr(f"{dist}/entry_points.txt", f"[console_scripts]\n{script} = {module}:main\n")
        z.writestr(f"{dist}/RECORD", "")
    return path

def pip(*args, python=None, **options):
    command = [python, "-m", "pip"] if python else ["pip"]
    run = subprocess.run(
        [*command, "--isolated", "--disable-pip-version-check", "--no-cache-dir", *args],
        capture_output=True,
        text=True,
        **options,
    )
    if run.returncode:
        raise RuntimeError(f"pip {args} exited {run.returncode}:\n{run.stdout}{run.stderr}")
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
const WHEELS: &str = r#"
import io, os, pathlib, sys, sysconfig, tarfile, zipfile

out, final = sys.argv[1], sys.argv[2]
plat = sysconfig.get_platform().replace("-", "_").replace(".", "_")
cp = f"cp{sys.version_info.major}{sys.version_info.minor}"


def wheel(name, tag, tags=None, purelib=True, requires=None, extra=()):
    """A wheel named for `tag`, whose WHEEL file lists `tags` -- one line per Python tag, as
    `bdist_wheel` writes a compressed tag -- or `tag` alone."""
    dist = f"{name}-1.0.dist-info"
    metadata = f"Metadata-Version: 2.1\nName: {name}\nVersion: 1.0\n"
    if requires:
        metadata += f"Requires-Dist: {requires}\n"
    with zipfile.ZipFile(os.path.join(out, f"{name}-1.0-{tag}.whl"), "w") as z:
        z.writestr(f"{name}.py", "ANSWER = 42\n")
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
wheel("other", "py3-none-any")
wheel("universal", "py2.py3-none-any", tags=["py2-none-any", "py3-none-any"])
wheel("compiled", f"{cp}-{cp}-{plat}", purelib=False)
wheel("mislabeled", "py3-none-any", extra=[("mislabeled_ext.so", b"\x7fELF, or so it says")])
wheel("needs_compiled", "py3-none-any", requires="compiled")

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
        let key = blake3::hash(WHEELS.as_bytes());
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
        let output = Command::new(python())
            .args(["-I", "-c", WHEELS])
            .arg(stage.path())
            .arg(&dir)
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
