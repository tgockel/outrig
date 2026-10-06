//! A relay between one interpreter and the binding processes of its session, all started on
//! the host, standing in for the one `0003-21` puts in `host.rs`.
//!
//! The interpreter is started as `interpreter_tests.rs` starts it, with the vendored RPyC's
//! directory as its second argument; each binding runs `binding.py` with that directory, the
//! fixture package, and a factory. `rpc` lines from the interpreter go to the binding they name,
//! lines from a binding go back with the binding's name added, and everything else the
//! interpreter says is handed to the test. The relay counts the lines each way and remembers the
//! longest, which is how a test checks the frame bound held in transit.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;
use serde_json::{Value, json};

use super::host::PRIMARY;
use super::payload;
use super::testing::{Start, capture, host_home, interpreter_command, python, python_command};

/// How long any one reply may take. A 64 MiB result crosses in a few seconds on a loaded runner.
pub(crate) const TIMEOUT: Duration = Duration::from_secs(60);

/// The binding program as the payload's `-c` argument, staged by `build.rs` as `host.rs`'s
/// `PROGRAM` is.
pub(crate) const BINDING: &str = include_str!(concat!(env!("OUT_DIR"), "/binding.bootstrap"));

/// The binding program's source, for the test that reads its handler table.
pub(crate) const BINDING_SOURCE: &str = include_str!("binding.py");

/// The vendored RPyC's directory, unpacked once for the whole test binary from the embedded
/// wheel, as a session's first start would.
pub(crate) fn rpyc_dir() -> &'static Path {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("a runtime to unpack the wheel on");
        runtime
            .block_on(payload::rpyc_dir())
            .expect("the RPyC wheel this build embedded")
    })
}

/// The fixture library as a module: an object with public and private members, a nested object,
/// a sequence, a context manager, methods that call back, report types, and return a large
/// result, two exception classes, and a counter around `pickle.loads`.
pub(crate) const FIXTURE_SOURCE: &str = r#"
"""The fixture library a binding process hosts in tests."""

import os
import pickle
import sys


class FixtureError(Exception):
    """Carries a public attribute beside its args."""

    def __init__(self, message, status):
        super().__init__(message)
        self.status = status


class CommandError(Exception):
    """Reads private state in __str__, as GitPython's GitCommandError reads _cmdline."""

    def __init__(self, command, status):
        super().__init__(command, status)
        self._cmdline = " ".join(command)
        self.status = status

    def __str__(self):
        return f"command {self._cmdline!r} failed with status {self.status}"


class Nested:
    def __init__(self):
        self.value = 7
        self._secret = "hidden"

    def method(self):
        return "nested method"


class Manager:
    def __init__(self):
        self.entered = 0
        self.exits = []

    def __enter__(self):
        self.entered += 1
        return self

    def __exit__(self, typ, value, tb):
        self.exits.append((None if typ is None else typ.__name__, repr(value), tb is None))
        return False


class Callable:
    def __call__(self, x):
        return x * 2

    def public(self):
        return 1

    def _private(self):
        return 2


unpickled = 0
_real_loads = pickle.loads


def _counting_loads(data, *args, **kwargs):
    global unpickled
    unpickled += 1
    return _real_loads(data, *args, **kwargs)


pickle.loads = _counting_loads


class Root:
    def __init__(self):
        self.public = "public value"
        self._private = "private value"
        self.writable = "before"
        self.nested = Nested()
        self.sequence = [10, 20, 30]
        self.manager = Manager()
        self.callable = Callable()
        self.kept = None
        self.nested_type = Nested
        self.str_type = str
        self.calls = 0
        self.last_raised = None

    def _private_method(self):
        return "private"

    def method(self, x, y=1):
        return x + y

    def takes(self, obj):
        return obj is self.nested

    def types(self, *args):
        return tuple(type(a).__name__ for a in args)

    def shape(self, obj):
        """The types of a nested value, as text."""
        if isinstance(obj, dict):
            items = sorted(obj.items(), key=repr)
            return "{" + ", ".join(f"{self.shape(k)}: {self.shape(v)}" for k, v in items) + "}"
        if isinstance(obj, (list, tuple, set, frozenset)):
            return type(obj).__name__ + "[" + ", ".join(sorted(self.shape(i) for i in obj)) + "]"
        return type(obj).__name__

    def noted(self, *args):
        """Counts its calls, so a test can show a refused request never reached it."""
        self.calls += 1
        return self.calls

    def call(self, fn, *args):
        return fn(*args)

    def returned_type(self, fn):
        return type(fn()).__name__

    def calling_raised(self, fn):
        """The exception's type name when calling `fn` raises, else None; kept in
        `last_raised` too, for a caller whose own execution the exception ended."""
        try:
            fn()
        except BaseException as e:
            self.last_raised = type(e).__name__
            return self.last_raised
        self.last_raised = None
        return None

    def keep(self, fn):
        self.kept = fn

    def call_kept(self):
        return self.kept(1)

    def probe(self, fn):
        """What a callback allows besides being called, during the call: the exception's type
        for each attempt, or 'ok'."""
        import copy

        results = {}
        for name, attempt in (
            ("attr", lambda: fn.anything),
            ("repr", lambda: repr(fn)),
            ("pickle", lambda: pickle.dumps(fn)),
            ("copy", lambda: copy.copy(fn)),
            ("call", lambda: fn(3)),
        ):
            try:
                results[name] = ("ok", repr(attempt()))
            except Exception as e:
                results[name] = (type(e).__name__, str(e)[:60])
        return results

    def big(self, n):
        return os.urandom(n)

    def gen(self):
        yield 1
        yield 2

    def frames(self):
        return (sys._getframe(),)

    def frames_with_nested(self):
        return (self.nested, sys._getframe())

    def call_with_bytes(self, fn, n):
        """Call `fn` with `n` random bytes; the error's text if that raises, else the result."""
        try:
            return fn(os.urandom(n))
        except Exception as e:
            return f"{type(e).__name__}: {e}"

    def note_raised(self, fn):
        """What calling `fn` raised, with a note added to it here, as `(type name, notes)`."""
        try:
            fn()
        except Exception as e:
            e.add_note("noted on the host")
            return (type(e).__name__, list(e.__notes__))
        return None

    def raise_fixture(self):
        raise FixtureError("fixture failed", 3)

    def raise_command(self):
        raise CommandError(["git", "push"], 128)

    def unpickled(self):
        return unpickled


def make():
    return Root()
"#;

/// A module the binding process could import, which writes a file when it is: the host must
/// never import a module the container names.
pub(crate) const EVIL_SOURCE: &str = r#"
import os

open(os.environ["OUTRIG_FIXTURE_EVIL_MARKER"], "w").close()


class Evil(Exception):
    pass
"#;

/// Builds the fixture wheel with the payload's `zipfile` and installs it with the payload's pip
/// into the target directory it is given, as `pip_installs_a_pure_package_that_imports_at_once`
/// does.
const INSTALL: &str = r#"
import os, subprocess, sys, zipfile
sources, wheels, target = sys.argv[1:]
path = os.path.join(wheels, "outrig_fixture-1.0-py3-none-any.whl")
dist = "outrig_fixture-1.0.dist-info"
with zipfile.ZipFile(path, "w") as z:
    z.write(os.path.join(sources, "outrig_fixture.py"), "outrig_fixture/__init__.py")
    z.write(os.path.join(sources, "fixture_evil.py"), "fixture_evil.py")
    z.writestr(f"{dist}/METADATA", "Metadata-Version: 2.1\nName: outrig_fixture\nVersion: 1.0\n")
    z.writestr(
        f"{dist}/WHEEL",
        "Wheel-Version: 1.0\nGenerator: outrig-tests\nRoot-Is-Purelib: true\nTag: py3-none-any\n",
    )
    z.writestr(f"{dist}/RECORD", "")
run = subprocess.run(
    [sys.executable, "-P", "-m", "pip", "install", "--isolated", "--disable-pip-version-check",
     "--no-cache-dir", "--no-index", "--target", target, path],
    capture_output=True, text=True,
)
if run.returncode:
    sys.exit(f"pip exited {run.returncode}:\n{run.stdout}{run.stderr}")
"#;

/// The fixture library, installed from its wheel once per content under the system's temporary
/// directory, so a test binary pays pip once and a rerun not at all.
pub(crate) fn fixture_dir() -> &'static Path {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        let key = blake3::hash(format!("{FIXTURE_SOURCE}\0{EVIL_SOURCE}\0{INSTALL}").as_bytes());
        let dir = std::env::temp_dir().join(format!(
            "outrig-test-fixture-{}-{}",
            nix::unistd::getuid(),
            &key.to_hex()[..16]
        ));
        if dir.join("outrig_fixture/__init__.py").is_file() {
            return dir;
        }
        let stage = tempfile::Builder::new()
            .prefix(".outrig-fixture-")
            .tempdir_in(std::env::temp_dir())
            .expect("a staging directory");
        let sources = stage.path().join("sources");
        std::fs::create_dir_all(&sources).expect("create the sources directory");
        std::fs::write(sources.join("outrig_fixture.py"), FIXTURE_SOURCE).expect("write");
        std::fs::write(sources.join("fixture_evil.py"), EVIL_SOURCE).expect("write");
        let wheels = stage.path().join("wheels");
        std::fs::create_dir_all(&wheels).expect("create the wheels directory");
        let target = stage.path().join("target");
        let output = Command::new(python())
            .args(["-I", "-c", INSTALL])
            .args([&sources, &wheels, &target])
            .env("HOME", host_home())
            .env_remove("PYTHONUSERBASE")
            .output()
            .expect("the payload's Python runs");
        assert!(
            output.status.success(),
            "installing the fixture failed: {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        match std::fs::rename(&target, &dir) {
            Ok(()) => {}
            Err(_) if dir.is_dir() => {}
            Err(e) => panic!("move the fixture to {}: {e}", dir.display()),
        }
        dir
    })
}

/// How many lines crossed one way, and the longest of them, in bytes.
#[derive(Default)]
pub(crate) struct Counter {
    lines: AtomicUsize,
    longest: AtomicUsize,
}

impl Counter {
    fn record(&self, len: usize) {
        self.lines.fetch_add(1, Ordering::SeqCst);
        self.longest.fetch_max(len, Ordering::SeqCst);
    }

    pub(crate) fn lines(&self) -> usize {
        self.lines.load(Ordering::SeqCst)
    }

    pub(crate) fn longest(&self) -> usize {
        self.longest.load(Ordering::SeqCst)
    }
}

/// What crossed the relay: `rpc` lines toward the bindings and toward the interpreter, and how
/// many of those toward a binding were close notices.
#[derive(Default)]
pub(crate) struct Stats {
    pub(crate) to_binding: Counter,
    pub(crate) to_interpreter: Counter,
    closed_to_binding: AtomicUsize,
}

impl Stats {
    pub(crate) fn closed_to_binding(&self) -> usize {
        self.closed_to_binding.load(Ordering::SeqCst)
    }
}

struct BindingProcess {
    child: Child,
    stdin: Arc<Mutex<ChildStdin>>,
    stderr: Arc<Mutex<String>>,
}

pub(crate) struct Relay {
    interpreter: Child,
    stdin: Arc<Mutex<ChildStdin>>,
    other: Receiver<Result<Value, String>>,
    stderr: Arc<Mutex<String>>,
    bindings: Arc<Mutex<HashMap<String, BindingProcess>>>,
    pub(crate) stats: Arc<Stats>,
}

/// Write `message` as one line, returning its length; a write to a process that has exited is
/// dropped, since the test learns of the exit another way.
fn write_line(stdin: &Mutex<ChildStdin>, message: &Value) -> usize {
    let line = format!("{message}\n");
    let mut stdin = stdin.lock().expect("stdin lock");
    let _ = stdin
        .write_all(line.as_bytes())
        .and_then(|()| stdin.flush());
    line.len()
}

impl Relay {
    pub(crate) fn start() -> Self {
        Self::start_with(&Start::default())
    }

    pub(crate) fn start_with(start: &Start) -> Self {
        let mut child = interpreter_command(start)
            .arg(rpyc_dir())
            .spawn()
            .expect("the interpreter starts");
        let stdin = Arc::new(Mutex::new(child.stdin.take().expect("stdin is piped")));
        let stdout = child.stdout.take().expect("stdout is piped");
        let stderr = capture(child.stderr.take().expect("stderr is piped"));
        let bindings = Arc::new(Mutex::new(HashMap::new()));
        let stats = Arc::new(Stats::default());
        let (tx, other) = channel();
        {
            let (stdin, bindings, stats) = (
                Arc::clone(&stdin),
                Arc::clone(&bindings),
                Arc::clone(&stats),
            );
            std::thread::spawn(move || pump_interpreter(stdout, tx, stdin, bindings, stats));
        }
        let mut relay = Self {
            interpreter: child,
            stdin,
            other,
            stderr,
            bindings,
            stats,
        };
        let ready = relay.recv();
        assert_eq!(ready["t"], "ready", "{ready}");
        assert_eq!(ready["agent"], PRIMARY, "{ready}");
        relay
    }

    /// Start binding `name`'s process over the fixture, with `factory` as `module:callable`,
    /// and wait for it to be ready.
    pub(crate) fn bind(&mut self, name: &str, factory: &str) {
        self.bind_with(name, factory, &[]);
    }

    /// [`Relay::bind`] with extra environment variables for the binding's process.
    pub(crate) fn bind_with(&mut self, name: &str, factory: &str, env: &[(&str, &str)]) {
        let mut child = python_command(None)
            .args(["-I", "-c", BINDING])
            .arg(rpyc_dir())
            .arg(fixture_dir())
            .arg(factory)
            .envs(env.iter().copied())
            .spawn()
            .expect("the binding starts");
        let stdin = Arc::new(Mutex::new(child.stdin.take().expect("stdin is piped")));
        let stdout = child.stdout.take().expect("stdout is piped");
        let stderr = capture(child.stderr.take().expect("stderr is piped"));
        let (ready_tx, ready_rx) = channel();
        {
            let (name, stdin, stats) = (
                name.to_string(),
                Arc::clone(&self.stdin),
                Arc::clone(&self.stats),
            );
            std::thread::spawn(move || pump_binding(name, stdout, stdin, ready_tx, stats));
        }
        match ready_rx.recv_timeout(TIMEOUT) {
            Ok(()) => {}
            Err(_) => panic!(
                "binding {name:?} did not greet; its stderr: {}",
                stderr.lock().expect("stderr lock")
            ),
        }
        self.bindings.lock().expect("bindings lock").insert(
            name.to_string(),
            BindingProcess {
                child,
                stdin,
                stderr,
            },
        );
    }

    /// Write `message` as one line to the interpreter's stdin, as the host would.
    pub(crate) fn send(&mut self, message: Value) {
        write_line(&self.stdin, &message);
    }

    /// Write one raw line to binding `name`'s stdin, as the host would.
    pub(crate) fn inject_to_binding(&mut self, name: &str, message: Value) {
        let bindings = self.bindings.lock().expect("bindings lock");
        let binding = bindings.get(name).expect("a started binding");
        write_line(&binding.stdin, &message);
    }

    /// The next message from the interpreter that is not an `rpc` line.
    pub(crate) fn recv(&mut self) -> Value {
        let message = match self.other.recv_timeout(TIMEOUT) {
            Ok(Ok(message)) => message,
            Ok(Err(bad)) => panic!("{bad}"),
            Err(RecvTimeoutError::Timeout) => {
                panic!("quiet for {TIMEOUT:?}; stderr: {}", self.stderr())
            }
            Err(RecvTimeoutError::Disconnected) => {
                panic!("the interpreter exited; stderr: {}", self.stderr())
            }
        };
        assert!(
            message["agent"].is_string(),
            "a message without an agent id: {message}"
        );
        message
    }

    /// Submit `source` to `agent` and return its result, which must be the very next message.
    pub(crate) fn exec_in(&mut self, agent: &str, id: u64, source: &str) -> Value {
        self.send(json!({"t": "exec", "agent": agent, "id": id, "src": source}));
        let result = self.recv();
        assert!(
            result["t"] == "result" && result["agent"] == agent && result["id"] == id,
            "expected the result of {agent}/{id}, got: {result}"
        );
        result
    }

    pub(crate) fn exec(&mut self, id: u64, source: &str) -> Value {
        self.exec_in(PRIMARY, id, source)
    }

    /// Run `source` in `agent`, asserting it did not raise, and return what it printed.
    pub(crate) fn output_in(&mut self, agent: &str, id: u64, source: &str) -> String {
        let result = self.exec_in(agent, id, source);
        assert_eq!(
            result["status"],
            "ok",
            "unexpected raise: {result}\ninterpreter stderr: {}\nbindings: {}",
            self.stderr(),
            self.binding_stderr()
        );
        result["output"]
            .as_str()
            .unwrap_or_else(|| panic!("no output: {result}"))
            .to_string()
    }

    pub(crate) fn output(&mut self, id: u64, source: &str) -> String {
        self.output_in(PRIMARY, id, source)
    }

    /// Open a kernel for `agent` and wait for its greeting.
    pub(crate) fn open(&mut self, agent: &str) {
        self.send(json!({"t": "open", "agent": agent}));
        let ready = self.recv();
        assert!(
            ready["t"] == "ready" && ready["agent"] == agent,
            "got: {ready}"
        );
    }

    pub(crate) fn stderr(&self) -> String {
        self.stderr.lock().expect("stderr lock").clone()
    }

    /// Every binding's stderr, labeled.
    pub(crate) fn binding_stderr(&self) -> String {
        let bindings = self.bindings.lock().expect("bindings lock");
        let mut text = String::new();
        for (name, binding) in bindings.iter() {
            let said = binding.stderr.lock().expect("stderr lock");
            if !said.is_empty() {
                text.push_str(&format!("[{name}] {said}"));
            }
        }
        text
    }
}

impl Drop for Relay {
    fn drop(&mut self) {
        if let Ok(pid) = i32::try_from(self.interpreter.id()) {
            let _ = killpg(Pid::from_raw(pid), Signal::SIGKILL);
        }
        let _ = self.interpreter.wait();
        let mut bindings = self.bindings.lock().expect("bindings lock");
        for binding in bindings.values_mut() {
            if let Ok(pid) = i32::try_from(binding.child.id()) {
                let _ = killpg(Pid::from_raw(pid), Signal::SIGKILL);
            }
            let _ = binding.child.wait();
        }
    }
}

/// Route the interpreter's lines: `rpc` lines to the binding they name, the rest to the test.
fn pump_interpreter(
    stdout: impl Read,
    tx: Sender<Result<Value, String>>,
    stdin: Arc<Mutex<ChildStdin>>,
    bindings: Arc<Mutex<HashMap<String, BindingProcess>>>,
    stats: Arc<Stats>,
) {
    for line in BufReader::with_capacity(1 << 20, stdout).lines() {
        let line = match line {
            Ok(line) => line,
            Err(e) => {
                let _ = tx.send(Err(format!("stdout: {e}")));
                return;
            }
        };
        let mut message = match serde_json::from_str::<Value>(&line) {
            Ok(message) if message.is_object() => message,
            _ => {
                let _ = tx.send(Err(format!("not a protocol message: {line:?}")));
                continue;
            }
        };
        if message["t"] != "rpc" {
            if tx.send(Ok(message)).is_err() {
                return;
            }
            continue;
        }
        stats.to_binding.record(line.len());
        if message.get("closed").is_some() {
            stats.closed_to_binding.fetch_add(1, Ordering::SeqCst);
        }
        let binding = message
            .as_object_mut()
            .expect("an object")
            .remove("binding")
            .unwrap_or(Value::Null);
        let target = binding.as_str().and_then(|name| {
            bindings
                .lock()
                .expect("bindings lock")
                .get(name)
                .map(|binding| Arc::clone(&binding.stdin))
        });
        match target {
            Some(target) => {
                write_line(&target, &message);
            }
            None => {
                write_line(
                    &stdin,
                    &json!({
                        "t": "rpc", "agent": message["agent"], "binding": binding,
                        "id": message["id"], "closed": "no such binding",
                    }),
                );
            }
        }
    }
}

/// Route a binding's lines: `rpc` lines to the interpreter with the binding's name added, its
/// greeting to whoever waits for it.
fn pump_binding(
    name: String,
    stdout: impl Read,
    interpreter: Arc<Mutex<ChildStdin>>,
    ready: Sender<()>,
    stats: Arc<Stats>,
) {
    for line in BufReader::with_capacity(1 << 20, stdout).lines() {
        let Ok(line) = line else { return };
        let Ok(mut message) = serde_json::from_str::<Value>(&line) else {
            eprintln!("binding {name:?} wrote a line that is not a message: {line:?}");
            continue;
        };
        if message["t"] == "rpc" {
            message["binding"] = json!(name);
            let len = write_line(&interpreter, &message);
            stats.to_interpreter.record(len);
        } else if message["t"] == "ready" {
            let _ = ready.send(());
        } else {
            eprintln!("binding {name:?} said: {message}");
        }
    }
}
