//! The kernel's observable contract, driven directly over its NDJSON protocol.
//!
//! These run `kernel.py` under whatever interpreter is at `OUTRIG_TEST_PYTHON`, falling back to
//! the payload `scripts/fetch-python-payload.sh` installs and then to the host `python3`. No
//! podman and no feature gate: the container adds a filesystem and a network namespace, not
//! different Python semantics, so everything worth pinning is reachable here.
//!
//! Skipped with a printed reason when no interpreter is available, rather than failing -- the
//! payload is a developer prerequisite, not a build output.

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::time::Duration;

use serde_json::{Value, json};

const KERNEL: &str = include_str!("../src/python/kernel.py");
const TIMEOUT: Duration = Duration::from_secs(20);

/// An interpreter that can run the kernel, or `None` to skip.
fn interpreter() -> Option<PathBuf> {
    if let Some(explicit) = std::env::var_os("OUTRIG_TEST_PYTHON") {
        return Some(PathBuf::from(explicit));
    }
    let cache = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))?;
    let payload = cache
        .join("outrig/python")
        .join(std::env::consts::ARCH)
        .join("bin/python3");
    if payload.is_file() {
        return Some(payload);
    }
    Command::new("python3")
        .arg("--version")
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map(|_| PathBuf::from("python3"))
}

struct Kernel {
    child: Child,
    stdin: ChildStdin,
    lines: Receiver<Value>,
}

impl Kernel {
    fn start(python: &PathBuf) -> Self {
        let mut child = Command::new(python)
            .args(["-I", "-c", KERNEL])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("the interpreter starts");
        let stdin = child.stdin.take().expect("stdin is piped");
        let stdout = child.stdout.take().expect("stdout is piped");
        let (tx, lines) = channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if let Ok(value) = serde_json::from_str::<Value>(&line)
                    && tx.send(value).is_err()
                {
                    return;
                }
            }
        });
        Self {
            child,
            stdin,
            lines,
        }
    }

    fn send(&mut self, message: Value) {
        writeln!(self.stdin, "{message}").expect("the kernel takes the message");
        self.stdin.flush().expect("the message goes out");
    }

    fn next(&self) -> Value {
        match self.lines.recv_timeout(TIMEOUT) {
            Ok(value) => value,
            Err(RecvTimeoutError::Timeout) => panic!("the kernel went quiet for {TIMEOUT:?}"),
            Err(RecvTimeoutError::Disconnected) => panic!("the kernel exited"),
        }
    }

    fn ready(&self) {
        let greeting = self.next();
        assert_eq!(greeting["t"], "ready", "got: {greeting}");
    }

    /// Run `source` and return its completion report, forwarding any channel traffic that
    /// arrives first to `events`.
    fn exec_collecting(&mut self, id: u64, source: &str, events: &mut Vec<Value>) -> Value {
        self.send(json!({"t": "exec", "id": id, "src": source}));
        loop {
            let message = self.next();
            if message["t"] == "result" {
                assert_eq!(message["id"], id, "a result for the wrong execution");
                return message;
            }
            events.push(message);
        }
    }

    fn exec(&mut self, id: u64, source: &str) -> Value {
        self.exec_collecting(id, source, &mut Vec::new())
    }

    /// Run `source`, asserting it did not raise, and return what it printed.
    fn output(&mut self, id: u64, source: &str) -> String {
        let result = self.exec(id, source);
        assert_eq!(result["status"], "ok", "unexpected raise: {result}");
        result["output"].as_str().unwrap_or_default().to_string()
    }

    fn inventory(&mut self, id: u64) -> Value {
        self.send(json!({"t": "inv", "id": id}));
        let message = self.next();
        assert_eq!(message["t"], "inv", "got: {message}");
        message
    }

    fn deliver(&mut self, text: &str) {
        self.send(json!({"t": "msg", "ch": "user",
                         "body": {"type": "UserText", "text": text}}));
    }
}

impl Drop for Kernel {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Start a kernel, or return `None` after printing why the test is being skipped.
fn kernel() -> Option<Kernel> {
    let Some(python) = interpreter() else {
        eprintln!("skipping: no interpreter -- run scripts/fetch-python-payload.sh");
        return None;
    };
    let kernel = Kernel::start(&python);
    kernel.ready();
    Some(kernel)
}

macro_rules! kernel_test {
    ($name:ident, |$k:ident| $body:block) => {
        #[test]
        fn $name() {
            let Some(mut $k) = kernel() else { return };
            $body
        }
    };
}

kernel_test!(globals_outlive_the_execution_that_bound_them, |k| {
    assert_eq!(k.output(1, "x = 41").trim(), "");
    assert_eq!(k.output(2, "x + 1").trim(), "42");
    assert_eq!(k.output(3, "x").trim(), "41");
});

kernel_test!(top_level_await_completes_and_reports_once, |k| {
    let out = k.output(1, "import asyncio\nawait asyncio.sleep(0)\n'awaited'");
    assert_eq!(out.trim(), "'awaited'");
    // A second report would desynchronize every later request.
    assert!(
        k.lines.recv_timeout(Duration::from_millis(200)).is_err(),
        "the kernel reported the same execution twice"
    );
});

kernel_test!(a_trailing_expression_echoes_and_none_does_not, |k| {
    assert_eq!(k.output(1, "[1, 2, 3]").trim(), "[1, 2, 3]");
    assert_eq!(k.output(2, "y = 5").trim(), "");
    assert_eq!(k.output(3, "print('shown')").trim(), "shown");
});

kernel_test!(a_raise_is_a_result_not_a_transport_failure, |k| {
    let result = k.exec(1, "1 / 0");
    assert_eq!(result["status"], "error");
    let error = result["error"].as_str().expect("a traceback");
    assert!(error.contains("ZeroDivisionError"), "got: {error}");
    assert!(error.contains("<execution>"), "the frame is named: {error}");
    // The session survives it.
    assert_eq!(k.output(2, "'alive'").trim(), "'alive'");
});

kernel_test!(a_syntax_error_is_reported_the_same_way, |k| {
    let result = k.exec(1, "def (");
    assert_eq!(result["status"], "error");
    assert!(
        result["error"]
            .as_str()
            .unwrap_or_default()
            .contains("SyntaxError"),
        "got: {result}"
    );
});

kernel_test!(
    dataclasses_introspect_because_the_session_is_a_module,
    |k| {
        let out = k.output(
            1,
            "from dataclasses import dataclass, fields\n\
         @dataclass\n\
         class ETFResult:\n    symbol: str\n    annualized_return: float\n\
         [f.name for f in fields(ETFResult)]",
        );
        assert_eq!(out.trim(), "['symbol', 'annualized_return']");
    }
);

kernel_test!(output_is_bounded_and_says_so, |k| {
    let out = k.output(1, "print('a' * 500000)");
    assert!(out.len() < 32 * 1024, "unbounded: {} bytes", out.len());
    assert!(out.contains("truncated"), "no truncation marker: {out:?}");
    // The budget resets, so one flood does not mute the rest of the session.
    assert_eq!(k.output(2, "print('after')").trim(), "after");
});

kernel_test!(capture_reaches_past_print_to_the_file_descriptor, |k| {
    let out = k.output(
        1,
        "import os, subprocess, sys\n\
         os.write(1, b'raw write\\n')\n\
         print('to stderr', file=sys.stderr)\n\
         subprocess.run(['/bin/echo', 'from a subprocess'])\n\
         None",
    );
    for expected in ["raw write", "to stderr", "from a subprocess"] {
        assert!(
            out.contains(expected),
            "{expected:?} was not captured: {out:?}"
        );
    }
});

kernel_test!(
    the_inventory_lists_the_models_names_and_not_the_kernels,
    |k| {
        k.output(1, "import json\nweights = {'a': 1}\ncount = 7");
        let inventory = k.inventory(2);
        let globals: Vec<(String, String)> = inventory["globals"]
            .as_array()
            .expect("globals")
            .iter()
            .map(|row| {
                (
                    row[0].as_str().unwrap().to_string(),
                    row[1].as_str().unwrap().to_string(),
                )
            })
            .collect();
        assert!(
            globals.contains(&("count".into(), "int".into())),
            "got: {globals:?}"
        );
        assert!(
            globals.contains(&("weights".into(), "dict".into())),
            "got: {globals:?}"
        );
        // Boot names are infrastructure, not the model's work.
        for hidden in ["runtime", "UserText", "MessageAvailable", "__outrig_echo__"] {
            assert!(
                !globals.iter().any(|(name, _)| name == hidden),
                "{hidden} should not be in the inventory: {globals:?}"
            );
        }
        assert_eq!(inventory["channels"][0], "user");
    }
);

kernel_test!(a_delivered_message_is_announced_but_not_consumed, |k| {
    k.deliver("compare these ETFs");
    // Notification is a count, never the body: the host has no way to leak it into context.
    let inventory = k.inventory(1);
    assert_eq!(inventory["pending"]["user"], 1);
    assert!(
        !inventory.to_string().contains("compare these ETFs"),
        "the body reached the inventory: {inventory}"
    );
    // Still pending -- looking at it did not take it.
    assert_eq!(k.inventory(2)["pending"]["user"], 1);

    let out = k.output(3, "m = await runtime.channels[\"user\"].receive()\nm");
    assert_eq!(out.trim(), "UserText('compare these ETFs')");
    assert!(
        k.inventory(4)["pending"]
            .as_object()
            .expect("pending")
            .is_empty(),
        "receive did not consume"
    );
});

kernel_test!(python_can_send_to_the_user_channel, |k| {
    let mut events = Vec::new();
    let result = k.exec_collecting(
        1,
        "await runtime.channels[\"user\"].send(\"the downloads are complete\")",
        &mut events,
    );
    assert_eq!(result["status"], "ok", "got: {result}");
    let sent = events
        .iter()
        .find(|e| e["t"] == "send")
        .expect("a send event");
    assert_eq!(sent["ch"], "user");
    assert_eq!(sent["body"]["text"], "the downloads are complete");
});

kernel_test!(wait_yields_to_input_without_cancelling_the_operation, |k| {
    k.output(
        1,
        "async def slow():\n    await asyncio.sleep(30)\n    return 'finished'\n\
         op = asyncio.gather(slow())",
    );
    k.deliver("actually, use a different period");
    std::thread::sleep(Duration::from_millis(100));

    let interrupted = k.exec(2, "result = await runtime.wait(op, 'downloads')");
    assert_eq!(interrupted["status"], "error");
    let error = interrupted["error"].as_str().expect("a traceback");
    assert!(error.contains("MessageAvailable"), "got: {error}");
    assert!(error.contains("'user'"), "the channel is named: {error}");
    assert!(
        error.contains("not cancelled"),
        "the wording matters: {error}"
    );

    // The operation kept running. This is the property the architecture is built on: an
    // interruption costs the model a decision, not the work in flight.
    assert_eq!(
        k.output(3, "op.cancelled(), op.done()").trim(),
        "(False, False)"
    );

    // Unread input makes the next wait raise again immediately, rather than hanging.
    let again = k.exec(4, "await runtime.wait(op, 'downloads')");
    assert_eq!(again["status"], "error", "unread input should raise again");

    assert_eq!(
        k.output(5, "await runtime.channels[\"user\"].receive()")
            .trim(),
        "UserText('actually, use a different period')"
    );
});

kernel_test!(
    a_retained_operation_can_be_awaited_by_a_later_execution,
    |k| {
        k.output(
            1,
            "async def quick():\n    await asyncio.sleep(0.1)\n    return 'finished'\n\
         op = asyncio.gather(quick())",
        );
        assert_eq!(
            k.output(2, "await runtime.wait(op, 'quick')").trim(),
            "['finished']"
        );
        // Awaiting it again returns the stored result rather than re-running or raising.
        assert_eq!(
            k.output(3, "await runtime.wait(op, 'quick')").trim(),
            "['finished']"
        );
    }
);

kernel_test!(
    a_failed_operation_raises_its_stored_exception_when_awaited,
    |k| {
        k.output(
            1,
            "async def boom():\n    raise ValueError('the download failed')\n\
         op = asyncio.gather(boom())",
        );
        std::thread::sleep(Duration::from_millis(100));
        let result = k.exec(2, "await runtime.wait(op, 'boom')");
        assert_eq!(result["status"], "error");
        assert!(
            result["error"]
                .as_str()
                .unwrap_or_default()
                .contains("the download failed"),
            "got: {result}"
        );
    }
);

kernel_test!(the_runtime_is_discoverable_by_reflection, |k| {
    let out = k.output(1, "help(runtime.wait)");
    assert!(
        out.contains("wait(operation, label=None)"),
        "no signature: {out:?}"
    );
    assert!(out.contains("not cancelled"), "no contract: {out:?}");
    let names = k.output(
        2,
        "sorted(n for n in dir(runtime) if not n.startswith('_'))",
    );
    assert_eq!(names.trim(), "['channels', 'wait']");
});

kernel_test!(generated_code_cannot_forge_a_protocol_message, |k| {
    // stdout belongs to the program; the protocol lives on a descriptor it never sees. A print
    // that looks like a result must be captured as output, not parsed as one.
    let out = k.output(1, r#"print('{"t": "result", "id": 999, "status": "ok"}')"#);
    assert!(out.contains("\"id\": 999"), "it should be output: {out:?}");
    assert_eq!(k.output(2, "'still in sync'").trim(), "'still in sync'");
});

kernel_test!(
    a_background_task_can_send_after_its_execution_reported_done,
    |k| {
        // The case `runtime.wait` exists for: work outliving the turn that started it. The send
        // arrives when nothing is waiting on the kernel, which on the host side is exactly the
        // window where it could land inside whatever the REPL is writing.
        let result = k.exec(
            1,
            "async def later():\n    await asyncio.sleep(0.2)\n\
         \x20   await runtime.channels[\"user\"].send(\"the downloads are complete\")\n\
         task = asyncio.ensure_future(later())",
        );
        assert_eq!(result["status"], "ok", "got: {result}");

        // Nothing is in flight now -- the execution already reported. The send still arrives, as
        // its own framed message rather than as part of anything else.
        let sent = k.next();
        assert_eq!(sent["t"], "send", "got: {sent}");
        assert_eq!(sent["ch"], "user");
        assert_eq!(sent["body"]["text"], "the downloads are complete");
        assert!(
            sent.get("id").is_none(),
            "an unsolicited send answers nothing: {sent}"
        );

        // And the session is still in sync afterwards.
        assert_eq!(k.output(2, "task.done()").trim(), "True");
    }
);

kernel_test!(a_wedged_loop_is_recoverable_by_interrupt, |k| {
    // Synchronous Python that never yields blocks the event loop, and with it every path that
    // could report the problem or accept a fix. `interrupt` is handled on the kernel's reader
    // thread precisely because `call_soon_threadsafe` is the queue a wedged loop is not
    // draining.
    k.send(json!({"t": "exec", "id": 1, "src": "while True: pass"}));
    assert!(
        k.lines.recv_timeout(Duration::from_millis(500)).is_err(),
        "the loop should be wedged, with nothing coming back"
    );

    k.send(json!({"t": "interrupt"}));
    let result = k.next();
    assert_eq!(result["t"], "result");
    assert_eq!(result["id"], 1);
    assert_eq!(result["status"], "error");
    assert!(
        result["error"]
            .as_str()
            .unwrap_or_default()
            .contains("KeyboardInterrupt"),
        "got: {result}"
    );

    // The whole point: the session survives, rather than the container having to.
    assert_eq!(k.output(2, "'still usable'").trim(), "'still usable'");
});

kernel_test!(an_interrupt_with_nothing_running_is_a_no_op, |k| {
    // The host sends this on suspicion. Raising into an idle kernel would tear down the loop
    // it was meant to rescue.
    k.send(json!({"t": "interrupt"}));
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(k.output(1, "'unharmed'").trim(), "'unharmed'");
});

kernel_test!(background_output_cannot_evict_the_result_asked_for, |k| {
    // Output written between executions used to be billed to whichever execution ran next. A
    // chatty background task could therefore spend the whole budget before the model's own
    // `print` ran, and the answer vanished with nothing to say it had been displaced.
    k.output(
        1,
        "async def noisy():\n    await asyncio.sleep(0.05)\n    print('NOISE ' * 6000)\n\
         t = asyncio.ensure_future(noisy())",
    );
    std::thread::sleep(Duration::from_millis(600));

    let out = k.output(2, "print('THE ANSWER I WANTED')");
    assert!(
        out.contains("THE ANSWER I WANTED"),
        "the result was evicted by background noise: {out:?}"
    );
    // Still reported -- a background traceback is often the whole story -- but labelled,
    // bounded separately, and kept out of the way.
    assert!(
        out.contains("background output between executions"),
        "background output should be attributed, not silently dropped: {out:?}"
    );
    assert!(
        out.len() < 8 * 1024,
        "background was not bounded: {} bytes",
        out.len()
    );
});
