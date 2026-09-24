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

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;
use serde_json::{Value, json};

use super::host::{ARGS, PRIMARY};
use super::payload;

/// How long any one reply may take. Generous for a loaded CI runner; nothing
/// here comes close.
const TIMEOUT: Duration = Duration::from_secs(20);

// The program's bounds, restated so that changing one changes a test.
const OUTPUT_MAX: usize = 16 * 1024;
const BG_MAX: usize = 2 * 1024;
const REPR_MAX: usize = 1000;
const INVENTORY_MAX: usize = 200;

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

/// The embedded payload's interpreter, unpacked once for the whole binary.
fn python() -> &'static Path {
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
fn py(source: &str) -> String {
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

struct Interpreter {
    child: Child,
    stdin: Option<ChildStdin>,
    replies: Receiver<Result<Value, String>>,
    stderr: Arc<Mutex<String>>,
    ready: Value,
}

impl Interpreter {
    fn start() -> Self {
        let mut child = Command::new(python())
            .args(ARGS)
            .arg(PRIMARY)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // A group of its own, so `Drop` reaches the children tests start.
            .process_group(0)
            .spawn()
            .expect("the interpreter starts");
        let stdin = child.stdin.take();
        let stdout = child.stdout.take().expect("stdout is piped");
        let mut stderr_pipe = child.stderr.take().expect("stderr is piped");

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
        let stderr = Arc::new(Mutex::new(String::new()));
        let sink = Arc::clone(&stderr);
        std::thread::spawn(move || {
            let mut buf = [0; 4096];
            while let Ok(n @ 1..) = stderr_pipe.read(&mut buf) {
                sink.lock()
                    .expect("stderr lock")
                    .push_str(&String::from_utf8_lossy(&buf[..n]));
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

    fn exec(&mut self, id: u64, source: &str) -> Value {
        self.exec_in(PRIMARY, id, source)
    }

    /// Run `source`, asserting it did not raise, and return what it printed.
    fn output_in(&mut self, agent: &str, id: u64, source: &str) -> String {
        let result = self.exec_in(agent, id, source);
        assert_eq!(result["status"], "ok", "unexpected raise: {result}");
        text(&result["output"])
    }

    fn output(&mut self, id: u64, source: &str) -> String {
        self.output_in(PRIMARY, id, source)
    }

    fn inventory(&mut self, id: u64) -> Vec<(String, String)> {
        self.send(json!({"t": "inv", "agent": PRIMARY, "id": id}));
        let reply = self.recv();
        assert!(reply["t"] == "inv" && reply["id"] == id, "got: {reply}");
        reply["globals"]
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
    // `asyncio` and `__outrig_echo__` are bound at boot and are not listed.
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
fn agents_run_at_the_same_time() {
    // The primary can finish only once the sub-agent has run, so this passes
    // only if one agent's running execution does not hold the other's slot.
    let mut k = Interpreter::start();
    k.open("sub");
    let flag = Flag::new();
    let waits = format!("{}\n'primary saw it'", flag.awaited());
    k.send(json!({"t": "exec", "agent": PRIMARY, "id": 1, "src": waits}));
    let raises = format!("open({}, 'w').close()\n'sub raised it'", flag.py());
    k.send(json!({"t": "exec", "agent": "sub", "id": 1, "src": raises}));

    // Either may report first.
    let results = [k.recv(), k.recv()];
    let from = |agent: &str| {
        let result = results.iter().find(|m| m["agent"] == agent);
        result.unwrap_or_else(|| panic!("no result from {agent}: {results:?}"))
    };
    assert_eq!(from("sub")["output"], "'sub raised it'\n");
    assert_eq!(from(PRIMARY)["output"], "'primary saw it'\n");
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
