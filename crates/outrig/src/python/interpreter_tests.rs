//! The interpreter's observable contract, driven directly over its NDJSON
//! protocol against the payload this build embedded.
//!
//! No podman: the container adds a filesystem and namespaces, not different
//! Python semantics, so everything worth pinning is reachable by running the
//! unpacked interpreter on the host. The payload comes from
//! [`payload::host_dir`], which unpacks it into the user's cache as a
//! session's first launch would; a build that could not embed it fails here,
//! as it fails every launch.
//!
//! Nothing waits on a sleep or a wall-clock threshold. Where a test needs two
//! things to overlap, a flag file or an `asyncio.Event` gates one on the other.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ExitStatus};
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use nix::sys::resource::{RLIM_INFINITY, Resource, getrlimit};
use nix::sys::signal::{Signal, killpg};
use nix::sys::sysinfo::sysinfo;
use nix::unistd::Pid;
use serde_json::{Value, json};

use super::host::PRIMARY;
use super::payload;
use super::testing::{PIP_PROBE, Start, capture, interpreter_command, py};

/// How long any one reply may take. Generous for a loaded CI runner; nothing
/// here comes close.
const TIMEOUT: Duration = Duration::from_secs(20);

// The program's bounds, restated so that changing one changes a test.
const OUTPUT_MAX: usize = 16 * 1024;
const BG_MAX: usize = 2 * 1024;
const REPR_MAX: usize = 1000;
const INVENTORY_MAX: usize = 200;
const HELP_MAX: usize = 8 * 1024;
const QUEUE_MAX: usize = 256;
const MESSAGE_MAX: usize = 1 << 20;
const SEND_WINDOW: usize = 16;

/// Polls `check` until it answers, failing after [`TIMEOUT`] with what it
/// was waiting for.
fn eventually<T>(mut check: impl FnMut() -> Option<T>, waited_for: impl FnOnce() -> String) -> T {
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

/// A path nothing exists at until the test says so.
struct Flag {
    _dir: tempfile::TempDir,
    path: PathBuf,
}

impl Flag {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("flag");
        Self { _dir: dir, path }
    }

    /// The path as a Python string literal.
    fn py(&self) -> String {
        format!("{:?}", self.path.to_str().expect("a UTF-8 temp path"))
    }

    fn raise(&self) {
        std::fs::write(&self.path, b"").expect("raise the flag");
    }

    /// Python that awaits the flag, failing rather than hanging if it never
    /// appears.
    fn awaited(&self) -> String {
        self.waited_with("await asyncio.sleep(0.01)")
    }

    /// The same wait, holding the kernel's loop the whole time.
    fn blocked(&self) -> String {
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

pub(super) struct Interpreter {
    child: Child,
    stdin: Option<ChildStdin>,
    replies: Receiver<Result<Value, String>>,
    stderr: Arc<Mutex<String>>,
    ready: Value,
}

impl Interpreter {
    pub(super) fn start() -> Self {
        Self::spawn(&Start::default())
    }

    /// The interpreter under a memory ceiling of `bytes`: a soft `RLIMIT_DATA`
    /// already in place when it starts, which it keeps rather than raising to
    /// its own. Far quicker to reach than half the machine.
    fn start_with_ceiling(bytes: u64) -> Self {
        Self::spawn(&Start {
            ceiling: Some(bytes),
            ..Start::default()
        })
    }

    fn spawn(start: &Start) -> Self {
        let mut child = interpreter_command(start)
            .spawn()
            .expect("the interpreter starts");
        let stdin = child.stdin.take();
        let stdout = child.stdout.take().expect("stdout is piped");
        let stderr = capture(child.stderr.take().expect("stderr is piped"));

        let (tx, replies) = channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                // Anything on stdout that is not a message is a protocol
                // failure, never something to skip.
                let reply = match line {
                    Ok(line) => match serde_json::from_str::<Value>(&line) {
                        Ok(message) if message.is_object() => Ok(message),
                        _ => Err(format!("not a protocol message: {line:?}")),
                    },
                    Err(e) => Err(format!("stdout: {e}")),
                };
                if tx.send(reply).is_err() {
                    return;
                }
            }
        });
        let mut interpreter = Self {
            child,
            stdin,
            replies,
            stderr,
            ready: Value::Null,
        };
        interpreter.ready = interpreter.recv();
        assert_eq!(interpreter.ready["t"], "ready", "{}", interpreter.ready);
        assert_eq!(interpreter.ready["agent"], PRIMARY, "{}", interpreter.ready);
        interpreter
    }

    fn send_raw(&mut self, bytes: &[u8]) {
        let stdin = self.stdin.as_mut().expect("stdin is open");
        stdin.write_all(bytes).expect("the interpreter takes it");
        stdin.flush().expect("it goes out");
    }

    fn send(&mut self, message: Value) {
        self.send_raw(format!("{message}\n").as_bytes());
    }

    /// The next message, whatever it is. Every message names an agent.
    fn recv(&mut self) -> Value {
        let message = match self.replies.recv_timeout(TIMEOUT) {
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

    /// Submit `source` and return its result, which must be the very next
    /// message: anything arriving first would mean the protocol is out of step.
    fn exec_in(&mut self, agent: &str, id: u64, source: &str) -> Value {
        self.send(json!({"t": "exec", "agent": agent, "id": id, "src": source}));
        let result = self.recv();
        assert!(
            result["t"] == "result" && result["agent"] == agent && result["id"] == id,
            "expected the result of {agent}/{id}, got: {result}"
        );
        result
    }

    pub(super) fn exec(&mut self, id: u64, source: &str) -> Value {
        self.exec_in(PRIMARY, id, source)
    }

    /// Run `source`, asserting it did not raise, and return what it printed.
    fn output_in(&mut self, agent: &str, id: u64, source: &str) -> String {
        let result = self.exec_in(agent, id, source);
        assert_eq!(result["status"], "ok", "unexpected raise: {result}");
        text(&result["output"])
    }

    pub(super) fn output(&mut self, id: u64, source: &str) -> String {
        self.output_in(PRIMARY, id, source)
    }

    /// Send `request`, and return its answer, which must be the very next
    /// message: of the same kind, for the same agent and id.
    fn ask(&mut self, request: Value) -> Value {
        self.send(request.clone());
        let answer = self.recv();
        assert!(
            ["t", "agent", "id"]
                .iter()
                .all(|key| answer[key] == request[key]),
            "expected the answer to {request}, got: {answer}"
        );
        answer
    }

    /// An inventory's whole reply, from past the name `after` when given.
    fn inventory_page(&mut self, id: u64, after: Option<&str>) -> Value {
        let mut request = json!({"t": "inv", "agent": PRIMARY, "id": id});
        if let Some(after) = after {
            request["after"] = json!(after);
        }
        self.ask(request)
    }

    fn inventory(&mut self, id: u64) -> Vec<(String, String)> {
        self.inventory_page(id, None)["globals"]
            .as_array()
            .expect("globals")
            .iter()
            .map(|row| (text(&row[0]), text(&row[1])))
            .collect()
    }

    fn open(&mut self, agent: &str) {
        self.send(json!({"t": "open", "agent": agent}));
        let ready = self.recv();
        assert!(
            ready["t"] == "ready" && ready["agent"] == agent,
            "got: {ready}"
        );
    }

    fn stderr(&self) -> String {
        self.stderr.lock().expect("stderr lock").clone()
    }

    fn await_stderr(&self, needle: &str) {
        eventually(
            || self.stderr().contains(needle).then_some(()),
            || format!("{needle:?} on stderr: {}", self.stderr()),
        );
    }

    fn exit_status(&mut self) -> ExitStatus {
        let child = &mut self.child;
        eventually(
            || child.try_wait().expect("try_wait"),
            || "the interpreter to exit".into(),
        )
    }
}

impl Drop for Interpreter {
    fn drop(&mut self) {
        if let Ok(pid) = i32::try_from(self.child.id()) {
            let _ = killpg(Pid::from_raw(pid), Signal::SIGKILL);
        }
        let _ = self.child.wait();
    }
}

fn text(value: &Value) -> String {
    value
        .as_str()
        .unwrap_or_else(|| panic!("not text: {value}"))
        .to_string()
}

/// The output `result` reports as background billed to execution `id`, or
/// nothing: background is grouped by execution, so there is one entry at most.
fn background_text(result: &Value, id: u64) -> String {
    result["background"]
        .as_array()
        .unwrap_or_else(|| panic!("no background: {result}"))
        .iter()
        .find(|entry| entry["id"] == id)
        .map(|entry| text(&entry["output"]))
        .unwrap_or_default()
}

/// Everything `result` reports as background, which must all be billed to
/// execution `id`: the text kept and the count of bytes dropped.
fn billed_only_to(result: &Value, id: u64) -> (String, usize) {
    let (mut kept, mut dropped) = (String::new(), 0);
    for entry in result["background"].as_array().expect("background") {
        assert_eq!(entry["id"], id, "billed to someone else: {result}");
        kept.push_str(&text(&entry["output"]));
        dropped += usize::try_from(entry["dropped"].as_u64().expect("a count")).expect("fits");
    }
    (kept, dropped)
}

/// Binds `wait_then_write(count, byte)`, which starts a child that writes
/// `count` copies of `byte` once `flag` exists and returns it at once, and
/// `write(count, byte)`, which runs a child that writes them now.
fn bind_children(k: &mut Interpreter, id: u64, flag: &Flag) {
    let source = py(&format!(
        r#"
        import subprocess, sys
        FLAG = {flag}
        _CHILD = """
        import os, sys, time
        flag, count, byte = sys.argv[1], int(sys.argv[2]), sys.argv[3]
        deadline = time.monotonic() + {timeout}
        while not os.path.exists(flag):
            if time.monotonic() > deadline:
                sys.exit('the flag never appeared')
            time.sleep(0.01)
        sys.stdout.write(byte * count)
        """
        def wait_then_write(count, byte):
            return subprocess.Popen([sys.executable, '-c', _CHILD, FLAG, str(count), byte])
        def write(count, byte):
            code = f'import sys; sys.stdout.write({{byte!r}} * {{count}})'
            subprocess.run([sys.executable, '-c', code], check=True)
        "#,
        flag = flag.py(),
        timeout = TIMEOUT.as_secs(),
    ));
    assert_eq!(k.output(id, &source), "");
}

// ---------------------------------------------------------------------------- the protocol

#[test]
fn ready_names_the_primary_and_the_payload_version() {
    let k = Interpreter::start();
    let version = text(&k.ready["version"]);
    assert!(
        payload::PAYLOAD.starts_with(&format!("cpython-{version}+")),
        "{version} is not {}",
        payload::PAYLOAD
    );
}

#[test]
fn globals_outlive_the_execution_that_bound_them() {
    let mut k = Interpreter::start();
    assert_eq!(k.output(1, "x = 41"), "");
    assert_eq!(k.output(2, "x + 1"), "42\n");
    assert_eq!(k.output(3, "x"), "41\n");
}

#[test]
fn top_level_await_completes_and_reports_once() {
    let mut k = Interpreter::start();
    let out = k.output(1, "await asyncio.sleep(0)\n'awaited'");
    assert_eq!(out, "'awaited'\n");
    // A second report of 1 would arrive before this result and fail it.
    assert_eq!(k.output(2, "'next'"), "'next'\n");
}

#[test]
fn a_trailing_expression_echoes_and_none_does_not() {
    let mut k = Interpreter::start();
    assert_eq!(k.output(1, "[1, 2, 3]"), "[1, 2, 3]\n");
    assert_eq!(k.output(2, "y = 5"), "");
    assert_eq!(k.output(3, "print('shown')"), "shown\n");
    assert_eq!(k.output(4, "None"), "");
}

#[test]
fn an_echoed_value_is_bounded() {
    let mut k = Interpreter::start();
    let expected = format!("'{}... [5002 chars]\n", "x".repeat(REPR_MAX - 1));
    assert_eq!(k.output(1, "'x' * 5000"), expected);
}

#[test]
fn a_raise_is_a_result_not_a_transport_failure() {
    let mut k = Interpreter::start();
    let result = k.exec(1, "print('before')\n1 / 0");
    assert_eq!(result["status"], "error");
    assert_eq!(result["output"], "before\n", "output survives a raise");
    let error = text(&result["error"]);
    assert!(error.contains("ZeroDivisionError"), "{error}");
    assert!(error.contains("<execution>"), "the frame is named: {error}");
    // The interpreter's own frames are not the model's business.
    assert!(!error.contains("<string>"), "{error}");
    assert_eq!(k.output(2, "'alive'"), "'alive'\n");
}

#[test]
fn a_syntax_error_is_reported_the_same_way() {
    let mut k = Interpreter::start();
    let result = k.exec(1, "def (");
    assert_eq!(result["status"], "error");
    let error = text(&result["error"]);
    assert!(error.contains("SyntaxError"), "{error}");
    assert!(error.contains("<execution>"), "{error}");
    assert!(!error.contains("<string>"), "{error}");
}

#[test]
fn a_lone_surrogate_still_reaches_the_host() {
    // `json.dumps` would pass these through as escapes a strict parser
    // rejects, and the whole result would be lost.
    let mut k = Interpreter::start();
    let result = k.exec(1, "print(chr(0xdc80))\nraise ValueError(chr(0xd800))");
    assert_eq!(result["status"], "error");
    assert_eq!(result["output"], "\\udc80\n");
    assert!(
        text(&result["error"]).contains("ValueError: \\ud800"),
        "{result}"
    );
}

#[test]
fn output_is_bounded_and_counts_what_it_dropped() {
    let mut k = Interpreter::start();
    let result = k.exec(1, "print('a' * 500000)");
    assert_eq!(result["status"], "ok");
    assert_eq!(text(&result["output"]), "a".repeat(OUTPUT_MAX));
    assert_eq!(result["dropped"], 500_001 - OUTPUT_MAX);
    // Past the budget, a loop of prints is counted without being piped, and
    // the count is still exact.
    let result = k.exec(2, "for _ in range(100000):\n    print('x' * 9)");
    let lines = "xxxxxxxxx\n".repeat(OUTPUT_MAX / 10 + 1);
    assert_eq!(text(&result["output"]), lines[..OUTPUT_MAX]);
    assert_eq!(result["dropped"], 1_000_000 - OUTPUT_MAX);
    // The budget is per execution, so one flood does not mute the rest.
    let after = k.exec(3, "print('after')");
    assert_eq!(after["output"], "after\n");
    assert_eq!(after["dropped"], 0);
}

#[test]
fn a_second_submission_while_one_runs_is_refused() {
    let mut k = Interpreter::start();
    let flag = Flag::new();
    let first = json!({"t": "exec", "agent": PRIMARY, "id": 1,
                       "src": format!("{}\n'released'", flag.awaited())});
    let second = json!({"t": "exec", "agent": PRIMARY, "id": 2, "src": "'second'"});
    // One write, so the second arrives before the first can have started.
    k.send_raw(format!("{first}\n{second}\n").as_bytes());

    let refused = k.recv();
    assert!(refused["t"] == "result" && refused["id"] == 2, "{refused}");
    assert_eq!(refused["status"], "refused");
    assert_eq!(refused["holder"], 1, "the refusal names who holds the slot");

    flag.raise();
    let released = k.recv();
    assert!(
        released["id"] == 1 && released["status"] == "ok",
        "{released}"
    );
    assert_eq!(released["output"], "'released'\n");
    assert_eq!(k.output(3, "'third'"), "'third'\n");
}

#[test]
fn a_submission_is_refused_while_the_body_holds_its_loop() {
    // Synchronous code holds its kernel's loop, so a check queued there would
    // run only once the body ended -- and find the slot free.
    let mut k = Interpreter::start();
    let flag = Flag::new();
    let source = format!(
        "import os\nos.write(2, b'BLOCKING\\n')\n{}\n'released'",
        flag.blocked()
    );
    k.send(json!({"t": "exec", "agent": PRIMARY, "id": 1, "src": source}));
    k.await_stderr("BLOCKING");
    k.send(json!({"t": "exec", "agent": PRIMARY, "id": 2, "src": "'second'"}));

    // Refused while 1 still blocks: the flag goes up only afterwards.
    let refused = k.recv();
    assert!(refused["t"] == "result" && refused["id"] == 2, "{refused}");
    assert_eq!(refused["status"], "refused");
    assert_eq!(refused["holder"], 1);

    flag.raise();
    let released = k.recv();
    assert!(
        released["id"] == 1 && released["output"] == "'released'\n",
        "{released}"
    );
    assert_eq!(k.output(3, "'third'"), "'third'\n");
}

#[test]
fn messages_the_interpreter_cannot_route_are_ignored() {
    let mut k = Interpreter::start();
    k.send_raw(b"not json\n[1]\nnull\n\"x\"\n\n");
    k.send_raw(&vec![b'['; 100_000]);
    k.send_raw(b"\n");
    for message in [
        json!({"t": "bogus", "agent": PRIMARY}),
        json!({"t": "exec", "agent": "nobody", "id": 1, "src": "1"}),
        json!({"t": "exec", "agent": PRIMARY, "id": "2", "src": "2"}),
        json!({"t": "exec", "agent": PRIMARY, "id": 3}),
        json!({"t": "exec", "id": 4, "src": "4"}),
        json!({"t": "open", "agent": PRIMARY}),
        json!({"t": "open", "agent": "a.b"}),
        json!({"t": "open", "agent": ""}),
    ] {
        k.send(message);
    }
    // None of them was answered: the next message is this result.
    assert_eq!(k.output(5, "'still in sync'"), "'still in sync'\n");
    // Each was reported where the host logs diagnostics.
    for needle in [
        "not an object: list",
        "unknown message type 'bogus'",
        "no agent 'nobody'",
        "has no integer id",
        "has no source",
        "agent 'primary' is already open",
        "'a.b' cannot name an agent",
    ] {
        k.await_stderr(needle);
    }
}

// ---------------------------------------------------------------------------- the session module

#[test]
fn dataclasses_and_type_hints_resolve_in_each_kernel() {
    // Both resolve a string annotation through `sys.modules[cls.__module__]`,
    // which a bare dict of globals does not satisfy. The same class names in
    // two kernels resolve to their own.
    let mut k = Interpreter::start();
    k.open("sub");
    let source = py(r#"
        import sys, typing
        from dataclasses import dataclass, fields
        class Price:
            pass
        @dataclass
        class Row:
            symbol: 'str'
            price: 'Price'
        ([f.name for f in fields(Row)], typing.get_type_hints(Row)['price'] is Price,
         Row.__module__, sys.modules[__name__].__dict__ is globals())
        "#);
    assert_eq!(
        k.output(1, &source),
        "(['symbol', 'price'], True, 'outrig_session_primary', True)\n"
    );
    assert_eq!(
        k.output_in("sub", 1, &source),
        "(['symbol', 'price'], True, 'outrig_session_sub', True)\n"
    );
}

#[test]
fn the_inventory_lists_the_models_names_and_not_the_kernels() {
    let mut k = Interpreter::start();
    k.output(1, "import json\nweights = {'a': 1}\ncount = 7");
    let rows = |pairs: &[(&str, &str)]| -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(n, t)| (n.to_string(), t.to_string()))
            .collect()
    };
    // `asyncio`, `runtime`, and `__outrig_echo__` are bound at boot and are not listed.
    assert_eq!(
        k.inventory(2),
        rows(&[("count", "int"), ("json", "module"), ("weights", "dict")])
    );
    // Hidden by identity, so a boot name the model rebinds is its own again.
    k.output(3, "asyncio = 'rebound'");
    assert_eq!(k.inventory(4)[0], ("asyncio".into(), "str".into()));
}

#[test]
fn the_inventory_is_bounded_and_survives_what_the_model_binds() {
    let mut k = Interpreter::start();
    k.output(
        1,
        &py(r#"
            for i in range(250):
                globals()[f'v{i:03}'] = i
            globals()[1] = 'not a name'
            long_one = type('L' * 2000, (), {})()
            "#),
    );
    let rows = k.inventory(2);
    assert_eq!(rows.len(), INVENTORY_MAX);
    let (_, long_type) = rows
        .iter()
        .find(|(name, _)| name == "long_one")
        .expect("long_one is listed");
    assert_eq!(*long_type, "L".repeat(REPR_MAX));
}

// ---------------------------------------------------------------------------- descriptors

#[test]
fn generated_code_cannot_forge_a_protocol_message() {
    // stdout belongs to the protocol, and nothing the program writes reaches
    // it -- from `print`, from a raw write to fd 1, or from a child.
    let mut k = Interpreter::start();
    let result = k.exec(
        1,
        &py(r##"
            import os, subprocess, sys
            FORGED = '{"t": "result", "agent": "primary", "id": 99, "status": "ok", "output": "", "dropped": 0, "error": null, "background": []}'
            print(FORGED)
            os.write(1, FORGED.replace('99', '98').encode() + b'\n')
            subprocess.run([sys.executable, '-c', 'import sys; print(sys.argv[1])',
                            FORGED.replace('99', '97')])
            "##),
    );
    assert_eq!(result["status"], "ok", "{result}");
    let output = text(&result["output"]);
    assert!(output.contains("\"id\": 99"), "{output}");
    assert!(output.contains("\"id\": 97"), "{output}");
    assert!(!output.contains("\"id\": 98"), "{output}");
    k.await_stderr("\"id\": 98");
    assert_eq!(k.output(2, "'still in sync'"), "'still in sync'\n");
}

#[test]
fn what_no_execution_wrote_goes_to_stderr_and_no_result() {
    // A raw descriptor write names no writer, and a thread started with
    // `threading` begins with no execution in its context. Both land on the
    // stream the host records as diagnostics, never in some result.
    let mut k = Interpreter::start();
    let out = k.output(
        1,
        &py(r#"
            import os, threading
            os.write(1, b'RAW-ONE\n')
            os.write(2, b'RAW-TWO\n')
            t = threading.Thread(target=print, args=('FROM-A-THREAD',))
            t.start()
            t.join()
            'done'
            "#),
    );
    assert_eq!(out, "'done'\n");
    for needle in ["RAW-ONE", "RAW-TWO", "FROM-A-THREAD"] {
        k.await_stderr(needle);
    }
}

#[test]
fn a_child_process_writes_into_its_execution() {
    let mut k = Interpreter::start();
    let out = k.output(
        1,
        &py(r#"
            import subprocess, sys
            subprocess.run([sys.executable, '-c',
                            'import sys; print("child out", flush=True); '
                            'print("child err", file=sys.stderr, flush=True)'])
            kept = subprocess.run([sys.executable, '-c', 'print("kept apart")'],
                                  capture_output=True, text=True).stdout
            subprocess.run([sys.executable, '-c', 'import sys; print("merged", file=sys.stderr)'],
                           stderr=subprocess.STDOUT)
            subprocess.Popen([sys.executable, '-c', 'print("positional")'], -1, None, None, None).wait()
            kept
            "#),
    );
    assert_eq!(
        out,
        "child out\nchild err\nmerged\npositional\n'kept apart\\n'\n"
    );
}

#[test]
fn python_and_child_output_keep_their_order() {
    // Python's writes go through the execution's pipe while it runs, so they
    // queue behind a child's rather than overtaking them.
    let mut k = Interpreter::start();
    let out = k.output(
        1,
        &py(r#"
            import subprocess, sys
            print('a')
            subprocess.run([sys.executable, '-c', 'print("b")'])
            print('c', file=sys.stderr)
            subprocess.run([sys.executable, '-c', 'print("d")'], stdout=sys.stdout)
            await asyncio.to_thread(print, 'e')
            sys.stdout.buffer.write(b'f\n')
            None
            "#),
    );
    assert_eq!(out, "a\nb\nc\nd\ne\nf\n");
}

#[test]
fn a_child_started_while_warning_does_not_deadlock() {
    // `Popen.__init__` warns through `sys.stderr` while the execution's
    // descriptor is held for the spawn.
    let mut k = Interpreter::start();
    let out = k.output(
        1,
        &py(r#"
            import os, subprocess, sys
            p = subprocess.Popen([sys.executable, '-c', 'pass'], stdout=subprocess.PIPE, bufsize=1)
            p.communicate()
            r, w = os.pipe()
            subprocess.Popen([sys.executable, '-c', 'pass'], pass_fds=[r], close_fds=False).wait()
            os.close(r)
            os.close(w)
            p.returncode
            "#),
    );
    assert!(out.contains("line buffering"), "{out}");
    assert!(out.contains("pass_fds overriding close_fds"), "{out}");
    assert!(out.ends_with("0\n"), "{out}");
}

#[test]
fn a_forked_child_can_fork_again() {
    // A child closes the protocol descriptors it inherited, so the next file
    // it opens takes one of their numbers. A grandchild inherits that file,
    // and must not close it again.
    let mut k = Interpreter::start();
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("forks");
    let out = k.output(
        1,
        &py(&format!(
            r#"
            import multiprocessing
            fork = multiprocessing.get_context('fork')
            PATH = {path:?}
            def grandchild():
                held.write('grandchild\n')
            def child():
                global held
                held = open(PATH, 'a', buffering=1)
                held.write('child\n')
                p = fork.Process(target=grandchild)
                p.start()
                p.join()
                held.write(f'grandchild exited {{p.exitcode}}\n')
            c = fork.Process(target=child)
            c.start()
            c.join()
            (c.exitcode, open(PATH).read())
            "#,
            path = path.to_str().expect("a UTF-8 temp path"),
        )),
    );
    assert_eq!(out, "(0, 'child\\ngrandchild\\ngrandchild exited 0\\n')\n");
}

#[test]
fn stdin_is_not_the_protocol() {
    let mut k = Interpreter::start();
    let result = k.exec(1, "input()");
    assert_eq!(result["status"], "error");
    assert!(text(&result["error"]).contains("EOFError"), "{result}");
    let out = k.output(
        2,
        &py(r#"
            import subprocess, sys
            subprocess.run([sys.executable, '-c', 'import sys; print(repr(sys.stdin.read()))'])
            None
            "#),
    );
    assert_eq!(out, "''\n");
    // Neither read a line meant for the reader.
    assert_eq!(k.output(3, "'still in sync'"), "'still in sync'\n");
}

// ---------------------------------------------------------------------------- agents

#[test]
fn two_agents_keep_separate_namespaces() {
    let mut k = Interpreter::start();
    k.open("sub");
    assert_eq!(k.output(1, "x = 'primary'"), "");
    let missing = k.exec_in("sub", 1, "x");
    assert_eq!(missing["status"], "error");
    assert!(text(&missing["error"]).contains("NameError"), "{missing}");

    assert_eq!(k.output_in("sub", 2, "y = 'sub'"), "");
    let missing = k.exec(2, "y");
    assert!(text(&missing["error"]).contains("NameError"), "{missing}");

    // The primary holds the main thread, the only one a signal handler runs on.
    let on_main = "import threading\nthreading.current_thread() is threading.main_thread()";
    assert_eq!(k.output(3, on_main), "True\n");
    assert_eq!(k.output_in("sub", 3, on_main), "False\n");
}

#[test]
fn deep_recursion_off_the_main_thread_raises_rather_than_crashing() {
    // musl's default thread stack overflows before CPython's recursion limit
    // is reached, which kills the process -- every agent with it.
    let mut k = Interpreter::start();
    k.open("sub");
    let deep = k.exec_in("sub", 1, "import json\njson.loads('[' * 100000)");
    assert!(text(&deep["error"]).contains("RecursionError"), "{deep}");
    let nested = "x = []\nfor _ in range(100000):\n    x = [x]\nlen(repr(x))";
    let deep = k.exec_in("sub", 2, nested);
    assert!(text(&deep["error"]).contains("RecursionError"), "{deep}");
    assert_eq!(k.output(1, "'primary unharmed'"), "'primary unharmed'\n");
}

#[test]
fn a_child_interpreter_inherits_safe_thread_stacks() {
    let mut k = Interpreter::start();
    let script = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(
        script.path(),
        r#"
import json
import threading

def recurse():
    for operation in (lambda: json.loads('[' * 100000), nested_repr):
        try:
            operation()
        except RecursionError:
            print('RecursionError')
        else:
            raise AssertionError('recursion did not raise')

def nested_repr():
    x = []
    for _ in range(100000):
        x = [x]
    repr(x)

assert threading.stack_size() == 0  # use the binary's default
thread = threading.Thread(target=recurse)
thread.start()
thread.join()
"#,
    )
    .unwrap();
    let result = k.exec(1, &format!(
        "import subprocess, sys\nr = subprocess.run([sys.executable, {:?}], capture_output=True, text=True)\nprint(r.returncode)\nprint(r.stdout, end='')\nprint(r.stderr, end='')",
        script.path().to_str().unwrap()
    ));
    assert_eq!(
        result["output"], "0\nRecursionError\nRecursionError\n",
        "{result}"
    );
}

#[test]
fn agents_run_at_the_same_time() {
    // The primary can finish only once the sub-agent has run, so this passes
    // only if one agent's running execution does not hold the other's slot.
    let mut k = Interpreter::start();
    k.open("sub");
    let flag = Flag::new();
    let waits = format!("{}\n'primary saw it'", flag.awaited());
    k.submit(1, &waits);
    let raises = format!("open({}, 'w').close()\n'sub raised it'", flag.py());
    k.submit_in("sub", 1, &raises);

    // Either may report first.
    let results = [k.recv(), k.recv()];
    assert_eq!(result_from(&results, "sub")["output"], "'sub raised it'\n");
    assert_eq!(
        result_from(&results, PRIMARY)["output"],
        "'primary saw it'\n"
    );
}

/// The one of `results` from `agent`.
fn result_from<'a>(results: &'a [Value], agent: &str) -> &'a Value {
    let result = results.iter().find(|m| m["agent"] == agent);
    result.unwrap_or_else(|| panic!("no result from {agent}: {results:?}"))
}

// ---------------------------------------------------------------------------- attribution

#[test]
fn a_background_task_is_billed_to_the_execution_that_started_it() {
    let mut k = Interpreter::start();
    let started = k.exec(
        1,
        &py(r#"
            release = asyncio.Event()
            async def noisy():
                await release.wait()
                print('N' * 5000)
            chatter = asyncio.ensure_future(noisy())
            "#),
    );
    assert_eq!(started["output"], "");

    // The task prints while 2 holds the slot, and 2 then fills its own budget
    // exactly. Had one byte of the noise been billed to 2, 2 would have
    // dropped something.
    let result = k.exec(
        2,
        &format!(
            "release.set()\nawait chatter\nprint('B' * {})",
            OUTPUT_MAX - 1
        ),
    );
    let own = format!("{}\n", "B".repeat(OUTPUT_MAX - 1));
    assert_eq!(text(&result["output"]), own, "{result}");
    assert_eq!(result["dropped"], 0);
    // Reported beside it, billed to 1, and bounded on its own: the newest
    // `BG_MAX` bytes, and a count of the rest.
    assert_eq!(
        result["background"],
        json!([{"id": 1, "output": format!("{}\n", "N".repeat(BG_MAX - 1)),
                "dropped": 5001 - BG_MAX}])
    );
}

#[test]
fn exceptions_asyncio_catches_are_billed_to_their_execution() {
    // asyncio reports a task's unretrieved exception from wherever the task is
    // collected, and a failing callback from its loop, with no execution in
    // context either way -- but the traceback is often the whole story.
    let mut k = Interpreter::start();
    k.output(
        1,
        &py(r#"
            async def boom():
                await asyncio.sleep(0)
                raise ValueError('lost in the background')
            asyncio.ensure_future(boom())
            asyncio.get_running_loop().call_soon(lambda: 1 / 0)
            None
            "#),
    );
    let result = k.exec(2, "import gc\ngc.collect()\nawait asyncio.sleep(0)\nNone");
    assert_eq!(result["output"], "", "{result}");
    let billed = background_text(&result, 1);
    assert!(billed.contains("lost in the background"), "{result}");
    assert!(billed.contains("Exception in callback"), "{result}");
    assert!(billed.contains("ZeroDivisionError"), "{result}");
}

#[test]
fn an_exit_raised_in_a_background_task_ends_only_that_task() {
    let mut k = Interpreter::start();
    k.output(
        1,
        &py(r#"
            async def leave():
                await asyncio.sleep(0)
                raise SystemExit(3)
            leaving = asyncio.ensure_future(leave())
            "#),
    );
    k.await_stderr("SystemExit escaped its event loop");
    assert_eq!(
        k.output(2, "type(leaving.exception()).__name__"),
        "'SystemExit'\n"
    );
}

#[test]
fn a_child_is_billed_to_its_execution_not_the_one_running_when_it_writes() {
    let mut k = Interpreter::start();
    let flag = Flag::new();
    bind_children(&mut k, 1, &flag);

    // 2 starts a child and returns before the child writes a byte.
    let started = k.exec(2, "a_child = wait_then_write(20000, 'a')");
    assert_eq!(started["output"], "");

    // 3 releases it, waits for all 20000 bytes to be written, then fills its
    // own budget exactly. None of 2's bytes may be in 3's output or in its
    // count of what it dropped.
    let result = k.exec(
        3,
        &format!("open(FLAG, 'w').close()\na_child.wait()\nwrite({OUTPUT_MAX}, 'b')"),
    );
    assert_eq!(result["status"], "ok", "{result}");
    assert_eq!(text(&result["output"]), "b".repeat(OUTPUT_MAX));
    assert_eq!(result["dropped"], 0);

    // Every one of 2's bytes is accounted to 2, as kept output or as a count,
    // across as many results as the drain takes to deliver them.
    let (mut kept, mut dropped, mut id, mut next) = (String::new(), 0, 3, result);
    eventually(
        || {
            let (more, more_dropped) = billed_only_to(&next, 2);
            kept.push_str(&more);
            dropped += more_dropped;
            if kept.len() + dropped >= 20000 {
                return Some(());
            }
            id += 1;
            next = k.exec(id, "None");
            assert_eq!(next["output"], "");
            None
        },
        || "all of 2's bytes billed to 2".into(),
    );
    assert_eq!(kept.len() + dropped, 20000);
    assert!(kept.chars().all(|c| c == 'a'), "{kept}");
}

#[test]
fn a_child_started_after_its_execution_reported_is_still_billed_to_it() {
    let mut k = Interpreter::start();
    k.output(
        1,
        &py(r#"
            import sys
            go = asyncio.Event()
            async def later():
                await go.wait()
                child = await asyncio.create_subprocess_exec(
                    sys.executable, '-c', 'print("LATE-CHILD")')
                await child.wait()
            task = asyncio.ensure_future(later())
            "#),
    );
    let result = k.exec(2, "go.set()\nawait task\n'done'");
    assert_eq!(result["output"], "'done'\n");

    let (mut id, mut next) = (2, result);
    eventually(
        || {
            if background_text(&next, 1).contains("LATE-CHILD") {
                return Some(());
            }
            id += 1;
            next = k.exec(id, "None");
            None
        },
        || "the late child's output, billed to 1".into(),
    );
}

#[test]
fn a_fork_is_billed_to_its_execution_before_and_after_it_reported() {
    // The fork copies the interpreter, and with it the execution the forking
    // context names -- after it has reported, one whose pipe is closed. What
    // the child writes has to reach this process either way.
    let mut k = Interpreter::start();
    let during = k.output(
        1,
        &py(r#"
            import multiprocessing
            fork = multiprocessing.get_context('fork')
            during = fork.Process(target=print, args=('IN-THE-BODY',))
            during.start()
            during.join()
            go = asyncio.Event()
            def fails():
                print('FROM-THE-FORK')
                raise ValueError('the fork failed')
            async def later():
                await go.wait()
                p = fork.Process(target=fails)
                p.start()
                p.join()
                return p.exitcode
            task = asyncio.ensure_future(later())
            "#),
    );
    assert_eq!(during, "IN-THE-BODY\n");
    let result = k.exec(2, "go.set()\nawait task");
    assert_eq!(result["output"], "1\n", "{result}");

    let (mut billed, mut id, mut next) = (String::new(), 2, result);
    eventually(
        || {
            billed.push_str(&background_text(&next, 1));
            if billed.contains("FROM-THE-FORK") && billed.contains("the fork failed") {
                return Some(());
            }
            id += 1;
            next = k.exec(id, "None");
            None
        },
        || "the fork's output, billed to 1".into(),
    );
}

#[test]
fn empty_background_writes_leave_nothing_behind() {
    // Zero bytes count nothing against the backlog's bound, so they must not
    // take a place in it either.
    let mut k = Interpreter::start();
    k.output(
        1,
        &py(r#"
            import sys
            go = asyncio.Event()
            async def quiet():
                await go.wait()
                for _ in range(1000):
                    print('', end='')
                    sys.stdout.buffer.write(b'')
                print('real', end='')
            task = asyncio.ensure_future(quiet())
            "#),
    );
    let held = "go.set()\nawait task\nimport __main__\nlen(__main__._kernels['primary']._bg)";
    let result = k.exec(2, held);
    assert_eq!(result["output"], "1\n", "{result}");
    assert_eq!(background_text(&result, 1), "real");
}

#[test]
fn a_result_does_not_wait_for_a_descendant_holding_its_pipe() {
    let mut k = Interpreter::start();
    let started = k.exec(
        1,
        &py(r#"
            import subprocess, sys
            holder = subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(60)'])
            "#),
    );
    assert_eq!(started["status"], "ok");
    // The child outlived the result it would otherwise have held up.
    assert_eq!(k.output(2, "holder.poll() is None"), "True\n");
    k.output(3, "holder.kill()\nholder.wait()");
    assert!(!k.stderr().contains("did not settle"), "{}", k.stderr());
}

#[test]
fn closing_stdin_ends_the_interpreter_even_mid_execution() {
    // A loop that never yields cannot be what keeps a dead session alive.
    let mut k = Interpreter::start();
    k.send(json!({"t": "exec", "agent": PRIMARY, "id": 1,
                  "src": "import os\nos.write(2, b'SPINNING\\n')\nwhile True: pass"}));
    k.await_stderr("SPINNING");
    k.stdin.take();
    assert!(k.exit_status().success());
}

// ---------------------------------------------------------------------------- interrupts

impl Interpreter {
    /// Submit `source` to `agent` without waiting for its result.
    fn submit_in(&mut self, agent: &str, id: u64, source: &str) {
        self.send(json!({"t": "exec", "agent": agent, "id": id, "src": source}));
    }

    /// Submit `source` to the primary without waiting for its result.
    fn submit(&mut self, id: u64, source: &str) {
        self.submit_in(PRIMARY, id, source);
    }

    fn cancel(&mut self, id: u64) {
        self.send(json!({"t": "cancel", "agent": PRIMARY, "id": id}));
    }

    fn interrupt(&mut self, id: u64, runaway: bool) {
        self.send(json!({"t": "interrupt", "agent": PRIMARY, "id": id, "runaway": runaway}));
    }

    /// The CPU time the primary's loop thread has used. Answered on the
    /// reader thread, so it answers while the loop is not turning.
    fn cpu(&mut self, id: u64) -> f64 {
        self.send(json!({"t": "cpu", "agent": PRIMARY, "id": id}));
        let reply = self.recv();
        assert!(reply["t"] == "cpu" && reply["id"] == id, "got: {reply}");
        reply["seconds"].as_f64().expect("seconds")
    }

    /// The next message, which must be execution `id`'s result.
    fn result_of(&mut self, id: u64) -> Value {
        let result = self.recv();
        assert!(
            result["t"] == "result" && result["id"] == id,
            "expected the result of {id}, got: {result}"
        );
        result
    }

    /// An inventory round trip. Answered on the loop, so it proves the loop
    /// turns and that nothing it had queued before -- a result included --
    /// is still to come.
    fn loop_turns(&mut self, id: u64) {
        self.inventory(id);
    }

    /// stderr up to a line execution `id` writes. The main thread writes both
    /// that line and any "escaped its event loop" diagnostic, so whatever the
    /// main thread wrote before running `id` is in what this returns.
    fn stderr_through(&mut self, id: u64) -> String {
        let marker = format!("THROUGH {id}");
        self.output(id, &format!("import os\nos.write(2, b'{marker}\\n')\nNone"));
        self.await_stderr(&marker);
        self.stderr()
    }

    fn ticks(&self) -> usize {
        self.stderr().matches("tick\n").count()
    }

    /// Wait until the spinning body has ticked twice more, so any signal
    /// already sent has been handled: the handler runs at the next bytecode
    /// boundary, and ticking is past several.
    fn still_spinning(&self) {
        let seen = self.ticks();
        eventually(
            || (self.ticks() >= seen + 2).then_some(()),
            || format!("two more ticks after {seen}; stderr: {}", self.stderr()),
        );
    }
}

/// A body that never yields. It says so once, then ticks on stderr now and
/// then, so a test can tell it is still spinning.
fn spinning(label: &str) -> String {
    py(&format!(
        r#"
        import os
        os.write(2, b'{label}\n')
        i = 0
        while True:
            i += 1
            if i % 200000 == 0:
                os.write(2, b'tick\n')
        "#
    ))
}

/// The interpreter's own marker on a `KeyboardInterrupt` it raised.
const INTERRUPTED: &str = "KeyboardInterrupt: interrupted by outrig";

fn error_of(result: &Value) -> String {
    assert_eq!(result["status"], "error", "{result}");
    text(&result["error"])
}

#[test]
fn a_wedge_is_interrupted_and_the_interpreter_carries_on() {
    let mut k = Interpreter::start();
    // The prototype's evidence was 5 of 5; this is ten in a row, one process.
    for round in 0..10 {
        let id = 1 + 2 * round;
        let label = format!("SPINNING {round}");
        k.submit(id, &spinning(&label));
        k.await_stderr(&label);
        k.interrupt(id, false);
        let error = error_of(&k.result_of(id));
        assert!(error.contains(INTERRUPTED), "{error}");
        assert!(error.contains("File \"<execution>\""), "{error}");
        assert!(
            !error.contains("_on_sigint"),
            "the handler's frame: {error}"
        );
        assert_eq!(k.output(id + 1, "1 + 1"), "2\n");
    }
}

#[test]
fn a_sigint_nobody_armed_changes_nothing() {
    let mut k = Interpreter::start();
    k.output(1, "kept = 41");
    let pid = Pid::from_raw(i32::try_from(k.child.id()).expect("a pid"));

    // Idle: a raw signal, and requests naming an execution that holds nothing.
    nix::sys::signal::kill(pid, Signal::SIGINT).expect("signal it");
    k.interrupt(1, false);
    k.interrupt(1, true);
    k.cancel(1);
    k.interrupt(7, true);
    k.cancel(7);
    k.loop_turns(2);
    assert_eq!(k.output(3, "kept + 1"), "42\n");

    // Spinning: agent code sending itself SIGINT is not an interrupt either.
    k.submit(4, &spinning("SPINNING"));
    k.await_stderr("SPINNING");
    nix::sys::signal::kill(pid, Signal::SIGINT).expect("signal it");
    k.still_spinning();
    k.interrupt(4, false);
    assert!(error_of(&k.result_of(4)).contains(INTERRUPTED));

    let stderr = k.stderr_through(5);
    assert!(!stderr.contains("escaped its event loop"), "{stderr}");
    assert_eq!(k.output(6, "kept"), "41\n");
}

#[test]
fn an_await_that_never_resolves_is_cancelled() {
    let mut k = Interpreter::start();
    k.submit(1, "await asyncio.get_running_loop().create_future()");
    k.loop_turns(2);
    k.cancel(1);
    let error = error_of(&k.result_of(1));
    assert!(error.contains("CancelledError"), "{error}");
    assert!(error.contains("File \"<execution>\""), "{error}");
    assert_eq!(k.output(3, "1 + 1"), "2\n");
}

/// The wrong remedy for a suspended execution is a signal: the main thread is
/// in the loop, not in agent code, and a `KeyboardInterrupt` there would land
/// in the loop's own bookkeeping. The handler declines, and the cancel that
/// is the right remedy still works.
#[test]
fn an_interrupt_leaves_a_suspended_execution_to_its_cancel() {
    let mut k = Interpreter::start();
    k.submit(1, "await asyncio.get_running_loop().create_future()");
    k.loop_turns(2);
    k.interrupt(1, false);
    k.interrupt(1, true);
    // Still holding its slot, and nothing reported: the next message is this.
    k.loop_turns(3);
    k.submit(4, "4");
    let refused = k.result_of(4);
    assert_eq!(refused["status"], "refused", "{refused}");
    assert_eq!(refused["holder"], 1);

    k.cancel(1);
    assert!(error_of(&k.result_of(1)).contains("CancelledError"));
    let stderr = k.stderr_through(5);
    assert!(!stderr.contains("escaped its event loop"), "{stderr}");
}

#[test]
fn a_late_cancel_or_interrupt_does_not_reach_the_next_execution() {
    let mut k = Interpreter::start();
    assert_eq!(k.output(1, "'first'"), "'first'\n");
    let (blocking, awaiting) = (Flag::new(), Flag::new());
    let source = format!(
        "import os\nos.write(2, b'BLOCKING\\n')\n{}\n{}\n'second'",
        blocking.blocked(),
        awaiting.awaited()
    );
    k.submit(2, &source);

    // 2 in synchronous agent code, where an interrupt for it would land.
    k.await_stderr("BLOCKING");
    k.interrupt(1, false);
    k.interrupt(1, true);
    k.cancel(1);
    blocking.raise();

    // 2 suspended, where a cancel for it would land.
    k.loop_turns(3);
    k.cancel(1);
    k.interrupt(1, false);
    k.loop_turns(4);
    awaiting.raise();
    assert_eq!(text(&k.result_of(2)["output"]), "'second'\n");
}

/// A coroutine cancelled before its first step never enters its own `try`, so
/// a cancel delivered then would leave an execution that never reports.
#[test]
fn a_cancel_before_the_first_step_is_reported_rather_than_lost() {
    let mut k = Interpreter::start();
    let hold = Flag::new();
    let source = format!(
        "async def hold():\n    import os\n    os.write(2, b'HOLDING\\n')\n{}\nheld = asyncio.create_task(hold())",
        indent(&hold.blocked())
    );
    k.output(1, &source);
    k.await_stderr("HOLDING");

    // Admitted, and queued behind the task holding the loop; then cancelled.
    k.submit(2, "import os\nos.write(2, b'RAN\\n')");
    k.cancel(2);
    hold.raise();

    let error = error_of(&k.result_of(2));
    assert!(error.contains("stopped before it started"), "{error}");
    let stderr = k.stderr_through(3);
    assert!(!stderr.contains("RAN"), "its body ran: {stderr}");
}

#[test]
fn an_execution_that_catches_its_cancel_keeps_the_slot_until_it_replies() {
    let mut k = Interpreter::start();
    let flag = Flag::new();
    let source = format!(
        "import os\ntry:\n    await asyncio.get_running_loop().create_future()\n\
         except asyncio.CancelledError:\n    os.write(2, b'CAUGHT\\n')\n{}\n'finished anyway'",
        flag.awaited()
    );
    k.submit(1, &source);
    k.loop_turns(2);
    k.cancel(1);
    k.await_stderr("CAUGHT");

    k.submit(3, "3");
    let refused = k.result_of(3);
    assert_eq!(refused["status"], "refused", "{refused}");
    assert_eq!(refused["holder"], 1);

    flag.raise();
    assert_eq!(text(&k.result_of(1)["output"]), "'finished anyway'\n");
    assert_eq!(k.output(4, "4"), "4\n");
}

/// A task an execution left running wedges the loop after that execution has
/// reported. The next submission is admitted but cannot start; interrupting
/// it ends the task that holds the loop, and the submission then runs. The
/// task's death is billed to the execution that started it, once.
#[test]
fn a_background_wedge_is_interrupted_for_the_submission_it_blocks() {
    let mut k = Interpreter::start();
    let source = format!(
        "async def spin():\n    await asyncio.sleep(0)\n{}\nspinner = asyncio.create_task(spin())",
        indent(&spinning("SPINNING"))
    );
    k.output(1, &source);
    k.await_stderr("SPINNING");

    k.submit(2, "'next'");
    k.interrupt(2, false);
    let result = k.result_of(2);
    assert_eq!(text(&result["output"]), "'next'\n", "{result}");
    let billed = background_text(&result, 1);
    assert!(billed.contains("outrig interrupted a task"), "{result}");
    assert!(billed.contains(INTERRUPTED), "{result}");
    assert!(billed.contains("in spin"), "{result}");

    // Not retrieved here: that is the interpreter's to have done, or asyncio
    // reports the same death again when the task is collected.
    assert_eq!(k.output(3, "spinner.done()"), "True\n");
    let collected = k.exec(4, "del spinner\nimport gc\ngc.collect()\nNone");
    assert_eq!(
        collected["background"],
        json!([]),
        "reported twice: {collected}"
    );
}

/// The same wedge while another execution is suspended. An interrupt scoped to
/// that execution does not touch the task, which is not its code; one the host
/// has judged a runaway does. The suspended execution keeps waiting through
/// both, and its own cancel still ends it.
#[test]
fn a_background_wedge_beside_a_suspended_execution() {
    let mut k = Interpreter::start();
    let source = format!(
        "go = asyncio.Event()\nasync def spin():\n    await go.wait()\n{}\n\
         spinner = asyncio.create_task(spin())",
        indent(&spinning("SPINNING"))
    );
    k.output(1, &source);
    k.submit(
        2,
        "go.set()\nawait asyncio.get_running_loop().create_future()",
    );
    k.await_stderr("SPINNING");

    k.interrupt(2, false);
    k.still_spinning();

    k.interrupt(2, true);
    k.loop_turns(3);
    k.cancel(2);
    let result = k.result_of(2);
    assert!(error_of(&result).contains("CancelledError"), "{result}");
    let billed = background_text(&result, 1);
    assert!(billed.contains(INTERRUPTED), "not billed to 1: {result}");
    assert_eq!(background_text(&result, 2), "", "{result}");
}

#[test]
fn the_cpu_clock_tells_a_spinning_loop_from_a_blocked_one() {
    let mut k = Interpreter::start();
    let flag = Flag::new();
    // A rate needs a window of wall time; the bounds are loose on purpose.
    let window = Duration::from_millis(500);
    let share = |k: &mut Interpreter, first: u64| {
        let before = k.cpu(first);
        let started = Instant::now();
        std::thread::sleep(window);
        let used = k.cpu(first + 1) - before;
        used / started.elapsed().as_secs_f64()
    };

    let blocked = format!(
        "import os, subprocess\nos.write(2, b'WAITING\\n')\n\
         subprocess.run(['sh', '-c', 'while [ ! -e {path} ]; do sleep 0.01; done'])\n'done'",
        path = flag.path.display()
    );
    k.submit(1, &blocked);
    k.await_stderr("WAITING");
    let idle = share(&mut k, 100);
    assert!(idle < 0.05, "blocked in waitpid, yet {idle:.3} of a CPU");
    flag.raise();
    assert_eq!(text(&k.result_of(1)["output"]), "'done'\n");

    k.submit(2, &spinning("SPINNING"));
    k.await_stderr("SPINNING");
    let busy = share(&mut k, 200);
    assert!(busy > 0.2, "spinning, yet {busy:.3} of a CPU");
    k.interrupt(2, true);
    assert!(error_of(&k.result_of(2)).contains(INTERRUPTED));
}

/// A synchronous `subprocess.run` blocks the main thread in `waitpid`. The
/// signal is aimed at that thread, so it breaks the wait; `run` then kills
/// the child it started. What that child started is another matter.
#[test]
fn an_interrupt_ends_a_blocking_run_but_not_what_it_started() {
    let mut k = Interpreter::start();
    let dir = tempfile::tempdir().expect("tempdir");
    let pidfile = dir.path().join("grandchild");
    let source = format!(
        "import subprocess\nsubprocess.run(['sh', '-c', 'sleep 60 & echo $! > {path}; wait'])",
        path = pidfile.display()
    );
    k.submit(1, &source);
    let grandchild: u32 = eventually(
        || std::fs::read_to_string(&pidfile).ok()?.trim().parse().ok(),
        || "the grandchild's pid".into(),
    );

    k.interrupt(1, false);
    let error = error_of(&k.result_of(1));
    assert!(error.contains(INTERRUPTED), "{error}");
    let alive = Path::new(&format!("/proc/{grandchild}")).exists()
        && !crate::process::process_tests::is_zombie(grandchild);
    assert!(alive, "the grandchild {grandchild} is gone");
    assert_eq!(k.output(2, "2"), "2\n");
}

/// Agent code that pumps the loop by hand puts the loop's own machinery nearer
/// the signal than its own frames. The interrupt declines there, where an
/// exception could lose a callback -- a `_start`, and with it a slot.
#[test]
fn an_interrupt_declines_inside_a_loop_agent_code_pumps_by_hand() {
    let mut k = Interpreter::start();
    let source = "import os\nloop = asyncio.get_running_loop()\nos.write(2, b'PUMPING\\n')\n\
                  loop._run_once()\n'pumped'";
    k.submit(1, source);
    k.await_stderr("PUMPING");
    // The main thread is the process's leader, so its wait channel is the
    // process's: blocked in the pumped loop's `select`.
    let wchan = format!("/proc/{}/wchan", k.child.id());
    eventually(
        || {
            std::fs::read_to_string(&wchan)
                .ok()
                .filter(|at| at.contains("poll"))
        },
        || format!("the main thread to block in the pumped select ({wchan})"),
    );
    k.interrupt(1, true);
    // The inventory is what the pumped loop runs next; had the interrupt
    // landed, the result would have come first, as an error.
    k.loop_turns(2);
    assert_eq!(text(&k.result_of(1)["output"]), "'pumped'\n");
}

/// A coroutine an imported module defines has no frame compiled from a
/// submission, so the walk out from a spin in it reaches the loop's own
/// dispatch first. Scheduled as a task -- with `create_task` or `gather` -- it
/// is still agent code, and the interrupt still lands; awaited directly it was
/// always found through the awaiting submission.
#[test]
fn an_interrupt_reaches_a_spin_in_imported_code() {
    let mut k = Interpreter::start();
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        dir.path().join("spinmod.py"),
        py(r#"
            import os
            async def spin(label):
                os.write(2, label.encode() + b'\n')
                while True:
                    pass
            "#),
    )
    .expect("write the module");
    k.output(
        1,
        &format!(
            "import sys\nsys.path.insert(0, {:?})\nimport spinmod\n\
             spinner = asyncio.create_task(spinmod.spin('TASK'))",
            dir.path().display().to_string()
        ),
    );
    k.await_stderr("TASK");
    k.submit(2, "'next'");
    k.interrupt(2, false);
    let result = k.result_of(2);
    assert_eq!(text(&result["output"]), "'next'\n", "{result}");
    assert!(
        background_text(&result, 1).contains(INTERRUPTED),
        "{result}"
    );

    for (id, how) in [
        (3, "await asyncio.gather(spinmod.spin('GATHER'))"),
        (4, "await spinmod.spin('DIRECT')"),
    ] {
        let label = how.split('\'').nth(1).expect("a label");
        k.submit(id, how);
        k.await_stderr(label);
        k.interrupt(id, false);
        let error = error_of(&k.result_of(id));
        assert!(error.contains(INTERRUPTED), "{how}: {error}");
    }
    assert_eq!(k.output(5, "5"), "5\n");
}

/// `traceback` reads an exception's `__notes__` guarding against `Exception`
/// alone, so anything else raised there would otherwise leave the wrapper and
/// end the execution without a reply.
#[test]
fn a_failure_that_cannot_be_formatted_is_still_reported() {
    let mut k = Interpreter::start();
    let source = py(r#"
        class Loud(Exception):
            @property
            def __notes__(self):
                raise SystemExit('not now')
        raise Loud()
        "#);
    let error = error_of(&k.exec(1, &source));
    assert!(
        error.contains("Loud: its traceback could not be formatted"),
        "{error}"
    );
    assert_eq!(k.output(2, "2"), "2\n");
}

/// An exception's type is named from the type itself, never through its
/// metaclass: one whose metaclass refuses `__name__` would otherwise escape
/// the wrapper while naming what was raised, and the execution would end with
/// no reply -- leaving the host's slot held for good.
#[test]
fn a_failure_whose_type_hides_its_name_is_still_reported() {
    let mut k = Interpreter::start();
    let source = py(r#"
        class Meta(type):
            def __getattribute__(cls, name):
                if name == '__name__':
                    raise RuntimeError('no name')
                return super().__getattribute__(name)
        class Hostile(Exception, metaclass=Meta):
            pass
        raise Hostile()
        "#);
    let result = k.exec(1, &source);
    assert_eq!(result["status"], "error", "{result}");
    assert_eq!(result["raised"], "Hostile", "{result}");
    assert_eq!(k.output(2, "2"), "2\n");
}

/// What names an exception's type is copied to a plain `str` before it is
/// sent: a type's `__name__` can be a `str` subclass, and one whose `encode`
/// raises would otherwise end the reply that carries it, leaving the host's
/// slot held for good.
#[test]
fn a_failure_whose_type_name_cannot_be_encoded_is_still_reported() {
    let mut k = Interpreter::start();
    let source = py(r#"
        class BadName(str):
            def encode(self, *args, **kwargs):
                raise RuntimeError('no encoding')
        class Hostile(Exception):
            pass
        Hostile.__name__ = BadName('Hostile')
        raise Hostile()
        "#);
    let result = k.exec(1, &source);
    assert_eq!(result["status"], "error", "{result}");
    assert_eq!(result["raised"], "Hostile", "{result}");
    assert_eq!(k.output(2, "2"), "2\n");
}

#[test]
fn only_the_primary_can_be_interrupted() {
    let mut k = Interpreter::start();
    k.open("helper");
    k.send(json!({"t": "interrupt", "agent": "helper", "id": 1}));
    k.await_stderr("agent 'helper' cannot be interrupted: it is not on the main thread");
    k.send(json!({"t": "cpu", "agent": "helper", "id": 2}));
    let reply = k.recv();
    assert!(reply["t"] == "cpu" && reply["seconds"].is_f64(), "{reply}");
}

/// `source` indented one level, to sit in a function body.
fn indent(source: &str) -> String {
    source
        .lines()
        .map(|line| format!("    {line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

// ---------------------------------------------------------------------------- memory

/// A ceiling a test can reach in a second or two.
const CEILING: u64 = 256 << 20;

/// Ordinary work that needs tens of mebibytes: a million-element list.
const ORDINARY: &str = "len(list(range(10**6)))";

/// Python that grows global `name` until memory runs out, a mebibyte at a
/// time.
fn grown_coarsely(name: &str) -> String {
    format!("{name} = []\nwhile True:\n    {name}.append(bytearray(1 << 20))")
}

/// The same, a small string at a time: the growth that leaves the least room
/// behind it, and the one that stopped the interpreter from reporting at all
/// until it held memory back for itself.
fn grown_finely(name: &str) -> String {
    format!("{name} = []\nwhile True:\n    {name}.append(str(len({name})) * 3)")
}

/// The error of a result that ran out of memory. Its traceback is not checked:
/// an allocation that took the last of the room can leave CPython none to
/// build one with.
fn out_of_memory(result: &Value) -> String {
    let error = error_of(result);
    assert!(error.contains("MemoryError"), "{error}");
    error
}

/// The ceiling the interpreter sets itself on this machine, by its rule
/// restated: half the smaller of the cgroup's limit and the machine's memory,
/// and never above a soft limit already in place.
fn derived_ceiling() -> u64 {
    let cgroup = [
        "/sys/fs/cgroup/memory.max",
        "/sys/fs/cgroup/memory/memory.limit_in_bytes",
    ]
    .into_iter()
    .filter_map(|path| std::fs::read_to_string(path).ok()?.trim().parse().ok());
    let total = sysinfo().expect("sysinfo").ram_total();
    let half = cgroup.chain([total]).min().expect("a size") / 2;
    let (soft, _) = getrlimit(Resource::RLIMIT_DATA).expect("RLIMIT_DATA");
    half.min(soft)
}

/// A limit as the Python below prints it.
fn limit(value: u64) -> String {
    if value == RLIM_INFINITY {
        "unlimited".into()
    } else {
        value.to_string()
    }
}

#[test]
fn an_outsized_allocation_is_an_error_result_and_the_next_submission_runs() {
    let mut k = Interpreter::start();
    let error = out_of_memory(&k.exec(1, "x = [0] * 10**12"));
    assert!(error.contains("File \"<execution>\""), "{error}");
    assert_eq!(k.output(2, "1 + 1"), "2\n");
}

#[test]
fn a_gradual_allocation_raises_at_the_ceiling_and_recovers_once_released() {
    for (grown, traced) in [(grown_coarsely("x"), true), (grown_finely("x"), false)] {
        let mut k = Interpreter::start_with_ceiling(CEILING);
        let error = out_of_memory(&k.exec(1, &grown));
        assert!(!traced || error.contains("File \"<execution>\""), "{error}");
        // `x` still holds everything the ceiling allowed, and this still runs.
        assert_eq!(k.output(2, "x = None\n'released'"), "'released'\n");
        assert_eq!(k.output(3, ORDINARY), "1000000\n");
    }
}

#[test]
fn running_out_again_before_releasing_is_still_reported() {
    // What a model plausibly does next: try again, or try something else,
    // before letting go of what the first attempt still holds.
    let mut k = Interpreter::start_with_ceiling(CEILING);
    out_of_memory(&k.exec(1, &grown_finely("x")));
    // With `x` still held, `y` gets whatever is left.
    out_of_memory(&k.exec(2, &grown_finely("y")));
    // The first attempt again, which lets go of `x` only to fill the room once more: what bricked
    // a reserve taken back only once there was room to spare.
    out_of_memory(&k.exec(3, &grown_finely("x")));
    assert_eq!(k.output(4, "x = y = None\n'released'"), "'released'\n");
    assert_eq!(k.output(5, ORDINARY), "1000000\n");
}

#[test]
fn output_written_at_the_ceiling_is_drained_and_counted_once() {
    // The drain once read into a buffer it allocated per read. At the ceiling that failed and
    // ended the drain, leaving every writer -- the body among them -- blocked on a full pipe, and
    // the result never came. Then, running out part-way through a read, it counted the whole read
    // dropped while keeping some of it.
    let mut k = Interpreter::start_with_ceiling(CEILING);
    // Enough, at the last, to fill the pipe several times over.
    let writes = [(7, 2341), (16, 1024), (64, 256), (1000, 16), (4096, 64)];
    for (id, (size, count)) in (1..).zip(writes) {
        let pin_then_write = py(&format!(
            r#"
            import sys
            out = b'y' * {size}
            x = []
            for size in (1 << 20, 1 << 16, 1 << 12):
                try:
                    while True:
                        x.append(bytearray(size))
                except MemoryError:
                    pass
            for _ in range({count}):
                sys.stdout.buffer.write(out)
            x = None
            "#
        ));
        let result = k.exec(id, &pin_then_write);
        assert_eq!(result["status"], "ok", "{result}");
        let kept = text(&result["output"]).len();
        let dropped = result["dropped"].as_u64().expect("a count");
        let dropped = usize::try_from(dropped).expect("fits");
        assert_eq!(kept + dropped, size * count, "{result}");
    }
    assert_eq!(k.output(10, ORDINARY), "1000000\n");
}

#[test]
fn an_execution_awaiting_at_the_ceiling_is_still_woken() {
    // asyncio drops a callback it has no memory to schedule. When that was a task's wakeup, the
    // task never ran again -- not even to take a cancel -- and its execution never reported. The
    // host's probe while it waits is what used the last of the room.
    let mut k = Interpreter::start_with_ceiling(CEILING);
    let pin_then_await = py(r#"
        import os
        x = []
        for size in (1 << 20, 1 << 16, 1 << 12):
            try:
                while True:
                    x.append(bytearray(size))
            except MemoryError:
                pass
        os.write(2, b'PINNED\n')
        await asyncio.sleep(1)
        x = None
        'woke'
        "#);
    k.submit(1, &pin_then_await);
    k.await_stderr("PINNED");
    k.loop_turns(50);
    let woke = k.result_of(1);
    assert_eq!(woke["output"], "'woke'\n", "{woke}");
    assert_eq!(k.output(2, ORDINARY), "1000000\n");
}

#[test]
fn a_sibling_fails_while_the_ceiling_is_pinned_and_recovers_after() {
    // The ceiling is the process's, not an agent's. This is the limit of it,
    // pinned so that it cannot quietly be forgotten.
    let mut k = Interpreter::start_with_ceiling(CEILING);
    k.open("sub");
    let gate = py(r#"
        import sys, threading, types
        gate = types.ModuleType('gate')
        gate.pinned, gate.release = threading.Event(), threading.Event()
        sys.modules['gate'] = gate
        "#);
    assert_eq!(k.output(1, &gate), "");

    // Started before the ceiling is pinned, so it already has what an
    // execution needs to run.
    let wait_then_work = py(&format!(
        r#"
        import gate
        assert gate.pinned.wait({timeout}), 'never pinned'
        {ORDINARY}
        "#,
        timeout = TIMEOUT.as_secs(),
    ));
    k.submit_in("sub", 1, &wait_then_work);
    // All but two mebibytes, held until the sub-agent says to let go.
    let pin = py(&format!(
        r#"
        import gate
        held = []
        try:
            while True:
                held.append(bytearray(1 << 20))
        except MemoryError:
            pass
        del held[-2:]
        gate.pinned.set()
        assert gate.release.wait({timeout}), 'never released'
        del held
        'released'
        "#,
        timeout = TIMEOUT.as_secs(),
    ));
    k.submit(1, &pin);

    let failed = k.recv();
    assert!(failed["agent"] == "sub" && failed["id"] == 1, "{failed}");
    out_of_memory(&failed);

    // Still pinned; the reserve is what lets this one start.
    k.submit_in("sub", 2, "import gate\ngate.release.set()");
    let results = [k.recv(), k.recv()];
    assert_eq!(result_from(&results, "sub")["status"], "ok", "{results:?}");
    assert_eq!(result_from(&results, PRIMARY)["output"], "'released'\n");

    assert_eq!(k.output_in("sub", 3, ORDINARY), "1000000\n");
}

#[test]
fn an_execd_child_inherits_the_ceiling_and_can_lift_its_own() {
    let (_, hard) = getrlimit(Resource::RLIMIT_DATA).expect("RLIMIT_DATA");
    let (space_soft, space_hard) = getrlimit(Resource::RLIMIT_AS).expect("RLIMIT_AS");
    let report = py(r#"
        import subprocess, sys
        CHILD = """
        import resource
        def show(which):
            return ' '.join('unlimited' if v == resource.RLIM_INFINITY else str(v)
                            for v in resource.getrlimit(which))
        print(show(resource.RLIMIT_DATA), show(resource.RLIMIT_AS))
        resource.setrlimit(resource.RLIMIT_DATA, (resource.getrlimit(resource.RLIMIT_DATA)[1],) * 2)
        print(show(resource.RLIMIT_DATA))
        """
        child = subprocess.run([sys.executable, '-c', CHILD], check=True)
        "#);
    for (mut k, ceiling) in [
        (Interpreter::start_with_ceiling(CEILING), CEILING),
        (Interpreter::start(), derived_ceiling()),
    ] {
        // The soft limit is the ceiling; the hard one and the address space
        // are as the interpreter found them. Lifting its own is up to the
        // child, and needs no privilege.
        let expected = format!(
            "{} {} {} {}\n{} {}\n",
            limit(ceiling),
            limit(hard),
            limit(space_soft),
            limit(space_hard),
            limit(hard),
            limit(hard),
        );
        assert_eq!(k.output(1, &report), expected);
    }
}

#[test]
fn a_real_build_runs_under_the_ceiling() {
    // What the ceiling reaches besides Python: a toolchain whose threads and
    // allocators reserve far more than they use. Under an inherited
    // 512 MiB, `cargo build` of this fails.
    let dir = tempfile::tempdir().expect("tempdir");
    let manifest = "[package]\nname = \"hello\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n\
                    [workspace]\n";
    std::fs::write(dir.path().join("Cargo.toml"), manifest).expect("write Cargo.toml");
    std::fs::create_dir(dir.path().join("src")).expect("create src");
    std::fs::write(
        dir.path().join("src/main.rs"),
        "fn main() {\n    println!(\"hello\");\n}\n",
    )
    .expect("write main.rs");
    let build = py(&format!(
        r#"
        import os, subprocess
        build = subprocess.run(
            [{cargo:?}, 'build', '--offline', '--quiet'],
            cwd={dir:?},
            env={{**os.environ, 'CARGO_TARGET_DIR': os.path.join({dir:?}, 'target')}},
            capture_output=True,
            text=True,
        )
        print(build.returncode)
        if build.returncode:
            print(build.stderr[-4000:])
        "#,
        cargo = env!("CARGO"),
        dir = dir.path().to_str().expect("a UTF-8 temp path"),
    ));
    let mut k = Interpreter::start();
    assert_eq!(k.output(1, &build), "0\n");
}

// ---------------------------------------------------------------------------- channels

impl Interpreter {
    /// Post `body` on `agent`'s user channel as request `id`, and return the
    /// answer.
    fn post_to(&mut self, agent: &str, id: u64, body: Value) -> Value {
        self.ask(json!({"t": "msg", "agent": agent, "id": id, "channel": "user", "body": body}))
    }

    fn post(&mut self, id: u64, body: Value) -> Value {
        self.post_to(PRIMARY, id, body)
    }

    /// What `agent` has waiting, and has ever had delivered, by channel.
    fn pending_in(&mut self, agent: &str, id: u64) -> Value {
        self.ask(json!({"t": "pending", "agent": agent, "id": id}))["channels"].take()
    }
}

/// A posted message is counted from the moment it is answered, stays counted
/// however often the count is asked for, and leaves the count only when code
/// receives it -- in the order it was sent, and never twice.
#[test]
fn a_message_waits_until_code_receives_it_once_and_in_order() {
    let mut k = Interpreter::start();
    assert_eq!(
        k.post(1, json!("first")),
        json!({"t": "msg", "agent": PRIMARY, "id": 1, "pending": 1})
    );
    assert_eq!(k.post(2, json!("second"))["pending"], 2);
    let two = json!({"user": {"pending": 2, "delivered": 2}});
    assert_eq!(k.pending_in(PRIMARY, 3), two);
    assert_eq!(k.output(4, "runtime.channels['user'].pending()"), "2\n");
    assert_eq!(k.pending_in(PRIMARY, 5), two, "counting took nothing");

    let read = py(r#"
        d = await runtime.channels['user'].receive()
        print(d.body, d.sender, d.received_at.tzinfo, runtime.channels['user'].pending())
        "#);
    assert_eq!(k.output(6, &read), "first user UTC 1\n");
    assert_eq!(k.output(7, &read), "second user UTC 0\n");
    // Received, but still counted as delivered: that is how a new message is
    // told from one the model has heard of.
    assert_eq!(
        k.pending_in(PRIMARY, 8),
        json!({"user": {"pending": 0, "delivered": 2}})
    );
}

/// A receive with nothing queued waits for the next message rather than
/// failing. One that is cancelled while it waits takes nothing with it: the
/// message goes to the next receive.
#[test]
fn a_receive_waits_and_a_cancelled_one_takes_nothing() {
    let mut k = Interpreter::start();
    k.output(
        1,
        py(r#"
        ch = runtime.channels['user']
        gone = asyncio.create_task(ch.receive())
        await asyncio.sleep(0)
        gone.cancel()
        waiting = asyncio.create_task(ch.receive())
        await asyncio.sleep(0)
        "#)
        .as_str(),
    );
    assert_eq!(k.post(2, json!("hello"))["pending"], 1);
    assert_eq!(
        k.output(3, "(await waiting).body, gone.cancelled(), ch.pending()"),
        "('hello', True, 0)\n"
    );
}

/// What the agent sends reaches the host as text, one line per message, in
/// the order it was sent and ahead of the result of the code that sent it.
#[test]
fn sends_reach_the_host_in_order_ahead_of_the_result() {
    let mut k = Interpreter::start();
    k.send(json!({"t": "exec", "agent": PRIMARY, "id": 1, "src":
        "for word in ['one', 'two', 'three']:\n    await runtime.channels['user'].send(word)"}));
    for word in ["one", "two", "three"] {
        assert_eq!(
            k.recv(),
            json!({"t": "send", "agent": PRIMARY, "channel": "user", "body": word})
        );
    }
    let result = k.recv();
    assert!(
        result["t"] == "result" && result["status"] == "ok",
        "{result}"
    );
}

/// The user channel carries text both ways. A message of any other type is
/// refused, whichever way it goes, and nothing is queued or sent.
#[test]
fn the_user_channel_carries_text_both_ways() {
    let mut k = Interpreter::start();
    let refused = k.post(1, json!(42));
    assert_eq!(
        refused["error"], "channel 'user' receives str, not int",
        "{refused}"
    );
    assert!(refused.get("pending").is_none(), "{refused}");
    k.send(json!({"t": "msg", "agent": PRIMARY, "id": 2, "channel": "nope", "body": "x"}));
    assert_eq!(k.recv()["error"], "agent 'primary' has no channel 'nope'");

    let result = k.exec(3, "await runtime.channels['user'].send(42)");
    assert_eq!(result["status"], "error", "{result}");
    assert!(
        text(&result["error"]).ends_with("TypeError: channel 'user' sends str, not int\n"),
        "{result}"
    );
    assert_eq!(
        k.pending_in(PRIMARY, 4),
        json!({"user": {"pending": 0, "delivered": 0}}),
        "a refused message was never delivered"
    );
    // The contract is there to read, as a peer reads it.
    assert_eq!(
        k.output(
            5,
            "runtime.channels['user'].receives, runtime.channels['user'].sends"
        ),
        "((<class 'str'>,), (<class 'str'>,))\n"
    );
}

/// A channel holds a bounded number of unread messages, and the one past the
/// bound is refused saying so, not dropped. Reading one makes room.
#[test]
fn a_full_channel_refuses_rather_than_dropping() {
    let mut k = Interpreter::start();
    for id in 1..=QUEUE_MAX as u64 {
        assert_eq!(k.post(id, json!(format!("m{id}")))["pending"], id);
    }
    let refused = k.post(1000, json!("one too many"));
    assert_eq!(
        refused["error"],
        format!("channel 'user' already holds {QUEUE_MAX} unread messages")
    );
    assert_eq!(
        k.output(1001, "(await runtime.channels['user'].receive()).body"),
        "'m1'\n"
    );
    assert_eq!(
        k.post(1002, json!("now there is room"))["pending"],
        QUEUE_MAX
    );
}

/// The agent runs ahead of the user by `SEND_WINDOW` messages and no further:
/// past that, a send waits in the agent's own code until the host says one was
/// received. A fast producer is held back there, under the interpreter's
/// memory ceiling, rather than piling up in the host's memory.
#[test]
fn a_send_waits_once_the_user_is_a_window_behind() {
    let mut k = Interpreter::start();
    let total = SEND_WINDOW + 2;
    k.send(json!({"t": "exec", "agent": PRIMARY, "id": 1, "src":
        format!("for i in range({total}):\n    await runtime.channels['user'].send(str(i))")}));
    for n in 0..SEND_WINDOW {
        assert_eq!(k.recv()["body"], n.to_string());
    }
    // The next send waits, and the loop, not waiting, answers first.
    k.inventory(2);
    for n in SEND_WINDOW..total {
        k.send(json!({"t": "received", "agent": PRIMARY, "id": 100 + n, "channel": "user"}));
        assert_eq!(k.recv()["body"], n.to_string());
    }
    let result = k.recv();
    assert!(
        result["t"] == "result" && result["status"] == "ok",
        "{result}"
    );
}

/// Python defining `CopyFailsOnce`, a list whose next `copy()` runs out of
/// memory, and making it the user endpoint's list of waiting sends.
const SENDERS_COPY_FAILS_ONCE: &str = r#"
ch = runtime.channels['user']
class CopyFailsOnce(list):
    failed = False
    def copy(self):
        if not CopyFailsOnce.failed:
            CopyFailsOnce.failed = True
            raise MemoryError
        return list(self)
ch._senders = CopyFailsOnce()
"#;

/// Running out of memory while taking in an acknowledgment, before it counts,
/// is retried rather than swallowed: the room it makes is not lost, and the
/// send waiting for that room goes out.
#[test]
fn memory_running_out_before_an_acknowledgment_counts_loses_no_room() {
    let mut k = Interpreter::start();
    let source = format!(
        "{SENDERS_COPY_FAILS_ONCE}\nfor i in range({}):\n    await ch.send(str(i))",
        SEND_WINDOW + 1
    );
    k.send(json!({"t": "exec", "agent": PRIMARY, "id": 1, "src": source}));
    for n in 0..SEND_WINDOW {
        assert_eq!(k.recv()["body"], n.to_string());
    }
    k.inventory(2);
    k.send(json!({"t": "received", "agent": PRIMARY, "id": 3, "channel": "user"}));
    assert_eq!(k.recv()["body"], SEND_WINDOW.to_string());
    let result = k.recv();
    assert!(
        result["t"] == "result" && result["status"] == "ok",
        "{result}"
    );
}

/// A send that fails to go out gives its room back, even if memory runs out
/// while it does, and raises what it failed with.
#[test]
fn a_send_that_fails_gives_its_room_back() {
    let mut k = Interpreter::start();
    let source = format!(
        "{SENDERS_COPY_FAILS_ONCE}\n{}",
        py(r#"
        import __main__
        real = __main__._write_line
        def broken(line):
            __main__._write_line = real
            raise OSError('the pipe broke')
        __main__._write_line = broken
        try:
            await ch.send('lost')
        except OSError as e:
            print(e)
        print(ch._unreceived)
        "#)
    );
    assert_eq!(k.output(1, &source), "the pipe broke\n0\n");
}

/// A message the agent sends is bounded as it is sent: one past the bound
/// raises in the code that sent it, and nothing reaches the host.
#[test]
fn a_send_past_the_bound_raises_and_sends_nothing() {
    let mut k = Interpreter::start();
    let result = k.exec(
        1,
        &format!("await runtime.channels['user'].send('x' * {MESSAGE_MAX})"),
    );
    assert_eq!(result["status"], "error", "{result}");
    assert!(
        text(&result["error"]).contains(&format!("past the {MESSAGE_MAX} a channel carries")),
        "{result}"
    );
}

/// A contract is checked when the channel is made, not when a message first
/// crosses it: a type outside the serializable subset is refused then, named,
/// with the field it sits in when it is nested in a dataclass.
#[test]
fn a_contract_outside_the_serializable_subset_is_refused_at_construction() {
    let mut k = Interpreter::start();
    let refusals = k.output(
        1,
        &py(r#"
        import __main__
        from dataclasses import dataclass
        from typing import Optional

        @dataclass
        class Tagged:
            name: str
            tags: set[int]

        @dataclass
        class Outer:
            inner: Tagged

        @dataclass
        class Unresolvable:
            thing: 'NoSuchType'

        for contract in (bytes, list, dict[int, str], tuple[str, int], Tagged, Outer, Unresolvable,
                         str | bytes):
            try:
                __main__.Endpoint('primary', 'x', receives=contract, sends=str)
                print('accepted', contract)
            except TypeError as e:
                print(e)
        "#),
    );
    let lines: Vec<&str> = refusals.lines().collect();
    let refused = |named: &str| format!("channel 'x' cannot receive {named}, which is outside");
    assert!(lines[0].starts_with(&refused("bytes")), "{refusals}");
    assert!(lines[1].starts_with(&refused("list")), "{refusals}");
    assert!(
        lines[2].starts_with(&refused("dict[int, str]")),
        "{refusals}"
    );
    assert!(
        lines[3].starts_with(&refused("tuple[str, int]")),
        "{refusals}"
    );
    assert!(
        lines[4].starts_with(&refused("set[int] (field 'tags' of Tagged)")),
        "{refusals}"
    );
    assert!(
        lines[5].starts_with(&refused("set[int] (field 'tags' of Tagged)")),
        "nested one dataclass down: {refusals}"
    );
    assert!(
        lines[6].starts_with(
            "channel 'x' cannot receive Unresolvable, whose field types cannot be \
                              resolved: NameError"
        ),
        "{refusals}"
    );
    assert!(lines[7].starts_with(&refused("bytes")), "{refusals}");
    assert_eq!(lines.len(), 8, "{refusals}");

    let accepted = k.output(
        2,
        &py(r#"
        from typing import Optional

        @dataclass
        class Node:
            value: float
            children: list['Node']
            parent: Optional['Node'] = None

        @dataclass
        class Row:
            name: str
            scores: dict[str, float]
            node: Node | None

        for contract in (str, list[int], dict[str, float], Row | None, (int, bool), ()):
            print(__main__.Endpoint('primary', 'x', receives=contract, sends=contract))
        "#),
    );
    assert_eq!(
        accepted,
        "<endpoint 'x': receives str, sends str, 0 pending>\n\
         <endpoint 'x': receives list[int], sends list[int], 0 pending>\n\
         <endpoint 'x': receives dict[str, float], sends dict[str, float], 0 pending>\n\
         <endpoint 'x': receives Row | None, sends Row | None, 0 pending>\n\
         <endpoint 'x': receives int | bool, sends int | bool, 0 pending>\n\
         <endpoint 'x': receives nothing, sends nothing, 0 pending>\n"
    );
}

/// Every agent has a user channel of its own: a message to one is not counted
/// by another.
#[test]
fn each_agent_has_its_own_user_channel() {
    let mut k = Interpreter::start();
    k.open("helper");
    assert_eq!(
        k.post_to("helper", 1, json!("for the helper"))["pending"],
        1
    );
    assert_eq!(
        k.pending_in(PRIMARY, 2),
        json!({"user": {"pending": 0, "delivered": 0}})
    );
    assert_eq!(
        k.output_in(
            "helper",
            3,
            "(await runtime.channels['user'].receive()).body"
        ),
        "'for the helper'\n"
    );
}

/// A receive whose loop has closed cannot be woken. The message is still
/// queued exactly once and the host still told so, and the next receive takes
/// it.
#[test]
fn a_receive_that_cannot_be_woken_costs_nothing() {
    let mut k = Interpreter::start();
    k.output(
        1,
        &py(r#"
        import threading
        ch = runtime.channels['user']
        def orphan():
            loop = asyncio.new_event_loop()
            loop.create_task(ch.receive())
            loop.run_until_complete(asyncio.sleep(0.01))
            loop.close()
        thread = threading.Thread(target=orphan)
        thread.start()
        thread.join()
        "#),
    );
    assert_eq!(k.post(2, json!("still here"))["pending"], 1);
    k.await_stderr("a waiting receive or send could not be woken");
    assert_eq!(
        k.output(3, "(await ch.receive()).body, ch.pending()"),
        "('still here', 0)\n"
    );
}

/// A message is queued once, whatever fails after it is. Memory running out
/// once it is queued -- here, while reporting a receive that could not be
/// woken -- is not `_handle`'s to retry, which would queue the message again.
#[test]
fn memory_running_out_after_a_message_is_queued_does_not_queue_it_twice() {
    let mut k = Interpreter::start();
    k.output(
        1,
        &py(r#"
        import contextlib, threading
        ch = runtime.channels['user']
        def orphan():
            loop = asyncio.new_event_loop()
            loop.create_task(ch.receive())
            loop.run_until_complete(asyncio.sleep(0.01))
            loop.close()
        thread = threading.Thread(target=orphan)
        thread.start()
        thread.join()
        # Building a `suppress` is an allocation; the reader's next one fails.
        real_suppress = contextlib.suppress
        def once_on_the_reader(*exceptions):
            if threading.current_thread().name != 'reader':
                return real_suppress(*exceptions)
            contextlib.suppress = real_suppress
            raise MemoryError
        contextlib.suppress = once_on_the_reader
        "#),
    );
    assert_eq!(k.post(2, json!("once"))["pending"], 1);
    assert_eq!(
        k.pending_in(PRIMARY, 3),
        json!({"user": {"pending": 1, "delivered": 1}})
    );
}

/// An interrupt never lands inside a protocol line, which agent code writes
/// when it sends: a line cut short would swallow the next.
#[test]
fn a_send_is_machinery_an_interrupt_does_not_land_in() {
    let mut k = Interpreter::start();
    assert_eq!(
        k.output(
            1,
            "import __main__\n__main__._write_line.__code__ in __main__._MACHINERY"
        ),
        "True\n"
    );
}

// ---------------------------------------------------------------------------- waiting

/// Python that writes `PARKED <id>` on stderr once the loop turns again. Put
/// just before an `await`, it is written once that await has suspended.
fn parked(id: u64) -> String {
    format!("import os\nasyncio.get_running_loop().call_soon(os.write, 2, b'PARKED {id}\\n')")
}

impl Interpreter {
    /// Submit `source` as execution `id`, and return once it has written
    /// [`parked`]`(id)`: suspended in the await that follows it.
    fn park(&mut self, id: u64, source: &str) {
        self.submit(id, source);
        self.await_stderr(&format!("PARKED {id}\n"));
    }

    /// Post `body` on the primary's channel `channel` as request `id`, while
    /// execution `waiting` is parked on a wait the post ends, and return the
    /// post's answer and that execution's result. The reader thread answers
    /// the post and the loop reports the result, so either can come first.
    fn post_waking(&mut self, id: u64, channel: &str, body: Value, waiting: u64) -> (Value, Value) {
        self.send(
            json!({"t": "msg", "agent": PRIMARY, "id": id, "channel": channel, "body": body}),
        );
        let (first, second) = (self.recv(), self.recv());
        let (answer, result) = if first["t"] == "msg" {
            (first, second)
        } else {
            (second, first)
        };
        assert!(
            answer["t"] == "msg" && answer["id"] == id,
            "expected the answer to {id}, got: {answer}"
        );
        assert!(
            result["t"] == "result" && result["id"] == waiting,
            "expected the result of {waiting}, got: {result}"
        );
        (answer, result)
    }
}

/// `runtime.wait` is spelled exactly as `asyncio.wait` is, so nothing in the
/// signature says it also watches the channels: the preamble has to.
#[test]
fn a_wait_has_asyncios_signature() {
    let mut k = Interpreter::start();
    assert_eq!(
        k.output(
            1,
            "import inspect\ninspect.signature(runtime.wait) == inspect.signature(asyncio.wait)"
        ),
        "True\n"
    );
}

/// Two operations and `FIRST_COMPLETED`: the wait returns once the first has
/// finished and not before, with the other in `pending`, where it goes on
/// running.
#[test]
fn first_completed_returns_the_first_and_leaves_the_other_running() {
    let mut k = Interpreter::start();
    let flag = Flag::new();
    let source = format!(
        "async def released():\n{}\nfirst = asyncio.create_task(released(), name='first')\n\
         second = asyncio.create_task(asyncio.Event().wait(), name='second')\n{}\n\
         done, pending = await runtime.wait({{first, second}}, \
         return_when=asyncio.FIRST_COMPLETED)\n\
         [t.get_name() for t in done], [t.get_name() for t in pending]",
        indent(&flag.awaited()),
        parked(1),
    );
    k.park(1, &source);
    // Still waiting: the loop answers, and nothing was reported before it.
    k.loop_turns(2);
    flag.raise();
    let result = k.result_of(1);
    assert_eq!(result["status"], "ok", "{result}");
    assert_eq!(text(&result["output"]), "(['first'], ['second'])\n");
    assert_eq!(
        k.output(
            3,
            "second.done(), second.cancelled(), runtime.channels['user']._receivers"
        ),
        "(False, False, [])\n"
    );
}

/// `ALL_COMPLETED` is the default, and `FIRST_EXCEPTION` returns at the first
/// failure, as asyncio's do.
#[test]
fn all_completed_is_the_default_and_first_exception_returns_at_a_failure() {
    let mut k = Interpreter::start();
    let output = k.output(
        1,
        &py(r#"
            async def fails():
                raise ValueError('boom')
            a = asyncio.create_task(asyncio.sleep(0), name='a')
            b = asyncio.create_task(asyncio.sleep(0), name='b')
            done, pending = await runtime.wait([a, b])
            print(sorted(t.get_name() for t in done), pending)
            failed = asyncio.create_task(fails(), name='failed')
            slow = asyncio.create_task(asyncio.Event().wait(), name='slow')
            done, pending = await runtime.wait({failed, slow}, return_when=asyncio.FIRST_EXCEPTION)
            print([t.get_name() for t in done], [t.get_name() for t in pending])
            "#),
    );
    assert_eq!(output, "['a', 'b'] set()\n['failed'] ['slow']\n");
}

/// A timeout returns what is done and what is not. It raises nothing and
/// cancels nothing: what is pending goes on running.
#[test]
fn a_timeout_returns_without_raising_or_cancelling() {
    let mut k = Interpreter::start();
    let output = k.output(
        1,
        &py(r#"
            quick = asyncio.create_task(asyncio.sleep(0), name='quick')
            slow = asyncio.create_task(asyncio.Event().wait(), name='slow')
            done, pending = await runtime.wait({quick, slow}, timeout=0.05)
            print([t.get_name() for t in done], [t.get_name() for t in pending])
            done, pending = await runtime.wait({slow}, timeout=0)
            print(done, [t.get_name() for t in pending], slow.cancelled())
            "#),
    );
    assert_eq!(output, "['quick'] ['slow']\nset() ['slow'] False\n");
    assert_eq!(
        k.output(2, "slow.done(), runtime.channels['user']._receivers"),
        "(False, [])\n"
    );
}

/// A task that failed is a completed task: it comes back in `done`, its
/// exception there to read, rather than raised out of the wait.
#[test]
fn a_failed_task_comes_back_done_rather_than_raised() {
    let mut k = Interpreter::start();
    let result = k.exec(
        1,
        &py(r#"
            async def fails():
                raise ValueError('boom')
            failed = asyncio.create_task(fails(), name='failed')
            done, pending = await runtime.wait({failed})
            done == {failed}, pending, failed.exception()
            "#),
    );
    assert_eq!(result["status"], "ok", "{result}");
    assert_eq!(
        text(&result["output"]),
        "(True, set(), ValueError('boom'))\n"
    );
}

/// A message arriving ends a wait on something that will never finish. It
/// raises, naming the channel, and takes nothing: the message still waits to
/// be received, and the operation is still running.
#[test]
fn a_message_ends_a_wait_naming_its_channel_and_takes_nothing() {
    let mut k = Interpreter::start();
    k.output(
        1,
        "op = asyncio.create_task(asyncio.Event().wait(), name='op')",
    );
    k.park(2, &format!("{}\nawait runtime.wait({{op}})", parked(2)));
    let (answer, result) = k.post_waking(3, "user", json!("hello"), 2);
    assert_eq!(answer["pending"], 1, "{answer}");
    let error = error_of(&result);
    assert!(
        error.ends_with(
            "MessageAvailable: input is waiting on runtime.channels[\"user\"]; it has not been \
             read, and the wait cancelled nothing\n"
        ),
        "{error}"
    );
    assert_eq!(
        k.output(
            4,
            "ch = runtime.channels['user']\nop.done(), op.cancelled(), ch.pending(), ch._receivers"
        ),
        "(False, False, 1, [])\n"
    );
    assert_eq!(k.output(5, "(await ch.receive()).body"), "'hello'\n");
}

/// `MessageAvailable` is a `BaseException`, so the `except Exception:` an agent
/// wraps around its work does not swallow a redirection.
#[test]
fn except_exception_does_not_catch_message_available() {
    let mut k = Interpreter::start();
    assert_eq!(k.post(1, json!("stop"))["pending"], 1);
    let result = k.exec(
        2,
        &py(r#"
            op = asyncio.create_task(asyncio.Event().wait())
            try:
                await runtime.wait({op})
            except Exception:
                print('swallowed')
            "#),
    );
    assert!(
        error_of(&result).contains("\nMessageAvailable: input is waiting"),
        "{result}"
    );
    assert_eq!(result["output"], "", "{result}");
}

/// Input wins: a message queued before the wait makes it raise even though the
/// task it waits on has already finished. Code that names the exception
/// catches it, the message stays queued, and the result stays in the task.
#[test]
fn input_wins_over_a_finished_task() {
    let mut k = Interpreter::start();
    assert_eq!(k.post(1, json!("stop"))["pending"], 1);
    let source = py(r#"
        finished = asyncio.create_task(asyncio.sleep(0, 'result'))
        await finished
        try:
            await runtime.wait({finished})
        except runtime.MessageAvailable as e:
            print(e.channels, finished.result(), runtime.channels['user'].pending())
        "#);
    assert_eq!(k.output(2, &source), "('user',) result 1\n");
}

/// Input also wins when it arrives with the completion that would have ended
/// the wait: both land before the wait next looks, and it raises rather than
/// returning.
#[test]
fn input_wins_when_it_arrives_with_a_completion() {
    let mut k = Interpreter::start();
    let source = py(r#"
        op = asyncio.get_running_loop().create_future()
        def both():
            op.set_result('result')
            runtime.channels['user']._deliver('now', 'user', 0)
        asyncio.get_running_loop().call_soon(both)
        try:
            await runtime.wait({op})
        except runtime.MessageAvailable as e:
            print(e.channels, op.result())
        "#);
    assert_eq!(k.output(1, &source), "('user',) result\n");
}

/// A message a receive already waiting takes first does not end the wait:
/// woken, the wait finds nothing unread and goes on waiting, and returns when
/// its operation does.
#[test]
fn a_message_another_receive_takes_does_not_end_the_wait() {
    let mut k = Interpreter::start();
    let flag = Flag::new();
    k.output(
        1,
        &format!(
            "async def released():\n{}\nop = asyncio.create_task(released())\n\
             reader = asyncio.create_task(runtime.channels['user'].receive())\n\
             await asyncio.sleep(0)",
            indent(&flag.awaited())
        ),
    );
    k.park(
        2,
        &format!(
            "{}\ndone, pending = await runtime.wait({{op}})\nlen(done), (await reader).body",
            parked(2)
        ),
    );
    assert_eq!(k.post(3, json!("for the reader"))["pending"], 1);
    flag.raise();
    let result = k.result_of(2);
    assert_eq!(result["status"], "ok", "{result}");
    assert_eq!(text(&result["output"]), "(1, 'for the reader')\n");
}

/// Every channel the agent has is watched, not the user's alone: one added to
/// the kernel is watched with no change to the wait, and a message on it
/// raises naming it. So a wake the host adds later takes the same path.
#[test]
fn a_wait_watches_every_channel_the_agent_has() {
    let mut k = Interpreter::start();
    k.output(
        1,
        &py(r#"
            import __main__
            __main__._kernels['primary'].channels['control'] = __main__.Endpoint(
                'primary', 'control', receives=str, sends=str)
            op = asyncio.create_task(asyncio.Event().wait())
            "#),
    );
    k.park(2, &format!("{}\nawait runtime.wait({{op}})", parked(2)));
    let (answer, result) = k.post_waking(3, "control", json!("check in"), 2);
    assert_eq!(answer["pending"], 1, "{answer}");
    assert!(
        error_of(&result)
            .contains("MessageAvailable: input is waiting on runtime.channels[\"control\"];"),
        "{result}"
    );
}

/// Cancelling an execution reaches a task it awaits directly, and not one it
/// waits on through `runtime.wait`, as `execution-and-rounds.md` measured. A
/// Ctrl-C that stops a wait leaves the work it was waiting on running.
#[test]
fn a_cancelled_wait_leaves_its_operation_running_where_a_bare_await_does_not() {
    let mut k = Interpreter::start();
    k.output(
        1,
        "waited = asyncio.create_task(asyncio.Event().wait())\n\
         awaited = asyncio.create_task(asyncio.Event().wait())",
    );
    for (id, how) in [(2, "await runtime.wait({waited})"), (3, "await awaited")] {
        k.park(id, &format!("{}\n{how}", parked(id)));
        k.cancel(id);
        let error = error_of(&k.result_of(id));
        assert!(error.contains("CancelledError"), "{error}");
    }
    assert_eq!(
        k.output(
            4,
            "waited.cancelled(), awaited.cancelled(), runtime.channels['user']._receivers"
        ),
        "(False, True, [])\n"
    );
}

/// What asyncio refuses, the wait refuses in asyncio's words: a coroutine,
/// inside `fs` or as it; a lone future; nothing to wait on, an iterator that
/// turns out empty included; and a `return_when` asyncio lacks.
#[test]
fn a_wait_refuses_what_asyncio_refuses() {
    let mut k = Interpreter::start();
    let refusals = k.output(
        1,
        &py(r#"
            async def job():
                pass
            coro = job()
            future = asyncio.get_running_loop().create_future()
            for refused in (
                lambda: runtime.wait([coro]),
                lambda: runtime.wait(coro),
                lambda: runtime.wait(future),
                lambda: runtime.wait([]),
                lambda: runtime.wait(iter(())),
                lambda: runtime.wait([future], return_when='SOMETIMES'),
            ):
                try:
                    await refused()
                except (TypeError, ValueError) as e:
                    print(type(e).__name__, e)
            coro.close()
            "#),
    );
    assert_eq!(
        refusals,
        "TypeError Passing coroutines is forbidden, use tasks explicitly.\n\
         TypeError expect a list of futures, not coroutine\n\
         TypeError expect a list of futures, not Future\n\
         ValueError Set of Tasks/Futures is empty.\n\
         ValueError Set of Tasks/Futures is empty.\n\
         ValueError Invalid return_when value: SOMETIMES\n"
    );
}

/// A wait registers with each future once, however they finish: `N` futures
/// finishing one loop turn apart cost it `N` callbacks, as they cost
/// `asyncio.wait`, rather than a pass over every future still pending each
/// time one finishes.
#[test]
fn a_wait_registers_with_each_future_once() {
    let mut k = Interpreter::start();
    let source = py(r#"
        class Counted(asyncio.Future):
            registered = 0
            def add_done_callback(self, fn, *, context=None):
                Counted.registered += 1
                super().add_done_callback(fn, context=context)
        loop = asyncio.get_running_loop()
        futures = [Counted(loop=loop) for _ in range(100)]
        async def finish_one_per_turn():
            for f in futures:
                f.set_result(None)
                await asyncio.sleep(0)
        finishing = asyncio.create_task(finish_one_per_turn())
        done, pending = await runtime.wait(futures)
        len(done), len(pending), Counted.registered
        "#);
    assert_eq!(k.output(1, &source), "(100, 0, 100)\n");
}

// ---------------------------------------------------------------------------- history

impl Interpreter {
    /// Push turn `id`, with one call per `(source, result)`, as the host
    /// commits one. Nothing answers.
    fn push_turn(&mut self, id: u64, round: u64, calls: &[(&str, &str)]) {
        let calls: Vec<Value> = calls
            .iter()
            .map(|(source, result)| json!({"source": source, "result": result}))
            .collect();
        self.send(json!({
            "t": "turn", "agent": PRIMARY, "id": id,
            "round": round, "prompt": (id == 0).then_some("go"), "text": format!("turn {id}"),
            "calls": calls,
        }));
    }
}

/// What the host pushes is held as ordinary Python data: frozen, readable with
/// any expression, and printed small.
#[test]
fn turns_the_host_pushes_read_as_python_data() {
    let mut k = Interpreter::start();
    k.push_turn(0, 1, &[("x = 1", "(no output)"), ("print(x)", "1\n")]);
    k.push_turn(1, 1, &[]);
    let scan = py(r#"
        [(t.id, t.round, t.prompt, t.text, [(c.source, c.result) for c in t.calls])
         for t in runtime.history.turns]
        "#);
    assert_eq!(
        k.output(1, &scan),
        "[(0, 1, 'go', 'turn 0', [('x = 1', '(no output)'), ('print(x)', '1\\n')]), \
         (1, 1, None, 'turn 1', [])]\n"
    );
    assert_eq!(
        k.output(2, "runtime.history, runtime.history.turns[0]"),
        "(<history: 2 turns over 1 rounds>, <turn 0, round 1: 2 calls, 6 characters of text>)\n"
    );
    let result = k.exec(3, "runtime.history.turns[0].text = 'rewritten'");
    assert!(
        text(&result["error"]).contains("FrozenInstanceError"),
        "{result}"
    );
}

/// A turn is held once, and only after the last one held: a repeat or one
/// arriving out of order is dropped, and a turn whose line never arrived
/// leaves a gap rather than stopping the ones after it.
#[test]
fn a_turn_is_held_once_and_a_lost_one_leaves_a_gap() {
    let mut k = Interpreter::start();
    for id in [0, 0, 2, 1, 3] {
        k.push_turn(id, 1, &[]);
    }
    k.send(json!({"t": "turn", "agent": PRIMARY, "id": 4, "round": 1, "calls": "no"}));
    k.await_stderr("turn 4 for 'primary' is not a turn");
    assert_eq!(
        k.output(1, "[t.id for t in runtime.history.turns]"),
        "[0, 2, 3]\n"
    );
}

/// `turns` is a snapshot: a turn committed while a scan runs is not added to
/// what the scan is reading.
#[test]
fn a_scan_sees_the_turns_there_when_it_began() {
    let mut k = Interpreter::start();
    k.push_turn(0, 1, &[]);
    k.output(1, "seen = runtime.history.turns");
    k.push_turn(1, 1, &[]);
    assert_eq!(
        k.output(2, "len(seen), len(runtime.history.turns)"),
        "(1, 2)\n"
    );
}

/// Memory running out after a turn is held is `_handle`'s to retry, which
/// runs the whole route again; the turn is still held once.
#[test]
fn memory_running_out_after_a_turn_is_held_does_not_hold_it_twice() {
    let mut k = Interpreter::start();
    k.output(
        1,
        &py(r#"
        import threading
        history = runtime.history
        real_add = type(history)._add
        calls = []
        def held_then_out_of_memory(turn):
            calls.append(threading.current_thread().name)
            real_add(history, turn)
            if len(calls) == 1:
                raise MemoryError
        history._add = held_then_out_of_memory
        "#),
    );
    k.push_turn(0, 1, &[]);
    assert_eq!(
        k.output(2, "[t.id for t in runtime.history.turns], calls"),
        "([0], ['reader', 'reader'])\n",
        "tried twice, held once"
    );
}

/// A promotion goes to the host as the ids of the turns it names, however
/// they were named, ahead of the result of the code that made it.
#[test]
fn a_promotion_reaches_the_host_as_ids_ahead_of_the_result() {
    let mut k = Interpreter::start();
    k.push_turn(0, 1, &[]);
    k.push_turn(1, 1, &[]);
    k.submit(
        1,
        "turns = runtime.history.turns\nruntime.context.promote(turns[1], 0, [turns[1]])",
    );
    assert_eq!(
        k.recv(),
        json!({"t": "promote", "agent": PRIMARY, "turns": [0, 1]})
    );
    let result = k.result_of(1);
    assert!(
        result["status"] == "ok" && text(&result["output"]).is_empty(),
        "{result}"
    );
    assert_eq!(
        k.output(2, "runtime.context.promoted, runtime.context"),
        "((0, 1), <context: 2 turns promoted>)\n"
    );
}

/// A promotion names turns that are there, by a turn or its id, or it is
/// refused whole and nothing is sent: the next message is the result.
#[test]
fn a_promotion_of_anything_but_a_turn_that_is_there_is_refused() {
    let mut k = Interpreter::start();
    k.push_turn(0, 1, &[]);
    for (id, (source, error)) in [
        (
            "runtime.context.promote(0, 7)",
            "ValueError: no finished turn has id 7",
        ),
        (
            "runtime.context.promote(True)",
            "TypeError: a turn or a turn's id, not bool",
        ),
        (
            "runtime.context.promote('0')",
            "TypeError: a turn or a turn's id, not str",
        ),
        (
            "runtime.context.promote(0.5)",
            "TypeError: a turn or a turn's id, not float",
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let result = k.exec(id as u64 + 1, source);
        assert!(
            result["status"] == "error" && text(&result["error"]).ends_with(&format!("{error}\n")),
            "{source}: {result}"
        );
    }
    assert_eq!(k.output(9, "runtime.context.promoted"), "()\n");
}

/// A demotion goes to the host the way a promotion does, ahead of the
/// result, and `promoted` stops listing the turn. Every turn named is sent,
/// promoted here or not, since the host may hold a promotion this side never
/// recorded.
#[test]
fn a_demotion_reaches_the_host_ahead_of_the_result_and_promoted_reflects_it() {
    let mut k = Interpreter::start();
    k.push_turn(0, 1, &[]);
    k.push_turn(1, 1, &[]);
    k.submit(1, "runtime.context.promote(0, 1)");
    assert_eq!(
        k.recv(),
        json!({"t": "promote", "agent": PRIMARY, "turns": [0, 1]})
    );
    k.result_of(1);
    k.submit(2, "runtime.context.demote(runtime.history.turns[1], 0)");
    assert_eq!(
        k.recv(),
        json!({"t": "demote", "agent": PRIMARY, "turns": [0, 1]})
    );
    k.result_of(2);
    assert_eq!(k.output(3, "runtime.context.promoted"), "()\n");
    k.submit(4, "runtime.context.demote(1)");
    assert_eq!(
        k.recv(),
        json!({"t": "demote", "agent": PRIMARY, "turns": [1]}),
        "demoting what is not promoted is still said, and changes nothing"
    );
    k.result_of(4);
    for (id, (source, error)) in [
        (
            "runtime.context.demote(7)",
            "ValueError: no finished turn has id 7",
        ),
        (
            "runtime.context.demote(False)",
            "TypeError: a turn or a turn's id, not bool",
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let result = k.exec(id as u64 + 5, source);
        assert!(
            result["status"] == "error" && text(&result["error"]).ends_with(&format!("{error}\n")),
            "{source}: {result}"
        );
    }
}

/// A promotion and a demotion of one turn made at once, from two threads,
/// leave the host and `promoted` agreeing: each change is told and recorded as
/// one step, so whichever reaches the host last is what `promoted` says. The
/// demotion here is held up just after its line is written, which is where the
/// two once crossed.
#[test]
fn a_promotion_racing_a_demotion_leaves_the_host_and_promoted_agreeing() {
    let mut k = Interpreter::start();
    k.push_turn(0, 1, &[]);
    k.submit(
        1,
        &py(r#"
        import threading, time
        context = runtime.context
        names = type(context).promote.__globals__
        write = names["_write_line"]
        def slow_after_a_demotion(line):
            write(line)
            if b'"demote"' in line:
                time.sleep(0.3)
        names["_write_line"] = slow_after_a_demotion
        try:
            demoting = threading.Thread(target=context.demote, args=(0,))
            demoting.start()
            time.sleep(0.1)
            context.promote(0)
            demoting.join()
        finally:
            names["_write_line"] = write
        print(context.promoted)
        "#),
    );
    assert_eq!(
        k.recv(),
        json!({"t": "demote", "agent": PRIMARY, "turns": [0]})
    );
    assert_eq!(
        k.recv(),
        json!({"t": "promote", "agent": PRIMARY, "turns": [0]})
    );
    let result = k.result_of(1);
    assert_eq!(
        text(&result["output"]),
        "(0,)\n",
        "the host holds the promotion, so `promoted` must too: {result}"
    );
}

/// A turn the round ended during reads as incomplete, and says so when
/// printed; a turn whose mark is not a bool is not a turn.
#[test]
fn an_incomplete_turn_reads_as_incomplete() {
    let mut k = Interpreter::start();
    k.send(json!({
        "t": "turn", "agent": PRIMARY, "id": 0, "round": 1, "prompt": "go", "text": "",
        "calls": [{"source": "x", "result": "[outrig: not run]"}], "incomplete": true,
    }));
    k.push_turn(1, 1, &[]);
    k.send(json!({
        "t": "turn", "agent": PRIMARY, "id": 2, "round": 1, "prompt": null, "text": "",
        "calls": [], "incomplete": "yes",
    }));
    k.await_stderr("turn 2 for 'primary' is not a turn");
    assert_eq!(
        k.output(
            1,
            "[t.incomplete for t in runtime.history.turns], runtime.history.turns[0]"
        ),
        "([True, False], <turn 0, round 1: 1 calls, 0 characters of text, incomplete>)\n"
    );
}

/// The measurement gate for keeping the whole conversation in the
/// interpreter: what a long session of full-size results costs it, measured
/// against its memory ceiling. The host's side is
/// `the_host_record_and_the_transport_of_a_long_session_are_measured`.
#[test]
fn a_long_sessions_mirror_is_measured_against_the_ceiling() {
    const TURNS: u64 = 1_000;
    const RESULT: usize = 16 << 10;
    let mut k = Interpreter::start();
    k.output(1, "import tracemalloc; tracemalloc.start()");
    let result = "r".repeat(RESULT);
    for id in 0..TURNS {
        k.push_turn(id, id / 10 + 1, &[("print(x)", &result)]);
    }
    let measured = k.output(
        2,
        &py(r#"
        import resource, sys
        current, peak = tracemalloc.get_traced_memory()
        tracemalloc.stop()
        def size(t):
            parts = (t, t.calls, t.text, t.prompt, *t.calls)
            return sum(map(sys.getsizeof, parts)) + sum(
                sys.getsizeof(c.source) + sys.getsizeof(c.result) for c in t.calls
            )
        turns = runtime.history.turns
        walked = sum(map(size, turns)) + sys.getsizeof(turns)
        ceiling = resource.getrlimit(resource.RLIMIT_DATA)[0]
        print(len(turns), current, peak, walked, runtime.history._held, ceiling)
        "#),
    );
    let numbers: Vec<u64> = measured
        .split_whitespace()
        .map(|n| n.parse().unwrap_or(u64::MAX))
        .collect();
    let [turns, current, peak, walked, held, ceiling] = numbers[..] else {
        panic!("six numbers: {measured}");
    };
    let payload = TURNS * RESULT as u64;
    eprintln!(
        "{TURNS} turns of {RESULT} bytes: payload {payload}, traced {current} (peak {peak}), \
         walked {walked}, counted {held}, ceiling {ceiling}"
    );
    assert_eq!(turns, TURNS);
    assert!(walked < payload * 11 / 10, "walked {walked}");
    assert!(current < payload * 125 / 100, "traced {current}");
    assert!(
        peak - current < 8 * RESULT as u64,
        "a turn at a time is in flight: peak {peak}, current {current}"
    );
    assert!(
        held >= payload && held < walked,
        "the count is of the text: {held}"
    );
}

/// Past an eighth of the ceiling, the mirror says so on stderr, once, and
/// keeps every turn.
#[test]
fn a_mirror_past_an_eighth_of_the_ceiling_is_reported_once() {
    const RESULT: usize = 256 << 10;
    let mut k = Interpreter::start_with_ceiling(256 << 20);
    let result = "r".repeat(RESULT);
    // 33 MiB of results, past an eighth of 256 MiB, then as much again.
    for id in 0..264 {
        k.push_turn(id, 1, &[("print(x)", &result)]);
    }
    k.await_stderr("past an eighth of the interpreter's 256 MiB memory ceiling");
    assert_eq!(k.output(1, "len(runtime.history.turns)"), "264\n");
    assert_eq!(
        k.stderr().matches("past an eighth").count(),
        1,
        "{}",
        k.stderr()
    );
}

// ---------------------------------------------------------------------------- what the agent can ask

/// The echo renders inside the execution that asked, so a `__repr__` that
/// never returns wedges that execution alone, and the interrupt ends it. The
/// inventory renders outside any, so it names the value without calling it.
#[test]
fn a_looping_repr_wedges_only_its_execution_and_the_inventory_still_answers() {
    let mut k = Interpreter::start();
    k.output(
        1,
        &py(r#"
            import os
            class Loop:
                def __repr__(self):
                    os.write(2, b'IN REPR\n')
                    while True:
                        pass
            x = Loop()
            "#),
    );
    k.submit(2, "x");
    k.await_stderr("IN REPR");
    k.interrupt(2, false);
    let error = error_of(&k.result_of(2));
    assert!(error.contains(INTERRUPTED), "{error}");
    let held = k.inventory(3);
    assert!(held.contains(&("x".into(), "Loop".into())), "{held:?}");
    assert_eq!(k.output(5, "runtime.names()['x']"), "'Loop'\n");
}

/// A listing past the cap says how many names there are and how many it
/// left out, and the next page starts after its last name.
#[test]
fn an_over_cap_inventory_counts_what_it_left_out_and_offers_the_rest() {
    let mut k = Interpreter::start();
    k.output(1, "globals().update({f'v{i:03}': i for i in range(250)})");
    let first = k.inventory_page(2, None);
    let rows = first["globals"].as_array().expect("globals");
    assert_eq!(rows.len(), INVENTORY_MAX);
    assert_eq!((&first["total"], &first["more"]), (&json!(250), &json!(50)));
    let last = text(&rows[INVENTORY_MAX - 1][0]);
    assert_eq!(last, "v199");

    let rest = k.inventory_page(3, Some(&last));
    let rows = rest["globals"].as_array().expect("globals");
    assert_eq!((rows.len(), text(&rows[0][0])), (50, "v200".to_string()));
    assert_eq!((&rest["total"], &rest["more"]), (&json!(250), &json!(0)));

    // The agent's own listing is a value, so it is whole; the runtime's
    // names are not in it.
    assert_eq!(
        k.output(
            4,
            "names = runtime.names()\nlen(names), names['v249'], 'asyncio' in names, 'runtime' in names"
        ),
        "(250, 'int', False, False)\n"
    );
}

#[test]
fn help_describes_a_runtime_object_within_its_bound() {
    let mut k = Interpreter::start();
    let wait = k.output(1, "help(runtime.wait)");
    assert!(
        wait.contains("async wait(fs, *, timeout=None, return_when='ALL_COMPLETED')")
            && wait.contains("Wait for the futures in `fs` as `asyncio.wait` does"),
        "{wait}"
    );
    let python = k.output(2, "help(runtime.python)");
    assert!(
        python.contains("Compiled code never loads") && python.contains("image_python"),
        "{python}"
    );
    let history = k.output(8, "help(runtime.history)");
    assert!(
        history.contains("Your whole conversation, a turn at a time")
            && history.contains("costs no context"),
        "{history}"
    );
    let context = k.output(9, "help(runtime.context)");
    assert!(
        context.contains("promote(self, *turns)") && context.contains("in its place"),
        "{context}"
    );
    let runtime = k.output(10, "help(runtime)");
    assert!(
        !runtime.contains("[help cut at") && runtime.contains("runtime.context.promote(turn)"),
        "{runtime}"
    );

    // All of asyncio is some 225 KB; the answer is cut at a line, and says
    // how to ask for less.
    let result = k.exec(3, "help(asyncio)");
    assert_eq!(result["dropped"], 0, "{result}");
    let asyncio = text(&result["output"]);
    let (kept, marker) = asyncio
        .rsplit_once("[help cut at ")
        .unwrap_or_else(|| panic!("no cut marker: {asyncio}"));
    assert!(
        kept.chars().count() <= HELP_MAX && kept.ends_with('\n'),
        "{kept}"
    );
    assert!(marker.contains("help(x.name)"), "{marker}");

    // No interactive utility to read /dev/null: a guide, and back.
    assert!(
        k.output(4, "help()").starts_with("help(x) describes x"),
        "{}",
        k.stderr()
    );
    assert!(k.output(5, "help('for')").contains("The \"for\" statement"));

    // A module search writes its matches to the answer, after its heading and within the bound.
    let dir = tempfile::tempdir().expect("tempdir");
    let synopsis = format!("outrigsearchprobe {}", "x".repeat(2 * HELP_MAX));
    std::fs::write(
        dir.path().join("outrig_search_probe.py"),
        format!("\"\"\"{synopsis}\"\"\"\n"),
    )
    .expect("write");
    k.output(6, &format!("import sys\nsys.path.append({:?})", dir.path()));
    let search = k.output(7, "help('modules outrigsearchprobe')");
    let (kept, marker) = search
        .rsplit_once("[help cut at ")
        .unwrap_or_else(|| panic!("no cut marker: {search}"));
    assert!(
        kept.chars().count() <= HELP_MAX
            && kept.starts_with("\nHere is a list of modules")
            && kept.contains("outrig_search_probe - outrigsearchprobe xxx"),
        "{kept}"
    );
    assert!(marker.contains("help(x.name)"), "{marker}");
}

/// Each failure whose reason is this interpreter says so, and what else is
/// wrong with an import is left as Python put it.
#[test]
fn a_failed_import_says_why_when_the_reason_is_this_interpreter() {
    let dir = tempfile::tempdir().expect("tempdir");
    // Carries this interpreter's suffix on every architecture, so it is found
    // and then cannot be loaded.
    std::fs::write(dir.path().join("native.abi3.so"), b"not really").expect("write");
    // Built for glibc, so it is not even found.
    std::fs::write(
        dir.path()
            .join("glibc_only.cpython-313-x86_64-linux-gnu.so"),
        b"",
    )
    .expect("write");
    let package = dir.path().join("purepkg");
    std::fs::create_dir(&package).expect("mkdir");
    std::fs::write(package.join("__init__.py"), b"").expect("write");
    std::fs::write(
        package.join("_speedups.cpython-313-x86_64-linux-gnu.so"),
        b"",
    )
    .expect("write");

    let mut k = Interpreter::start();
    k.output(1, &format!("import sys\nsys.path.append({:?})", dir.path()));
    let explained = [
        ("import native", "cannot import 'native': "),
        ("import glibc_only", "cannot import 'glibc_only': "),
        (
            "import purepkg._speedups",
            "cannot import 'purepkg._speedups': ",
        ),
    ];
    for (id, (source, head)) in (2..).zip(explained) {
        let error = error_of(&k.exec(id, source));
        let line = error.lines().last().expect("an exception line");
        assert!(
            line.contains(head)
                && line.contains(
                    ".so is compiled code, and this interpreter cannot load compiled code"
                )
                && line.contains("through subprocess, or as a service."),
            "{error}"
        );
    }
    let absent = error_of(&k.exec(5, "import numpy_outrig_absent"));
    assert!(
        absent.lines().last().is_some_and(|line| line.starts_with(
            "ModuleNotFoundError: No module named 'numpy_outrig_absent'. If a pure-Python \
             package provides it, `pip install` that package"
        )),
        "{absent}"
    );
    // Explained through a cause, as the traceback shows it.
    let chained = error_of(&k.exec(
        6,
        "try:\n    import numpy_outrig_absent\nexcept ImportError as e:\n    raise RuntimeError('needs it') from e",
    ));
    assert!(chained.contains("`pip install` that package"), "{chained}");

    // Left alone: a submodule a pure package lacks, a platform's missing
    // standard module, and what code that catches the error sees itself.
    let missing = error_of(&k.exec(7, "import purepkg.missing"));
    assert!(
        missing.ends_with("ModuleNotFoundError: No module named 'purepkg.missing'\n"),
        "{missing}"
    );
    let winreg = error_of(&k.exec(8, "import winreg"));
    assert!(
        winreg.ends_with("ModuleNotFoundError: No module named 'winreg'\n"),
        "{winreg}"
    );
    assert_eq!(
        k.output(
            9,
            "try:\n    import other_outrig_absent\nexcept ImportError as e:\n    print(e)"
        ),
        "No module named 'other_outrig_absent'\n"
    );

    // A task nobody awaited is reported by asyncio's handler, and explained there too.
    k.output(
        10,
        &py(r#"
            async def needs():
                await asyncio.sleep(0)
                import background_outrig_absent
            asyncio.ensure_future(needs())
            None
            "#),
    );
    let result = k.exec(11, "import gc\ngc.collect()\nawait asyncio.sleep(0)\nNone");
    let billed = background_text(&result, 10);
    assert!(billed.contains("`pip install` that package"), "{result}");
}

/// Explaining a failed import reads a namespace package's path as importlib last computed it.
/// Iterating it would compute it again once its parent's path had changed, through every path
/// hook and finder the agent installed, outside the execution that installed them.
#[test]
fn explaining_a_failed_import_runs_no_finder_the_agent_installed() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (hooked, namespace, later) = (
        dir.path().join("hooked"),
        dir.path().join("namespace"),
        dir.path().join("later"),
    );
    for path in [&hooked, &namespace.join("nspkg"), &later] {
        std::fs::create_dir_all(path).expect("mkdir");
    }
    let mut k = Interpreter::start();
    k.output(
        1,
        &py(&format!(
            r#"
            import sys
            HOOKED, NAMESPACE, LATER = {hooked:?}, {namespace:?}, {later:?}
            calls, armed = [], [False]
            class Finder:
                def find_spec(self, name, target=None):
                    if armed[0]:
                        calls.append(name)
                    return None
            def hook(path):
                if path == HOOKED:
                    return Finder()
                raise ImportError(path)
            sys.path_hooks.insert(0, hook)
            sys.path += [HOOKED, NAMESPACE]
            import nspkg
            "#
        )),
    );
    let failed = error_of(&k.exec(
        2,
        &py(r#"
            try:
                import nspkg.missing
            finally:
                armed[0] = True
                sys.path.append(LATER)
            "#),
    ));
    assert!(
        failed.ends_with("ModuleNotFoundError: No module named 'nspkg.missing'\n"),
        "{failed}"
    );
    assert_eq!(k.output(3, "calls"), "[]\n");
}

/// The case an agent reaches for first: a module of the project's own. The
/// workspace follows the standard library, so a file named like a standard
/// module does not replace it.
#[test]
fn a_pure_python_module_in_the_workspace_imports() {
    let workspace = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        workspace.path().join("wsmod.py"),
        b"X = 'from the workspace'\n",
    )
    .expect("write");
    std::fs::write(workspace.path().join("colorsys.py"), b"SHADOWED = True\n").expect("write");
    let mut k = Interpreter::spawn(&Start {
        dir: Some(workspace.path()),
        ..Start::default()
    });
    assert_eq!(
        k.output(1, "import wsmod\nwsmod.X"),
        "'from the workspace'\n"
    );
    assert_eq!(
        k.output(2, "import colorsys\nhasattr(colorsys, 'rgb_to_hsv')"),
        "True\n"
    );
    assert_eq!(
        k.output(3, "print(runtime.python.workspace)"),
        format!("{}\n", workspace.path().display())
    );
}

/// `pip` on `PATH` is the interpreter's own, and what it installs imports in
/// the running interpreter, without a restart. `--user` is explicit only
/// because the payload is writable on the host, and this test must not write
/// into it; in a session it is read-only, and pip picks the user site itself,
/// which the e2e suite checks.
#[test]
fn pip_installs_a_pure_package_that_imports_at_once() {
    let home = tempfile::tempdir().expect("tempdir");
    let wheels = tempfile::tempdir().expect("tempdir");
    // pip runs in the workspace, and a project's own files are not pip's: one named `pip.py`
    // would run in its place, and one named `types.py` would stop it starting.
    let workspace = tempfile::tempdir().expect("tempdir");
    std::fs::write(workspace.path().join("pip.py"), b"print('not pip')\n").expect("write");
    std::fs::write(workspace.path().join("types.py"), b"X = 42\n").expect("write");
    let mut k = Interpreter::spawn(&Start {
        dir: Some(workspace.path()),
        home: Some(home.path()),
        ..Start::default()
    });
    k.output(1, PIP_PROBE);
    // Variables an image sets for its own Python, which would stop this one's pip starting, or
    // stop it installing to the user site. Its own Python still gets them.
    k.output(
        4,
        "os.environ.update(PYTHONHOME='/nonexistent', PYTHONNOUSERSITE='1')\n\
         subprocess.run(['sh', '-c', 'test \"$PYTHONHOME\" = /nonexistent'], check=True)\n\
         None",
    );
    let installed = format!(
        "pip('install', '--user', '--no-index', wheel({wheels:?}, 'outrig_probe'))\n\
         import outrig_probe, shutil\n\
         (outrig_probe.ANSWER, outrig_probe.__file__.startswith({home:?}), \
          shutil.which('pip') == os.path.expanduser('~/.local/share/outrig/bin/pip'))",
        wheels = wheels.path(),
        home = home.path(),
    );
    assert_eq!(k.output(2, &installed), "(42, True, True)\n");
    // `--target` is not in the way either.
    let target = format!(
        "import sys\n\
         pip('install', '--no-index', '--target', {target:?}, wheel({wheels:?}, 'outrig_target'))\n\
         sys.path.append({target:?})\n\
         import outrig_target\n\
         outrig_target.ANSWER",
        target = wheels.path().join("target"),
        wheels = wheels.path(),
    );
    assert_eq!(k.output(3, &target), "42\n");
}

/// Bounding what the model sees of a value does not bound the value: it
/// stays whole in Python, and a later execution reads any part of it exactly.
#[test]
fn a_value_larger_than_the_context_stays_whole_and_only_its_observation_is_cut() {
    let mut k = Interpreter::start();
    let size = (8 << 20) + 3;
    k.output(1, "big = 'x' * (8 << 20) + 'END'");
    let echo = k.output(2, "big");
    assert!(
        echo.len() < REPR_MAX + 64 && echo.ends_with(&format!("... [{} chars]\n", size + 2)),
        "{echo}"
    );
    let printed = k.exec(3, "print(big)");
    assert_eq!(text(&printed["output"]).len(), OUTPUT_MAX);
    assert_eq!(printed["dropped"], size + 1 - OUTPUT_MAX, "{printed}");
    assert_eq!(
        k.output(4, "len(big), big[-3:], big[4_000_000:4_000_003]"),
        format!("({size}, 'END', 'xxx')\n")
    );
}
