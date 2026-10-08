//! A relay between one interpreter and the binding processes of its session, all started on
//! the host, standing in for the one `0003-21` puts in `host.rs`.
//!
//! The interpreter is started as `interpreter_tests.rs` starts it, with the vendored RPyC's
//! directory as its second argument; each binding is started through `supervisor.rs`, as a
//! session will start it, over the fixture package with a factory. `rpc` lines from the
//! interpreter go to the binding they name, lines from a binding go back with the binding's name
//! added, and everything else the interpreter says is handed to the test, as are a binding's
//! event lines and decision requests. The relay counts the lines each way and remembers the
//! longest, which is how a test checks the frame bound held in transit.

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::ffi::OsString;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;
use serde_json::{Value, json};
use tokio::runtime::Runtime;
use tokio::sync::mpsc;

use super::host::PRIMARY;
use super::payload;
use super::supervisor::{self, Binding, Decision, Spec, Stopped};
use super::testing::{Start, capture, host_home, interpreter_command, python, python_command};

/// How long any one reply may take. A 64 MiB result crosses in a few seconds on a loaded runner.
pub(crate) const TIMEOUT: Duration = Duration::from_secs(60);

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
/// result, two exception classes, a counter around `pickle.loads`, a recording method that notes
/// when it ran on the host and sleeps or waits for a file, and objects made on request with a
/// weak reference kept, so a test can watch the binding's table drop them.
pub(crate) const FIXTURE_SOURCE: &str = r#"
"""The fixture library a binding process hosts in tests."""

import itertools
import os
import pickle
import signal
import subprocess
import sys
import threading
import time
import weakref


class Records:
    """The service shape: a wait that blocks until someone answers, a question asked for a ticket
    and polled for, and records in bulk. One implementation of the state, used in the binding's
    process by `Root` and in the service process the two-session measurement starts."""

    def __init__(self):
        self._answer = threading.Event()
        self._answer_value = None
        self._tickets = {}  # ticket -> [question, polls so far]
        self._next_ticket = itertools.count(1)
        self._records_lock = threading.Lock()
        self._records = [
            {"id": i, "name": f"record {i}", "score": i * 1.5, "flag": bool(i % 2)}
            for i in range(1000)
        ]

    def wait_for_answer(self, until=None, timeout=600):
        """Block until `answer` is called -- or, given `until`, until that file exists: the one
        release that works inside a binding started with `--serialize`, where `answer` would
        wait for the lock this call holds."""
        deadline = time.monotonic() + timeout
        while True:
            if self._answer.wait(0.05):
                return self._answer_value
            if until is not None and os.path.exists(until):
                return "released by file"
            if time.monotonic() > deadline:
                raise TimeoutError("nobody answered")

    def answer(self, value):
        self._answer_value = value
        self._answer.set()

    def ask(self, question):
        """A ticket for `question`, answered on the second poll."""
        ticket = next(self._next_ticket)
        self._tickets[ticket] = [question, 0]
        return ticket

    def poll(self, ticket):
        entry = self._tickets[ticket]
        entry[1] += 1
        return f"answer to {entry[0]}" if entry[1] >= 2 else None

    def list_records(self, n):
        """The first `n` records as a list of dicts, which the agent receives proxied."""
        with self._records_lock:
            return [dict(record) for record in self._records[:n]]

    def records_by_value(self, n):
        """The first `n` records as field names and a tuple of tuples, which cross by value."""
        with self._records_lock:
            rows = tuple(
                (record["id"], record["name"], record["score"], record["flag"])
                for record in self._records[:n]
            )
        return ("id", "name", "score", "flag"), rows

    def update_records(self, changes):
        """Apply `changes`, dicts with an `id`, and return how many."""
        with self._records_lock:
            by_id = {record["id"]: record for record in self._records}
            for change in changes:
                by_id[change["id"]].update(change)
        return len(changes)


class ServiceClient:
    """A client of the service process, as a Rust service would ship one: each method is one
    round trip over a Unix socket. One connection per calling thread, since the binding serves
    its connections on several threads and one socket cannot carry two exchanges at once."""

    def __init__(self, address):
        self._address = address
        self._local = threading.local()

    def _connection(self):
        conn = getattr(self._local, "conn", None)
        if conn is None:
            from multiprocessing.connection import Client

            conn = self._local.conn = Client(self._address, family="AF_UNIX", authkey=b"fixture")
        return conn

    def _call(self, method, *args, **kwargs):
        conn = self._connection()
        conn.send((method, args, kwargs))
        status, value = conn.recv()
        if status == "error":
            raise RuntimeError(value)
        return value

    def wait_for_answer(self, until=None, timeout=600):
        return self._call("wait_for_answer", until=until, timeout=timeout)

    def answer(self, value):
        return self._call("answer", value)

    def ask(self, question):
        return self._call("ask", question)

    def poll(self, ticket):
        return self._call("poll", ticket)

    def list_records(self, n):
        return self._call("list_records", n)

    def records_by_value(self, n):
        return self._call("records_by_value", n)

    def update_records(self, changes):
        return self._call("update_records", changes)


def service_client():
    """The factory of a binding that is a client of the service process."""
    return ServiceClient(os.environ["OUTRIG_FIXTURE_SERVICE"])


class Fresh:
    """An object made on request, so a test can watch the binding's table drop it."""

    def __init__(self, root, name):
        self._root = root
        self.name = name

    def record(self, **kwargs):
        return self._root.record(**kwargs)


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


class Root(Records):
    def __init__(self):
        Records.__init__(self)
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
        self._lock = threading.Lock()
        self._intervals = []
        self._kept = {}  # name -> (weak reference, strong reference or None)

    def _private_method(self):
        return "private"

    def record(self, seconds=0, label="", started=None, until=None):
        """The recording method: notes when it starts and ends here, on the host, and the thread
        that served it. It sleeps `seconds`, or, given `until`, waits for that file to exist with
        `seconds or 60` as the ceiling; `started` is a file it creates on starting, so a test
        knows the call is in flight."""
        start = time.monotonic()
        if started:
            open(started, "w").close()
        if until is None:
            time.sleep(seconds)
        else:
            deadline = start + (seconds or 60)
            while not os.path.exists(until):
                if time.monotonic() > deadline:
                    raise TimeoutError(f"{until} never appeared")
                time.sleep(0.01)
        self._note(label, start)
        return label

    def hold_callable(self, fn, started=None, until=None):
        """Wait as `record` does, holding `fn` without calling it: a call that carries a callable
        and blocks."""
        self.record(0, "hold", started=started, until=until)

    def call_then_fresh(self, fn, name):
        """Call `fn`, swallow what it raises -- a connection that closed under it -- and return a
        fresh object under `name`, as a library that survives a failed callback and still
        answers would."""
        try:
            fn()
        except Exception:
            pass
        return self.fresh(name)

    def call_recorded(self, fn, label):
        """`fn()`, with its interval recorded as `record` records one."""
        start = time.monotonic()
        try:
            return fn()
        finally:
            self._note(label, start)

    def _note(self, label, start):
        with self._lock:
            self._intervals.append((label, start, time.monotonic(), threading.current_thread().name))

    def intervals(self):
        """Every recorded interval as `(label, start, end, thread name)`, by value."""
        with self._lock:
            return tuple(self._intervals)

    def thread_name(self):
        return threading.current_thread().name

    def fresh(self, name):
        """A new object, kept here under `name` with a weak reference beside it."""
        obj = Fresh(self, name)
        self._kept[name] = (weakref.ref(obj), obj)
        return obj

    def again(self, name):
        """The object `fresh` made under `name`, once more."""
        return self._kept[name][1]

    def let_go(self, name):
        """Drop this module's own reference to `name`'s object, so the binding's table alone
        holds it."""
        ref, _ = self._kept[name]
        self._kept[name] = (ref, None)

    def alive(self, name):
        """Whether `name`'s object still exists, after a collection."""
        import gc

        gc.collect()
        return self._kept[name][0]() is not None

    def sleep_child(self, seconds, started=None):
        """Run `sleep seconds` as a child of this process and wait for it: a call in flight whose
        grandchild the owner must end with the binding. `started` is a file created once the
        child runs."""
        child = subprocess.Popen(["sleep", str(seconds)])
        if started:
            open(started, "w").close()
        return child.wait()

    def event(self, payload):
        """Publish `payload` as an event line, through the binding program's own primitive."""
        import __main__

        __main__._event(payload)

    def decide(self, payload):
        """Hold this call until the owner answers `payload`, through the binding program's own
        primitive, and return the answer."""
        import __main__

        return __main__._decide(payload)

    def decide_path(self, raw):
        """`decide` with a path the host made from the bytes `raw`, as a library reports a file
        name: not UTF-8 when the bytes are not."""
        import __main__

        return __main__._decide({"path": os.fsdecode(raw)})

    def env(self, name):
        return os.environ.get(name)

    def child_env(self, name):
        """`name` as a program this process starts sees it."""
        run = subprocess.run(["sh", "-c", f"echo ${name}"], capture_output=True, text=True)
        return run.stdout.strip()

    def sys_path(self):
        return tuple(sys.path)

    def pgid(self):
        return os.getpgrp()

    def method(self, x, y=1):
        return x + y

    def takes(self, obj):
        return obj is self.nested

    def echo(self, x):
        """`x` itself, so a callable passed in comes back as a reference to it."""
        return x

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


def make_stubborn():
    """A binding that ignores SIGTERM, with a child that ignores it too: what only SIGKILL ends.
    Set here, on the main thread, where `signal.signal` is allowed."""
    signal.signal(signal.SIGTERM, signal.SIG_IGN)
    subprocess.Popen(["sh", "-c", 'trap "" TERM; sleep 1000'])
    return Root()


def make_failing():
    """A factory that starts a program and then raises, writing its process group to
    `OUTRIG_FIXTURE_PGID_FILE` first, so a test can look for what it left behind."""
    subprocess.Popen(["sleep", "1000"])
    with open(os.environ["OUTRIG_FIXTURE_PGID_FILE"], "w") as f:
        f.write(str(os.getpgrp()))
    raise RuntimeError("the factory failed on purpose")


def make_slow():
    """A factory that starts a program, writes its process group to `OUTRIG_FIXTURE_PGID_FILE`,
    and then takes longer than any owner waits."""
    subprocess.Popen(["sleep", "1000"])
    with open(os.environ["OUTRIG_FIXTURE_PGID_FILE"], "w") as f:
        f.write(str(os.getpgrp()))
    time.sleep(1000)
    return Root()
"#;

/// The service process of the two-session measurement: the fixture's `Records` behind a Unix
/// socket, a thread per connection, each exchange a `(method, args, kwargs)` tuple answered with
/// `("ok", result)` or `("error", text)`. Started as `python3 -I -c <this> <socket> <fixture-dir>`,
/// and says `ready` on stdout once it listens.
pub(crate) const SERVICE_SOURCE: &str = r#"
import sys, threading
from multiprocessing.connection import Listener

address, fixture_dir = sys.argv[1], sys.argv[2]
sys.path.append(fixture_dir)
from outrig_fixture import Records

state = Records()
listener = Listener(address, family="AF_UNIX", authkey=b"fixture")
print("ready", flush=True)


def serve(conn):
    with conn:
        while True:
            try:
                method, args, kwargs = conn.recv()
            except EOFError:
                return
            try:
                result = ("ok", getattr(state, method)(*args, **kwargs))
            except Exception as e:
                result = ("error", f"{type(e).__name__}: {e}")
            conn.send(result)


while True:
    threading.Thread(target=serve, args=(listener.accept(),), daemon=True).start()
"#;

/// The service process, started on the host and killed with the test.
pub(crate) struct Service {
    child: Child,
    _dir: tempfile::TempDir,
    /// The Unix socket it listens on, for `OUTRIG_FIXTURE_SERVICE`.
    pub(crate) socket: PathBuf,
}

impl Service {
    pub(crate) fn start() -> Self {
        let dir = tempfile::Builder::new()
            .prefix("outrig-svc-")
            .tempdir_in(std::env::temp_dir())
            .expect("a directory for the socket");
        let socket = dir.path().join("s");
        let mut child = python_command(None)
            .args(["-I", "-c", SERVICE_SOURCE])
            .arg(&socket)
            .arg(fixture_dir())
            .spawn()
            .expect("the service starts");
        let stdout = child.stdout.take().expect("stdout is piped");
        let stderr = capture(child.stderr.take().expect("stderr is piped"));
        let (ready_tx, ready_rx) = channel();
        std::thread::spawn(move || {
            let first = BufReader::new(stdout).lines().next();
            let _ = ready_tx.send(first.map(|line| line.unwrap_or_default()));
        });
        match ready_rx.recv_timeout(TIMEOUT) {
            Ok(Some(line)) if line == "ready" => {}
            other => panic!(
                "the service did not greet: {other:?}; its stderr: {}",
                stderr.lock().expect("stderr lock")
            ),
        }
        Self {
            child,
            _dir: dir,
            socket,
        }
    }

    pub(crate) fn socket(&self) -> &str {
        self.socket.to_str().expect("a UTF-8 socket path")
    }
}

impl Drop for Service {
    fn drop(&mut self) {
        if let Ok(pid) = i32::try_from(self.child.id()) {
            let _ = killpg(Pid::from_raw(pid), Signal::SIGKILL);
        }
        let _ = self.child.wait();
    }
}

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
/// many of those toward a binding were close notices -- overall, and per agent and binding.
#[derive(Default)]
pub(crate) struct Stats {
    pub(crate) to_binding: Counter,
    pub(crate) to_interpreter: Counter,
    closed_to_binding: AtomicUsize,
    per: Mutex<HashMap<(String, String), Arc<Traffic>>>,
}

impl Stats {
    pub(crate) fn closed_to_binding(&self) -> usize {
        self.closed_to_binding.load(Ordering::SeqCst)
    }

    /// The traffic between `agent` and `binding`, empty while none has crossed.
    pub(crate) fn between(&self, agent: &str, binding: &str) -> Arc<Traffic> {
        let mut per = self.per.lock().expect("stats lock");
        Arc::clone(
            per.entry((agent.to_string(), binding.to_string()))
                .or_default(),
        )
    }
}

/// One agent's `rpc` traffic with one binding, and the connection ids it has used.
#[derive(Default)]
pub(crate) struct Traffic {
    pub(crate) to_binding: Counter,
    pub(crate) to_interpreter: Counter,
    closed_to_binding: AtomicUsize,
    ids: Mutex<BTreeSet<u64>>,
}

impl Traffic {
    pub(crate) fn closed_to_binding(&self) -> usize {
        self.closed_to_binding.load(Ordering::SeqCst)
    }

    /// Every connection id a line has named, either way.
    pub(crate) fn connection_ids(&self) -> BTreeSet<u64> {
        self.ids.lock().expect("ids lock").clone()
    }

    fn record(&self, len: usize, id: Option<u64>, to_binding: bool, closed: bool) {
        if to_binding {
            self.to_binding.record(len);
            if closed {
                self.closed_to_binding.fetch_add(1, Ordering::SeqCst);
            }
        } else {
            self.to_interpreter.record(len);
        }
        if let Some(id) = id {
            self.ids.lock().expect("ids lock").insert(id);
        }
    }
}

/// How [`Relay::bind_opts`] starts a binding process.
#[derive(Default)]
pub(crate) struct Bind<'a> {
    /// The factory, as `module:callable`, in the fixture package.
    pub(crate) factory: &'a str,
    /// Extra environment variables for the process, on top of this process's own. With none,
    /// the binding inherits the environment, as one under the CLI does.
    pub(crate) env: &'a [(&'a str, &'a str)],
    /// Exactly the process's environment, as an embedder gives one; in place of `env`.
    pub(crate) exact_env: Option<&'a [(&'a str, &'a str)]>,
    /// `--serialize`: one call at a time across the binding's connections.
    pub(crate) serialize: bool,
    /// The package directory, the fixture's when `None`.
    pub(crate) packages: Option<&'a Path>,
    /// The process's working directory.
    pub(crate) cwd: Option<&'a Path>,
}

/// A binding the relay started, with the lines of it that only the test reads.
struct Bound {
    binding: Binding,
    events: mpsc::Receiver<Value>,
    decisions: mpsc::Receiver<Decision>,
}

pub(crate) struct Relay {
    interpreter: Child,
    stdin: Arc<Mutex<ChildStdin>>,
    other: Receiver<Result<Value, String>>,
    /// Messages read while looking for another, kept in order for the next `recv`.
    pending: VecDeque<Value>,
    stderr: Arc<Mutex<String>>,
    /// Each binding's stdin, for the pump that forwards the interpreter's `rpc` lines.
    targets: Arc<Mutex<HashMap<String, mpsc::Sender<Value>>>>,
    bindings: HashMap<String, Bound>,
    /// Where the bindings live: their pipes and their reaping are registered with it.
    runtime: Runtime,
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
        let targets = Arc::new(Mutex::new(HashMap::new()));
        let stats = Arc::new(Stats::default());
        let (tx, other) = channel();
        {
            let (stdin, targets, stats) =
                (Arc::clone(&stdin), Arc::clone(&targets), Arc::clone(&stats));
            std::thread::spawn(move || pump_interpreter(stdout, tx, stdin, targets, stats));
        }
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("a runtime for the bindings");
        let mut relay = Self {
            interpreter: child,
            stdin,
            other,
            pending: VecDeque::new(),
            stderr,
            targets,
            bindings: HashMap::new(),
            runtime,
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
        self.bind_opts(
            name,
            &Bind {
                factory,
                env,
                ..Bind::default()
            },
        );
    }

    /// [`Relay::bind`] with `--serialize`: the binding runs one call at a time.
    pub(crate) fn bind_serialized(&mut self, name: &str, factory: &str) {
        self.bind_opts(
            name,
            &Bind {
                factory,
                serialize: true,
                ..Bind::default()
            },
        );
    }

    /// Start binding `name`'s process as `bind` says, through the supervisor, and wait for it to
    /// be ready.
    pub(crate) fn bind_opts(&mut self, name: &str, bind: &Bind) {
        let owned = |pairs: &[(&str, &str)]| -> Vec<(OsString, OsString)> {
            pairs
                .iter()
                .map(|(k, v)| (OsString::from(k), OsString::from(v)))
                .collect()
        };
        let exact = match bind.exact_env {
            Some(given) => Some(owned(given)),
            None if bind.env.is_empty() => None,
            None => Some(std::env::vars_os().chain(owned(bind.env)).collect()),
        };
        let spec = Spec {
            python: python(),
            rpyc_dir: rpyc_dir(),
            packages: match bind.packages {
                Some(packages) => packages,
                None => fixture_dir(),
            },
            factory: bind.factory,
            serialize: bind.serialize,
            cwd: bind.cwd,
            env: exact.as_deref(),
            probe: None,
        };
        let mut binding = self
            .runtime
            .block_on(supervisor::start(spec))
            .unwrap_or_else(|e| panic!("binding {name:?} did not start: {e}"));
        let rpc = binding.take_rpc();
        let events = binding.take_events();
        let decisions = binding.take_decisions();
        self.targets
            .lock()
            .expect("targets lock")
            .insert(name.to_string(), binding.sender());
        {
            let (name, stdin, stats) = (
                name.to_string(),
                Arc::clone(&self.stdin),
                Arc::clone(&self.stats),
            );
            std::thread::spawn(move || pump_binding(name, rpc, stdin, stats));
        }
        self.bindings.insert(
            name.to_string(),
            Bound {
                binding,
                events,
                decisions,
            },
        );
    }

    /// Write `message` as one line to the interpreter's stdin, as the host would.
    pub(crate) fn send(&mut self, message: Value) {
        write_line(&self.stdin, &message);
    }

    /// Write one raw line to binding `name`'s stdin, as the host would.
    pub(crate) fn inject_to_binding(&mut self, name: &str, message: Value) {
        let sender = self
            .targets
            .lock()
            .expect("targets lock")
            .get(name)
            .cloned()
            .expect("a started binding");
        let _ = sender.blocking_send(message);
    }

    /// The next event line binding `name` writes, within `within`.
    pub(crate) fn event(&mut self, name: &str, within: Duration) -> Value {
        let bound = self.bindings.get_mut(name).expect("a started binding");
        self.runtime
            .block_on(async { tokio::time::timeout(within, bound.events.recv()).await })
            .unwrap_or_else(|_| panic!("no event from binding {name:?} within {within:?}"))
            .expect("the binding's event lines")
    }

    /// The next decision request binding `name` writes, within `within`.
    pub(crate) fn decision(&mut self, name: &str, within: Duration) -> Decision {
        let bound = self.bindings.get_mut(name).expect("a started binding");
        self.runtime
            .block_on(async { tokio::time::timeout(within, bound.decisions.recv()).await })
            .unwrap_or_else(|_| panic!("no decision request from {name:?} within {within:?}"))
            .expect("the binding's decision requests")
    }

    /// Answer binding `name`'s decision `id`.
    pub(crate) fn answer(&mut self, name: &str, id: u64, answer: Value) {
        let bound = self.bindings.get(name).expect("a started binding");
        self.runtime
            .block_on(bound.binding.answer(id, answer))
            .expect("the binding takes the answer");
    }

    /// Stop binding `name` as a session's shutdown would, with `grace` between the signals.
    pub(crate) fn stop_binding(&mut self, name: &str, grace: Duration) -> Stopped {
        self.targets.lock().expect("targets lock").remove(name);
        let bound = self.bindings.remove(name).expect("a started binding");
        self.runtime.block_on(bound.binding.stop(grace))
    }

    /// Close binding `name`'s stdin, as the owner's death does.
    pub(crate) fn close_binding_stdin(&mut self, name: &str) {
        self.targets.lock().expect("targets lock").remove(name);
        self.bindings
            .get_mut(name)
            .expect("a started binding")
            .binding
            .close_stdin();
    }

    /// The next message from the interpreter that is not an `rpc` line: one held back by
    /// [`Relay::recv_where`] first, else the next to arrive within [`TIMEOUT`].
    pub(crate) fn recv(&mut self) -> Value {
        self.recv_within(TIMEOUT)
    }

    /// [`Relay::recv`], waiting up to `within` for a fresh message.
    pub(crate) fn recv_within(&mut self, within: Duration) -> Value {
        match self.pending.pop_front() {
            Some(message) => message,
            None => self.recv_fresh(within),
        }
    }

    fn recv_fresh(&mut self, within: Duration) -> Value {
        let message = match self.other.recv_timeout(within) {
            Ok(Ok(message)) => message,
            Ok(Err(bad)) => panic!("{bad}"),
            Err(RecvTimeoutError::Timeout) => {
                panic!("quiet for {within:?}; stderr: {}", self.stderr())
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

    /// The next message `matches`, within `within`; the messages read meanwhile that do not are
    /// held back, in order, for later `recv`s. How a test reads answers from several kernels
    /// that arrive in whatever order they finish.
    pub(crate) fn recv_where(
        &mut self,
        within: Duration,
        matches: impl Fn(&Value) -> bool,
    ) -> Value {
        self.try_recv_where(within, matches).unwrap_or_else(|| {
            panic!(
                "no matching message within {within:?}; held back: {:?}; stderr: {}",
                self.pending,
                self.stderr()
            )
        })
    }

    /// [`Relay::recv_where`], answering `None` rather than failing when `within` passes.
    pub(crate) fn try_recv_where(
        &mut self,
        within: Duration,
        matches: impl Fn(&Value) -> bool,
    ) -> Option<Value> {
        if let Some(index) = self.pending.iter().position(&matches) {
            return Some(self.pending.remove(index).expect("an index in range"));
        }
        let deadline = Instant::now() + within;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return None;
            }
            let message = match self.other.recv_timeout(left) {
                Ok(Ok(message)) => message,
                Ok(Err(bad)) => panic!("{bad}"),
                Err(RecvTimeoutError::Timeout) => return None,
                Err(RecvTimeoutError::Disconnected) => {
                    panic!("the interpreter exited; stderr: {}", self.stderr())
                }
            };
            if matches(&message) {
                return Some(message);
            }
            self.pending.push_back(message);
        }
    }

    /// Submit `source` to `agent` without waiting for its result.
    pub(crate) fn submit_in(&mut self, agent: &str, id: u64, source: &str) {
        self.send(json!({"t": "exec", "agent": agent, "id": id, "src": source}));
    }

    /// The result of `agent`'s execution `id`, within `within`, whatever else arrives first.
    pub(crate) fn result_from(&mut self, agent: &str, id: u64, within: Duration) -> Value {
        self.recv_where(within, |m| {
            m["t"] == "result" && m["agent"] == agent && m["id"] == id
        })
    }

    /// [`Relay::result_from`], answering `None` rather than failing when `within` passes.
    pub(crate) fn try_result_from(
        &mut self,
        agent: &str,
        id: u64,
        within: Duration,
    ) -> Option<Value> {
        self.try_recv_where(within, |m| {
            m["t"] == "result" && m["agent"] == agent && m["id"] == id
        })
    }

    /// Submit `source` to `agent` and return its result.
    pub(crate) fn exec_in(&mut self, agent: &str, id: u64, source: &str) -> Value {
        self.exec_in_within(agent, id, source, TIMEOUT)
    }

    /// [`Relay::exec_in`], waiting up to `within` for the result.
    pub(crate) fn exec_in_within(
        &mut self,
        agent: &str,
        id: u64,
        source: &str,
        within: Duration,
    ) -> Value {
        self.submit_in(agent, id, source);
        self.result_from(agent, id, within)
    }

    pub(crate) fn exec(&mut self, id: u64, source: &str) -> Value {
        self.exec_in(PRIMARY, id, source)
    }

    /// Run `source` in `agent`, asserting it did not raise, and return what it printed.
    pub(crate) fn output_in(&mut self, agent: &str, id: u64, source: &str) -> String {
        self.output_in_within(agent, id, source, TIMEOUT)
    }

    /// [`Relay::output_in`], waiting up to `within` for the result.
    pub(crate) fn output_in_within(
        &mut self,
        agent: &str,
        id: u64,
        source: &str,
        within: Duration,
    ) -> String {
        let result = self.exec_in_within(agent, id, source, within);
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

    pub(crate) fn interrupt(&mut self, agent: &str, id: u64, runaway: bool) {
        self.send(json!({"t": "interrupt", "agent": agent, "id": id, "runaway": runaway}));
    }

    pub(crate) fn cancel(&mut self, agent: &str, id: u64) {
        self.send(json!({"t": "cancel", "agent": agent, "id": id}));
    }

    /// Ask `agent` for its inventory without waiting; the answer comes from its loop.
    pub(crate) fn ask_inv(&mut self, agent: &str, id: u64) {
        self.send(json!({"t": "inv", "agent": agent, "id": id}));
    }

    /// The answer to [`Relay::ask_inv`], whatever else arrives first.
    pub(crate) fn inv_answer(&mut self, agent: &str, id: u64, within: Duration) -> Value {
        self.recv_where(within, |m| {
            m["t"] == "inv" && m["agent"] == agent && m["id"] == id
        })
    }

    /// An inventory round trip, which proves `agent`'s loop turns.
    pub(crate) fn inv(&mut self, agent: &str, id: u64) -> Value {
        self.ask_inv(agent, id);
        self.inv_answer(agent, id, TIMEOUT)
    }

    /// The CPU time `agent`'s thread has used, answered on the reader thread.
    pub(crate) fn cpu(&mut self, agent: &str, id: u64) -> Value {
        self.send(json!({"t": "cpu", "agent": agent, "id": id}));
        self.recv_where(TIMEOUT, |m| {
            m["t"] == "cpu" && m["agent"] == agent && m["id"] == id
        })
    }

    /// Post `body` on `agent`'s channel `channel`, answered on the reader thread with how many
    /// messages then wait there.
    pub(crate) fn msg(&mut self, agent: &str, id: u64, channel: &str, body: &str) -> Value {
        self.send(json!({"t": "msg", "agent": agent, "id": id, "channel": channel, "body": body}));
        self.recv_where(TIMEOUT, |m| {
            m["t"] == "msg" && m["agent"] == agent && m["id"] == id
        })
    }

    pub(crate) fn binding_pid(&self, name: &str) -> u32 {
        let bound = self.bindings.get(name).expect("a started binding");
        bound.binding.pid().as_raw() as u32
    }

    /// The binding process's resident set, in KiB, as `/proc` reports it.
    pub(crate) fn binding_rss_kib(&self, name: &str) -> usize {
        let status = std::fs::read_to_string(format!("/proc/{}/status", self.binding_pid(name)))
            .expect("the binding's /proc status");
        status
            .lines()
            .find_map(|line| line.strip_prefix("VmRSS:"))
            .and_then(|rest| rest.split_whitespace().next())
            .and_then(|kib| kib.parse().ok())
            .expect("a VmRSS line")
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

    /// The last of every binding's stderr, labeled.
    pub(crate) fn binding_stderr(&self) -> String {
        let mut text = String::new();
        for (name, bound) in &self.bindings {
            let said = bound.binding.stderr();
            if !said.is_empty() {
                text.push_str(&format!("[{name}] {said}\n"));
            }
        }
        text
    }
}

impl Drop for Relay {
    fn drop(&mut self) {
        // The bindings first, stopped on the runtime that reaps them; the runtime itself goes
        // last, with the struct, so nothing is left unreaped.
        self.targets.lock().expect("targets lock").clear();
        for (_, bound) in self.bindings.drain() {
            let _ = self
                .runtime
                .block_on(bound.binding.stop(Duration::from_millis(200)));
        }
        if let Ok(pid) = i32::try_from(self.interpreter.id()) {
            let _ = killpg(Pid::from_raw(pid), Signal::SIGKILL);
        }
        let _ = self.interpreter.wait();
    }
}

/// Route the interpreter's lines: `rpc` lines to the binding they name, the rest to the test.
fn pump_interpreter(
    stdout: impl Read,
    tx: Sender<Result<Value, String>>,
    stdin: Arc<Mutex<ChildStdin>>,
    targets: Arc<Mutex<HashMap<String, mpsc::Sender<Value>>>>,
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
        let closed = message.get("closed").is_some();
        if closed {
            stats.closed_to_binding.fetch_add(1, Ordering::SeqCst);
        }
        let binding = message
            .as_object_mut()
            .expect("an object")
            .remove("binding")
            .unwrap_or(Value::Null);
        if let (Some(agent), Some(name)) = (message["agent"].as_str(), binding.as_str()) {
            stats
                .between(agent, name)
                .record(line.len(), message["id"].as_u64(), true, closed);
        }
        let target = binding
            .as_str()
            .and_then(|name| targets.lock().expect("targets lock").get(name).cloned());
        match target {
            Some(target) => {
                let _ = target.blocking_send(message);
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

/// Forward a binding's `rpc` lines, as the supervisor hands them over, to the interpreter with
/// the binding's name added.
fn pump_binding(
    name: String,
    mut rpc: mpsc::Receiver<Value>,
    interpreter: Arc<Mutex<ChildStdin>>,
    stats: Arc<Stats>,
) {
    while let Some(mut message) = rpc.blocking_recv() {
        message["binding"] = json!(name);
        let len = write_line(&interpreter, &message);
        stats.to_interpreter.record(len);
        if let Some(agent) = message["agent"].as_str() {
            stats
                .between(agent, &name)
                .record(len, message["id"].as_u64(), false, false);
        }
    }
}
