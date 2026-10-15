//! A test harness around the relay in `hosted.rs`: one interpreter and the process holding its
//! session's bindings, both started on the host, with the session's relay between them.
//!
//! The interpreter is started as `interpreter_tests.rs` starts it, with the vendored RPyC's
//! directory as its second argument; the bindings process is started through `supervisor.rs`, as
//! a session starts it, over the fixture package with a factory per binding. The interpreter's
//! `rpc` lines and notices go to the session's relay, as `host.rs` hands them to it, and what the
//! relay sends the interpreter is written to its stdin; everything else the interpreter says is
//! handed to the test, as are the process's decision requests and the event lines the relay does
//! not know. The relay's events are recorded on a stream the test reads. The harness counts the
//! `rpc` lines each way and remembers the longest, which is how a test checks the frame bound
//! held in transit.

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

use super::host::{PRIMARY, bindings_message};
use super::hosted::{self, Notice, Rpc};
use super::payload;
use super::supervisor::{self, Decision, Factory, Process, Spec, Stopped};
use super::testing::{Start, capture, host_home, interpreter_command, python, python_command};
use crate::events::{Events, StreamBuilder};
use crate::harness::bindings::Brief;
use crate::harness::event::{Event, Received, Subscription, TryRecvError};
use crate::harness::{Closed, ClosedBy};

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

/// The fixture library, `outrig_fixture.py`, as a module: an object with public and private
/// members, a nested object, a sequence that counts what iterating it asks for, a context
/// manager, methods that call back, report types, and return a large result, two exception
/// classes, a counter around `pickle.loads`, a recording method that notes when it ran on the
/// host and sleeps or waits for a file, objects made on request with a weak reference kept, so a
/// test can watch the binding's table drop them, and a call whose forked helper leaves the
/// process's group holding its output.
pub(crate) const FIXTURE_SOURCE: &str = include_str!("outrig_fixture.py");

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

/// How [`Relay::bind_opts`] declares a binding, and what it changes about the process that
/// holds them all.
#[derive(Default)]
pub(crate) struct Bind<'a> {
    /// The factory, as `module:callable`, in the fixture package.
    pub(crate) factory: &'a str,
    /// Extra environment variables for the process, on top of this process's own. With none,
    /// the process inherits the environment, as one under the CLI does.
    pub(crate) env: &'a [(&'a str, &'a str)],
    /// Exactly the process's environment, as an embedder gives one; in place of `env`.
    pub(crate) exact_env: Option<&'a [(&'a str, &'a str)]>,
    /// One call at a time across the binding's connections.
    pub(crate) serialize: bool,
    /// The binding's package directory, the fixture's when `None`.
    pub(crate) packages: Option<&'a Path>,
    /// The process's working directory.
    pub(crate) cwd: Option<&'a Path>,
}

/// The bindings process the harness started, the session's relay to it, and the lines of it that
/// only the test reads.
struct Bound {
    process: Process,
    relay: hosted::Relay,
    /// A sender into the process's stdin, `None` once a test has closed it.
    stdin: Option<mpsc::Sender<Value>>,
    /// Event lines the relay does not know.
    events: mpsc::UnboundedReceiver<Value>,
    decisions: mpsc::Receiver<Decision>,
}

pub(crate) struct Relay {
    interpreter: Child,
    /// The interpreter's stdin, `None` once a test has closed it.
    stdin: Arc<Mutex<Option<ChildStdin>>>,
    /// What the session's relay sends the interpreter, written by a thread of its own.
    to_interpreter: hosted::ToInterpreter,
    other: Receiver<Result<Value, String>>,
    /// Messages read while looking for another, kept in order for the next `recv`.
    pending: VecDeque<Value>,
    stderr: Arc<Mutex<String>>,
    /// The session's relay to the running bindings process, for the pump that hands it the
    /// interpreter's lines.
    hosted: Arc<Mutex<Option<hosted::Relay>>>,
    /// The agents opened besides the primary, which every new relay is told of.
    agents: Vec<String>,
    /// The bindings declared so far, in order, each with its package directory, and how their
    /// process starts.
    declared: Vec<(Factory, PathBuf)>,
    env: Option<Vec<(OsString, OsString)>>,
    cwd: Option<PathBuf>,
    process: Option<Bound>,
    /// The stream the relays record to, and the test's subscription to it.
    events: Events,
    subscription: Subscription,
    /// Every event taken from the subscription so far, and how many it missed.
    seen: Vec<Arc<Event>>,
    missed: u64,
    /// Where the bindings process and its relay live: their pipes and tasks are registered with
    /// it.
    runtime: Runtime,
    pub(crate) stats: Arc<Stats>,
}

/// Write `message` as one line, returning its length; a write to a process that has exited, or
/// whose stdin a test has closed, is dropped, since the test learns of that another way.
fn write_line(stdin: &Mutex<Option<ChildStdin>>, message: &Value) -> usize {
    let line = format!("{message}\n");
    if let Some(stdin) = stdin.lock().expect("stdin lock").as_mut() {
        let _ = stdin
            .write_all(line.as_bytes())
            .and_then(|()| stdin.flush());
    }
    line.len()
}

/// Events the test's subscription holds before the oldest is dropped, unless a test asks for
/// fewer.
const EVENTS_HELD: usize = 4096;

impl Relay {
    pub(crate) fn start() -> Self {
        Self::start_with(&Start::default())
    }

    /// [`Relay::start`], the test's subscription holding `capacity` events.
    pub(crate) fn start_with_capacity(capacity: usize) -> Self {
        Self::start_holding(&Start::default(), capacity)
    }

    pub(crate) fn start_with(start: &Start) -> Self {
        Self::start_holding(start, EVENTS_HELD)
    }

    fn start_holding(start: &Start, capacity: usize) -> Self {
        let mut child = interpreter_command(start)
            .arg(rpyc_dir())
            .spawn()
            .expect("the interpreter starts");
        let stdin = Arc::new(Mutex::new(Some(
            child.stdin.take().expect("stdin is piped"),
        )));
        let stdout = child.stdout.take().expect("stdout is piped");
        let stderr = capture(child.stderr.take().expect("stderr is piped"));
        let hosted = Arc::new(Mutex::new(None));
        let stats = Arc::new(Stats::default());
        let (tx, other) = channel();
        {
            let (stdin, hosted, stats) =
                (Arc::clone(&stdin), Arc::clone(&hosted), Arc::clone(&stats));
            std::thread::spawn(move || pump_interpreter(stdout, tx, stdin, hosted, stats));
        }
        let to_interpreter = {
            let (lines, queued) = channel::<Value>();
            let (stdin, stats) = (Arc::clone(&stdin), Arc::clone(&stats));
            std::thread::spawn(move || write_from_relay(queued, stdin, stats));
            let lines = Mutex::new(lines);
            Arc::new(move |line: Value| {
                let _ = lines.lock().expect("lines lock").send(line);
            }) as hosted::ToInterpreter
        };
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("a runtime for the bindings");
        let mut stream = StreamBuilder::default();
        let subscription = stream.subscribe(capacity);
        let mut relay = Self {
            interpreter: child,
            stdin,
            to_interpreter,
            other,
            pending: VecDeque::new(),
            stderr,
            hosted,
            agents: Vec::new(),
            declared: Vec::new(),
            env: None,
            cwd: None,
            process: None,
            events: stream.build(),
            subscription,
            seen: Vec::new(),
            missed: 0,
            runtime,
            stats,
        };
        let ready = relay.recv();
        assert_eq!(ready["t"], "ready", "{ready}");
        assert_eq!(ready["agent"], PRIMARY, "{ready}");
        relay
    }

    /// Declare binding `name` over the fixture, with `factory` as `module:callable`, and start
    /// the bindings process again with it, waiting for it to be ready.
    pub(crate) fn bind(&mut self, name: &str, factory: &str) {
        self.bind_with(name, factory, &[]);
    }

    /// [`Relay::bind`], adding `env` to the process's environment.
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

    /// [`Relay::bind`], the binding running one call at a time.
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

    /// Declare binding `name` as `bind` says, and start the bindings process again, through the
    /// supervisor, with every binding declared so far, waiting for it to be ready. The process
    /// is stopped first if it runs: a test declares its bindings before it uses any.
    pub(crate) fn bind_opts(&mut self, name: &str, bind: &Bind) {
        let owned = |pairs: &[(&str, &str)]| -> Vec<(OsString, OsString)> {
            pairs
                .iter()
                .map(|(k, v)| (OsString::from(k), OsString::from(v)))
                .collect()
        };
        match bind.exact_env {
            Some(given) => self.env = Some(owned(given)),
            None if bind.env.is_empty() => {}
            None => self.env = Some(std::env::vars_os().chain(owned(bind.env)).collect()),
        }
        if let Some(cwd) = bind.cwd {
            self.cwd = Some(cwd.to_path_buf());
        }
        let factory = Factory {
            name: name.to_string(),
            factory: bind.factory.to_string(),
            args: json!([]),
            kwargs: json!({}),
            serialize: bind.serialize,
        };
        self.bind_factory(factory, bind.packages.unwrap_or_else(|| fixture_dir()));
    }

    /// Declare the binding `factory` describes, its package in `packages` -- one a session's own
    /// lowering of a declaration made, say -- and start the bindings process again with every
    /// binding declared so far, waiting for it to be ready, with a new session relay to it.
    pub(crate) fn bind_factory(&mut self, factory: Factory, packages: &Path) {
        self.declared.push((factory, packages.to_path_buf()));
        self.stop_process(Duration::from_millis(200));
        let mut paths: Vec<&Path> = Vec::new();
        for (_, dir) in &self.declared {
            if !paths.contains(&dir.as_path()) {
                paths.push(dir);
            }
        }
        let spec = Spec {
            python: python(),
            rpyc_dir: rpyc_dir(),
            paths,
            bindings: self.declared.iter().map(|(f, _)| f.clone()).collect(),
            cwd: self.cwd.as_deref(),
            env: self.env.as_deref(),
            probe: None,
        };
        let mut process = self
            .runtime
            .block_on(supervisor::start(spec))
            .unwrap_or_else(|e| panic!("the bindings did not start: {e}"));
        let names = self.declared.iter().map(|(f, _)| f.name.clone()).collect();
        let decisions = process.take_decisions();
        let relay =
            hosted::Relay::new(Arc::clone(&self.to_interpreter), self.events.clone(), names);
        for agent in &self.agents {
            relay.opened(agent);
        }
        let (others, events) = mpsc::unbounded_channel();
        relay.others(others);
        {
            let _entered = self.runtime.enter();
            relay.attach(&mut process);
        }
        *self.hosted.lock().expect("hosted lock") = Some(relay.clone());
        self.process = Some(Bound {
            stdin: Some(process.sender()),
            process,
            relay,
            events,
            decisions,
        });
    }

    /// Write `message` as one line to the interpreter's stdin, as the host would.
    pub(crate) fn send(&mut self, message: Value) {
        write_line(&self.stdin, &message);
    }

    /// Write one raw line to the bindings process's stdin, past the relay.
    pub(crate) fn inject_to_binding(&mut self, message: Value) {
        let bound = self.process.as_ref().expect("a started bindings process");
        let sender = bound.stdin.clone().expect("the process's stdin is open");
        let _ = sender.blocking_send(message);
    }

    /// The session's relay to the running bindings process.
    fn relay(&self) -> &hosted::Relay {
        &self
            .process
            .as_ref()
            .expect("a started bindings process")
            .relay
    }

    /// Close admission at the relay, as the session's close does: with the error a session's
    /// owner's close gives.
    pub(crate) fn close_admission(&mut self) {
        self.relay()
            .close(&Closed::from(ClosedBy::Owner).to_string());
    }

    /// How many request frames the relay has forwarded to the process.
    pub(crate) fn forwarded(&self) -> u64 {
        self.relay().forwarded()
    }

    /// Tell the interpreter the declared bindings, as a session does once it has started them:
    /// every kernel then holds a stub for each, under its name.
    pub(crate) fn declare_bindings(&mut self) {
        let bound = self.process.as_ref().expect("a started bindings process");
        let briefs: Vec<Brief> = self
            .declared
            .iter()
            .map(|(factory, _)| Brief {
                name: factory.name.clone(),
                description: format!("the fixture, as {}", factory.factory),
                type_name: bound.process.type_name(&factory.name).to_string(),
                serialize: factory.serialize,
            })
            .collect();
        let mut paths: Vec<String> = Vec::new();
        for (_, dir) in &self.declared {
            let path = dir.to_string_lossy().into_owned();
            if !paths.contains(&path) {
                paths.push(path);
            }
        }
        self.send(bindings_message(&briefs, paths));
    }

    /// Take what the subscription holds into `seen`.
    fn take_events(&mut self) {
        loop {
            match self.subscription.try_recv() {
                Ok(Received::Event(event)) => self.seen.push(event),
                Ok(Received::Missed(missed)) => self.missed += missed,
                Err(TryRecvError::Empty | TryRecvError::Closed) => return,
            }
        }
    }

    /// Every event the relays have recorded, once no request the process was sent is still to
    /// end.
    pub(crate) fn hosted_events(&mut self) -> Vec<Arc<Event>> {
        if let Some(bound) = &self.process {
            let drained = bound.relay.drained();
            self.runtime
                .block_on(async { tokio::time::timeout(TIMEOUT, drained).await })
                .unwrap_or_else(|_| panic!("hosted requests still running after {TIMEOUT:?}"));
        }
        self.take_events();
        self.seen.clone()
    }

    /// Every event the relays have recorded, once `done` holds of them, within `within`.
    pub(crate) fn hosted_events_until(
        &mut self,
        within: Duration,
        done: impl Fn(&[Arc<Event>]) -> bool,
    ) -> Vec<Arc<Event>> {
        self.hosted_events_with_gaps(within, done).1
    }

    /// [`Relay::hosted_events_until`], with how many events the subscription missed.
    pub(crate) fn hosted_events_with_gaps(
        &mut self,
        within: Duration,
        done: impl Fn(&[Arc<Event>]) -> bool,
    ) -> (u64, Vec<Arc<Event>>) {
        let deadline = Instant::now() + within;
        loop {
            self.take_events();
            if done(&self.seen) {
                return (self.missed, self.seen.clone());
            }
            assert!(
                Instant::now() < deadline,
                "the events did not come within {within:?}: {:?}",
                self.seen
                    .iter()
                    .map(|event| &event.payload)
                    .collect::<Vec<_>>()
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// The next event line the bindings process writes, within `within`.
    pub(crate) fn event(&mut self, within: Duration) -> Value {
        let bound = self.process.as_mut().expect("a started bindings process");
        self.runtime
            .block_on(async { tokio::time::timeout(within, bound.events.recv()).await })
            .unwrap_or_else(|_| panic!("no event from the bindings within {within:?}"))
            .expect("the process's event lines")
    }

    /// The next decision request the bindings process writes, within `within`.
    pub(crate) fn decision(&mut self, within: Duration) -> Decision {
        let bound = self.process.as_mut().expect("a started bindings process");
        self.runtime
            .block_on(async { tokio::time::timeout(within, bound.decisions.recv()).await })
            .unwrap_or_else(|_| panic!("no decision request within {within:?}"))
            .expect("the process's decision requests")
    }

    /// Answer the bindings process's decision `id`.
    pub(crate) fn answer(&mut self, id: u64, answer: Value) {
        let bound = self.process.as_ref().expect("a started bindings process");
        self.runtime
            .block_on(bound.process.answer(id, answer))
            .expect("the process takes the answer");
    }

    /// Stop the bindings process as a session's shutdown would, with `grace` between the
    /// signals, and wait for its relay to handle the last of its lines; `None` when none runs.
    pub(crate) fn stop_process(&mut self, grace: Duration) -> Option<Stopped> {
        self.hosted.lock().expect("hosted lock").take();
        let bound = self.process.take()?;
        bound.relay.stopping();
        let finished = bound.relay.finished();
        Some(self.runtime.block_on(async {
            let stopped = bound.process.stop(grace).await;
            let _ = tokio::time::timeout(TIMEOUT, finished).await;
            stopped
        }))
    }

    /// Close the bindings process's stdin, as the owner's death does.
    pub(crate) fn close_binding_stdin(&mut self) {
        let bound = self.process.as_mut().expect("a started bindings process");
        bound.relay.hang_up();
        bound.stdin = None;
        bound.process.close_stdin();
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

    /// The bindings process's pid, which is also its process group's id.
    pub(crate) fn binding_pid(&self) -> u32 {
        let bound = self.process.as_ref().expect("a started bindings process");
        bound.process.pid().as_raw() as u32
    }

    /// The bindings process's resident set, in KiB, as `/proc` reports it.
    pub(crate) fn binding_rss_kib(&self) -> usize {
        let status = std::fs::read_to_string(format!("/proc/{}/status", self.binding_pid()))
            .expect("the process's /proc status");
        status
            .lines()
            .find_map(|line| line.strip_prefix("VmRSS:"))
            .and_then(|rest| rest.split_whitespace().next())
            .and_then(|kib| kib.parse().ok())
            .expect("a VmRSS line")
    }

    /// Open a kernel for `agent` and wait for its greeting.
    pub(crate) fn open(&mut self, agent: &str) {
        self.agents.push(agent.to_string());
        if let Some(bound) = &self.process {
            bound.relay.opened(agent);
        }
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

    /// The last of the bindings process's stderr.
    pub(crate) fn binding_stderr(&self) -> String {
        self.process
            .as_ref()
            .map(|bound| bound.process.stderr())
            .unwrap_or_default()
    }
}

impl Drop for Relay {
    fn drop(&mut self) {
        // The bindings first, stopped on the runtime that reaps them; the runtime itself goes
        // last, with the struct, so nothing is left unreaped.
        self.stop_process(Duration::from_millis(200));
        if let Ok(pid) = i32::try_from(self.interpreter.id()) {
            let _ = killpg(Pid::from_raw(pid), Signal::SIGKILL);
        }
        let _ = self.interpreter.wait();
    }
}

/// Route the interpreter's lines: `rpc` lines and notices to the session's relay, the rest to the
/// test.
fn pump_interpreter(
    stdout: impl Read,
    tx: Sender<Result<Value, String>>,
    stdin: Arc<Mutex<Option<ChildStdin>>>,
    hosted: Arc<Mutex<Option<hosted::Relay>>>,
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
        let message = match serde_json::from_str::<Value>(&line) {
            Ok(message) if message.is_object() => message,
            _ => {
                let _ = tx.send(Err(format!("not a protocol message: {line:?}")));
                continue;
            }
        };
        let relay = hosted.lock().expect("hosted lock").clone();
        let agent = message["agent"].as_str().unwrap_or_default().to_string();
        if message["t"] == "hosted"
            && let Some(relay) = &relay
        {
            match serde_json::from_value::<Notice>(message) {
                Ok(notice) => relay.noticed(&agent, notice),
                Err(e) => {
                    let _ = tx.send(Err(format!("an unreadable notice: {e}: {line}")));
                }
            }
            continue;
        }
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
        if let (Some(agent), Some(name)) = (message["agent"].as_str(), message["binding"].as_str())
        {
            stats
                .between(agent, name)
                .record(line.len(), message["id"].as_u64(), true, closed);
        }
        match relay {
            Some(relay) => match serde_json::from_value::<Rpc>(message) {
                Ok(rpc) => relay.rpc(&agent, rpc),
                Err(e) => {
                    let _ = tx.send(Err(format!("an unreadable rpc line: {e}: {line}")));
                }
            },
            None => {
                write_line(
                    &stdin,
                    &json!({
                        "t": "rpc", "agent": message["agent"], "binding": message["binding"],
                        "id": message["id"], "closed": "no bindings process runs",
                    }),
                );
            }
        }
    }
}

/// Write what the session's relay sends the interpreter, counting its `rpc` lines.
fn write_from_relay(
    lines: Receiver<Value>,
    interpreter: Arc<Mutex<Option<ChildStdin>>>,
    stats: Arc<Stats>,
) {
    for message in lines {
        let len = write_line(&interpreter, &message);
        if message["t"] != "rpc" {
            continue;
        }
        stats.to_interpreter.record(len);
        if let (Some(agent), Some(name)) = (message["agent"].as_str(), message["binding"].as_str())
        {
            stats
                .between(agent, name)
                .record(len, message["id"].as_u64(), false, false);
        }
    }
}
