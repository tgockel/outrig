//! `outrig run-new` end to end: the binary, a real podman, an image with no
//! Python in it, and a scripted Anthropic endpoint standing in for the model.
//!
//! Sessions are driven the way a person drives them -- a line, the reply, a
//! Ctrl-C at the prompt, a line whose Python never finishes and a Ctrl-C to
//! stop it, a line typed while the agent's code is still running -- and
//! everything is asserted from outside: what reached the model, what the
//! terminal showed, and what the session left on disk. Each Ctrl-C is sent to
//! outrig's whole process group, as a terminal sends it, so it reaches every
//! child outrig did not move out of the way.
//!
//! The model's own text is commentary, on stderr; stdout carries only what the
//! agent sends the user on its channel.

#![cfg(feature = "e2e")]

mod common;

use std::path::Path;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::task::JoinHandle;
use tokio::time::timeout;

use common::{CannedResponse, RecordedRequest, drain_recorded, start_mock_http, stream_lines};

const TEST_TIMEOUT: Duration = Duration::from_secs(300);
/// How long a step has once the session is up. Short, so a line that never
/// reaches the agent fails the test rather than waiting out the whole budget.
const STEP_TIMEOUT: Duration = Duration::from_secs(30);
const KEY_VAR: &str = "OUTRIG_TEST_RUN_NEW_KEY";
/// Named by the primary MCP server's `env` and never set: starting that server
/// would fail on resolving it.
const SECRET_VAR: &str = "OUTRIG_TEST_RUN_NEW_UNSET_SECRET";

fn message(content: Value, stop_reason: &str) -> CannedResponse {
    CannedResponse::ok(json!({
        "type": "message",
        "id": "msg_mock",
        "model": "claude-sonnet-4-6",
        "role": "assistant",
        "stop_reason": stop_reason,
        "stop_sequence": null,
        "content": content,
        "usage": { "input_tokens": 12, "output_tokens": 7 },
    }))
}

fn text_reply(text: &str) -> CannedResponse {
    message(json!([{ "type": "text", "text": text }]), "end_turn")
}

fn submit(id: &str, source: &str) -> CannedResponse {
    message(
        json!([{
            "type": "tool_use",
            "id": id,
            "name": "submit_python",
            "input": { "source": source },
        }]),
        "tool_use",
    )
}

/// The text of the `tool_result` for `id` in `request`.
fn tool_result(request: &RecordedRequest, id: &str) -> String {
    let body = &request.body;
    let block = body["messages"]
        .as_array()
        .expect("a messages array")
        .iter()
        .filter_map(|message| message["content"].as_array())
        .flatten()
        .find(|block| block["type"] == "tool_result" && block["tool_use_id"] == id)
        .unwrap_or_else(|| panic!("no tool_result for {id} in {body:#}"));
    block["content"]
        .as_array()
        .expect("tool_result content blocks")
        .iter()
        .map(|part| part["text"].as_str().expect("a text block"))
        .collect()
}

/// A repo whose image has no Python and whose config declares a primary MCP
/// server carrying a secret.
fn repo(addr: std::net::SocketAddr) -> tempfile::TempDir {
    repo_with(addr, "")
}

/// [`repo`], its config ending with `extra`.
fn repo_with(addr: std::net::SocketAddr, extra: &str) -> tempfile::TempDir {
    let repo = tempfile::tempdir().expect("a repo");
    let cfg_dir = repo.path().join(".agents/outrig");
    std::fs::create_dir_all(&cfg_dir).expect("create config dir");
    std::fs::write(
        cfg_dir.join("config.toml"),
        format!(
            r#"
default-model = "sonnet"
default-image = "primary"

[providers.claude]
style                = "anthropic"
base-url             = "http://{addr}"
api-key              = "${{{KEY_VAR}}}"
request-timeout-secs = 30

[models.sonnet]
provider   = "claude"
identifier = "claude-sonnet-4-6"
max-tokens = 4096

[images.primary]
image-name = "docker.io/library/alpine:latest"

  [images.primary.mcp]
  leaky = {{ command = ["/nonexistent-mcp"], env = {{ TOKEN = "${{{SECRET_VAR}}}" }} }}

{extra}
"#
        ),
    )
    .expect("write config");
    repo
}

/// A running `outrig run-new`, its output mirrored into sinks as it arrives,
/// so a hang or an early exit shows why.
struct Session {
    child: Child,
    pid: String,
    stdin: ChildStdin,
    stdout: Arc<Mutex<String>>,
    stderr: Arc<Mutex<String>>,
    streams: [JoinHandle<()>; 2],
}

impl Session {
    fn start(repo: &Path, sessions: &Path) -> Self {
        Self::spawn(repo, sessions, None)
    }

    /// With `stdout_pause`, stdout is read a line at a time with that pause
    /// after each: a terminal slower than the agent writing to it.
    fn spawn(repo: &Path, sessions: &Path, stdout_pause: Option<Duration>) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_outrig"))
            .args(["--global-config"])
            .arg(repo.join("no-such-global.toml"))
            .arg("--session-root")
            .arg(sessions)
            .arg("run-new")
            .current_dir(repo)
            .env(KEY_VAR, "sk-ant-mock-key")
            .env_remove(SECRET_VAR)
            .env("OUTRIG_LOG", "info")
            .env("NO_PROXY", "127.0.0.1,localhost")
            .env("no_proxy", "127.0.0.1,localhost")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            // A group of its own, led by outrig, as a shell's foreground job is.
            .process_group(0)
            .spawn()
            .expect("spawn outrig");
        let pid = child.id().expect("a pid").to_string();
        let stdin = child.stdin.take().expect("stdin");
        let stdout = Arc::new(Mutex::new(String::new()));
        let stderr = Arc::new(Mutex::new(String::new()));
        let child_stdout = child.stdout.take().expect("stdout");
        let streams = [
            match stdout_pause {
                None => tokio::spawn(stream_lines(child_stdout, Arc::clone(&stdout), "stdout")),
                Some(pause) => tokio::spawn(read_slowly(child_stdout, Arc::clone(&stdout), pause)),
            },
            tokio::spawn(stream_lines(
                child.stderr.take().expect("stderr"),
                Arc::clone(&stderr),
                "stderr",
            )),
        ];
        Self {
            child,
            pid,
            stdin,
            stdout,
            stderr,
            streams,
        }
    }

    async fn type_line(&mut self, line: &str) {
        self.stdin
            .write_all(format!("{line}\n").as_bytes())
            .await
            .expect("typed");
    }

    /// Wait for outrig to exit, and return how, with what each stream carried.
    /// With `close_input`, input ends first, as Ctrl-D ends it; otherwise it
    /// stays open, as a terminal's does, and outrig has to exit on its own.
    async fn exit(self, close_input: bool) -> (std::process::ExitStatus, String, String) {
        let Self {
            mut child,
            stdin,
            stdout,
            stderr,
            streams,
            ..
        } = self;
        let open = (!close_input).then_some(stdin);
        let status = timeout(STEP_TIMEOUT, child.wait())
            .await
            .expect("exits")
            .expect("wait");
        drop(open);
        for stream in streams {
            stream.await.expect("drained");
        }
        let text = |sink: Arc<Mutex<String>>| sink.lock().expect("unpoisoned").clone();
        (status, text(stdout), text(stderr))
    }
}

/// Read `stdout` into `sink` a line at a time, pausing after each.
async fn read_slowly(stdout: ChildStdout, sink: Arc<Mutex<String>>, pause: Duration) {
    let mut lines = BufReader::new(stdout).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        {
            let mut sink = sink.lock().expect("unpoisoned");
            sink.push_str(&line);
            sink.push('\n');
        }
        tokio::time::sleep(pause).await;
    }
}

/// Wait up to `within` for `done`, which `what` names.
async fn wait_until(within: Duration, what: &str, done: impl Fn() -> bool) {
    timeout(within, async {
        while !done() {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("no {what} within {within:?}"));
}

/// Wait up to `within` for `sink` to hold `want`.
async fn wait_for(sink: &Mutex<String>, want: &str, within: Duration) {
    let holds = || sink.lock().expect("unpoisoned").contains(want);
    wait_until(within, &format!("{want:?}"), holds).await;
}

/// Wait up to `within` for `path` to exist.
async fn wait_for_file(path: &Path, within: Duration) {
    wait_until(within, &path.display().to_string(), || path.exists()).await;
}

fn podman_names(filter: &str) -> String {
    let out = std::process::Command::new("podman")
        .args(["ps", "-a", "--format", "{{.Names}}", "--filter"])
        .arg(filter)
        .output()
        .expect("podman ps");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// Send SIGINT to the process group `leader` leads, as a terminal's Ctrl-C
/// does.
fn ctrl_c(leader: &str) {
    let kill = std::process::Command::new("kill")
        .args(["-INT", "--", &format!("-{leader}")])
        .status()
        .expect("kill -INT");
    assert!(kill.success());
}

/// SIGKILL `pid` alone: outrig dying with no chance to clean up. The container
/// is podman's, not a child, so it keeps running.
fn sigkill(pid: &str) {
    let kill = std::process::Command::new("kill")
        .args(["-KILL", "--", pid])
        .status()
        .expect("kill -KILL");
    assert!(kill.success());
}

/// The id the banner named: the word after `[outrig] session id: `, ahead of
/// the hint in parentheses.
fn banner_session_id(stderr: &Mutex<String>) -> String {
    stderr
        .lock()
        .expect("unpoisoned")
        .lines()
        .find_map(|line| line.strip_prefix("[outrig] session id: "))
        .and_then(|rest| rest.split_whitespace().next())
        .expect("the banner names the session")
        .to_string()
}

/// `podman rm -f` of a container on drop, so an assertion that fails between
/// a kill and the tidy-up leaves nothing behind.
struct RemoveOnDrop(String);

impl Drop for RemoveOnDrop {
    fn drop(&mut self) {
        let _ = std::process::Command::new("podman")
            .args(["rm", "-f", &self.0])
            .output();
    }
}

/// The phase's premise, through the binary: a name bound in one round is still
/// bound in the rounds after. Between them, a Ctrl-C at the prompt returns to
/// the prompt, and a Ctrl-C during an `await` that would never finish stops
/// it, the model reads how it ended, and the interpreter is still there.
/// The primary MCP server holding a secret is not started -- asserted on
/// startup, since starting it would have failed the launch -- and the session
/// is recorded under the container it really ran in.
#[tokio::test]
async fn names_survive_rounds_and_ctrl_c_stops_python_not_the_session() {
    let (addr, mut requests) = start_mock_http(vec![
        submit("toolu_1", "import os\nx = 41\nprint(os.getcwd())"),
        text_reply("bound x"),
        submit(
            "toolu_wait",
            "open('running', 'w').close()\nawait asyncio.get_running_loop().create_future()",
        ),
        text_reply("stopped the wait"),
        submit("toolu_2", "print(x + 1)"),
        text_reply("x + 1 is 42"),
    ])
    .await;
    let repo = repo(addr);
    let sessions = tempfile::tempdir().expect("a session root");

    let mut session = Session::start(repo.path(), sessions.path());

    session.type_line("bind x").await;
    wait_for(&session.stderr, "bound x", TEST_TIMEOUT).await;

    // At the prompt now. One Ctrl-C there is a fresh line, not an exit.
    ctrl_c(&session.pid);
    tokio::time::sleep(Duration::from_millis(500)).await;

    // A wait that will never finish, stopped once it is running.
    session.type_line("wait").await;
    wait_for_file(&repo.path().join("running"), STEP_TIMEOUT).await;
    ctrl_c(&session.pid);
    wait_for(&session.stderr, "stopped the wait", STEP_TIMEOUT).await;

    session.type_line("use x").await;
    wait_for(&session.stderr, "x + 1 is 42", STEP_TIMEOUT).await;

    let (status, stdout, stderr) = session.exit(true).await;
    assert!(status.success(), "{status}");
    assert_eq!(
        stdout, "",
        "the agent sent nothing, and its commentary is on stderr"
    );

    // Startup: nothing MCP started, and Python came up.
    assert!(
        stderr.contains("[outrig] run-new starts no MCP servers; not started: leaky"),
        "{stderr}"
    );
    assert!(
        stderr.contains("[outrig] model:         sonnet"),
        "{stderr}"
    );
    assert!(stderr.contains("[outrig] python 3."), "{stderr}");
    // What the person watched run.
    assert!(
        stderr.contains("[outrig] python:\n    import os\n    x = 41"),
        "{stderr}"
    );

    assert!(
        stderr.contains("[outrig] stopping execution"),
        "the Ctrl-C reached the Python: {stderr}"
    );

    let recorded = drain_recorded(&mut requests);
    assert_eq!(recorded.len(), 6, "two model calls per round");
    assert_eq!(
        tool_result(&recorded[1], "toolu_1"),
        "/workspace\n",
        "the Python ran against the workspace"
    );
    let stopped = tool_result(&recorded[3], "toolu_wait");
    assert!(
        stopped.starts_with("[the user interrupted this call")
            && stopped.contains("CancelledError"),
        "{stopped}"
    );
    assert_eq!(
        tool_result(&recorded[5], "toolu_2"),
        "42\n",
        "the name the first round bound is still bound"
    );

    // Recorded under the container it ran in, ended cleanly, and reaped.
    let dirs: Vec<_> = std::fs::read_dir(sessions.path())
        .expect("the session root")
        .map(|entry| entry.expect("an entry").path())
        .collect();
    assert_eq!(dirs.len(), 1, "{dirs:?}");
    let record: Value = serde_json::from_str(
        &std::fs::read_to_string(dirs[0].join("session.json")).expect("a record"),
    )
    .expect("the record parses");
    let container = record["container_name"].as_str().expect("a name");
    let id = record["id"].as_str().expect("an id");
    assert_eq!(container, format!("outrig-{id}"), "{record:#}");
    assert!(
        stderr.contains(&format!("ready in {container}")),
        "{stderr}"
    );
    assert_eq!(record["exit_code"], 0, "{record:#}");
    assert!(
        podman_names(&format!("name={container}")).is_empty(),
        "the container is gone"
    );
    assert!(dirs[0].join("logs").is_dir());
}

/// The last user message `request` carried, as text.
fn last_user_text(request: &RecordedRequest) -> String {
    let message = request.body["messages"]
        .as_array()
        .expect("a messages array")
        .iter()
        .rev()
        .find(|message| message["role"] == "user")
        .expect("a user message");
    match &message["content"] {
        Value::String(text) => text.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(|block| block["text"].as_str())
            .collect(),
        other => panic!("no user text: {other}"),
    }
}

/// A typed line is a message the agent's code reads, not a prompt the model
/// reads: the model is told one is waiting, and nothing it is sent carries what
/// the user typed. A second line typed while the first round's code is still
/// running -- no Ctrl-C -- reaches that same code, which is only possible if
/// input is read while a round runs. What the agent sends back is what lands on
/// stdout, including a send from a task that outlived its round.
///
/// Two Ctrl-Cs at the prompt then end the session with input still open: the
/// read always in flight does not hold the process open.
#[tokio::test]
async fn typed_input_reaches_the_agent_mid_round_and_its_sends_reach_the_terminal() {
    let (addr, mut requests) = start_mock_http(vec![
        submit(
            "toolu_read",
            "ch = runtime.channels['user']\nfirst = await ch.receive()\n\
             open('reading', 'w').close()\nsecond = await ch.receive()\n\
             await ch.send(first.body + '|' + second.body)",
        ),
        text_reply("read both"),
        submit(
            "toolu_later",
            "import os\nasync def later():\n    while not os.path.exists('release'):\n        \
             await asyncio.sleep(0.05)\n    await runtime.channels['user'].send('done later')\n\
             task = asyncio.create_task(later())",
        ),
        text_reply("started it"),
    ])
    .await;
    // Recorded, so the event log is checked through the binary too.
    let repo = repo_with(addr, "[events]\nmode = \"record\"");
    let sessions = tempfile::tempdir().expect("a session root");
    let mut session = Session::start(repo.path(), sessions.path());

    session.type_line("alpha-7").await;
    wait_for_file(&repo.path().join("reading"), TEST_TIMEOUT).await;
    // The first round's code is waiting on its second receive.
    session.type_line("bravo-7").await;
    wait_for(&session.stdout, "alpha-7|bravo-7\n", STEP_TIMEOUT).await;
    wait_for(&session.stderr, "read both", STEP_TIMEOUT).await;

    session.type_line("charlie-7").await;
    wait_for(&session.stderr, "started it", STEP_TIMEOUT).await;
    std::fs::write(repo.path().join("release"), b"").expect("release the task");
    wait_for(&session.stdout, "done later\n", STEP_TIMEOUT).await;

    ctrl_c(&session.pid);
    tokio::time::sleep(Duration::from_millis(500)).await;
    ctrl_c(&session.pid);
    let (status, stdout, stderr) = session.exit(false).await;
    assert!(status.success(), "{status}");
    assert_eq!(
        stdout, "alpha-7|bravo-7\ndone later\n",
        "stdout is the agent's sends alone"
    );
    assert!(
        stderr.contains("[outrig] queued for the agent (1 waiting)"),
        "the line typed mid-round was queued, and said so: {stderr}"
    );

    // Two model calls per round and no more: the mock repeats its last answer,
    // so only the count would show a round the second line started.
    let recorded = drain_recorded(&mut requests);
    assert_eq!(recorded.len(), 4, "{recorded:#?}");
    for opening in [&recorded[0], &recorded[2]] {
        assert_eq!(
            last_user_text(opening),
            "[outrig] 1 message is waiting on runtime.channels[\"user\"]."
        );
    }
    let wire: String = recorded.iter().map(|r| r.body.to_string()).collect();
    for typed in ["alpha-7", "bravo-7", "charlie-7"] {
        assert!(!wire.contains(typed), "{typed} reached the model");
    }

    // The event log is whole by the time the binary has exited, holds what
    // the user typed, and is the user's alone.
    let session_dir = std::fs::read_dir(sessions.path())
        .expect("the session root")
        .next()
        .expect("one session")
        .expect("an entry")
        .path();
    let log = session_dir.join("logs/events.jsonl");
    let text = std::fs::read_to_string(&log).expect("the event log");
    let events: Vec<Value> = text
        .lines()
        .map(|line| serde_json::from_str(line).expect("a whole record"))
        .collect();
    assert_eq!(
        events.last().map(|e| e["type"].clone()),
        Some(json!("org.outrig.agent.stopped")),
        "{text}"
    );
    // Under the session's own id, the one `session.json` and `outrig ls` show.
    let record: Value = serde_json::from_str(
        &std::fs::read_to_string(session_dir.join("session.json")).expect("a record"),
    )
    .expect("the record parses");
    let id = record["id"].as_str().expect("an id");
    assert_eq!(
        record["container_name"],
        json!(format!("outrig-{id}")),
        "{record:#}"
    );
    let source = json!(format!("/outrig/session/{id}"));
    assert!(
        events.iter().all(|e| e["source"] == source),
        "every record names the session by its id: {text}"
    );
    let typed: Vec<&Value> = events
        .iter()
        .filter(|e| e["type"] == "org.outrig.message.sent" && e["data"]["from"] == "user")
        .map(|e| &e["data"]["body"])
        .collect();
    assert_eq!(
        typed,
        [&json!("alpha-7"), &json!("bravo-7"), &json!("charlie-7")]
    );
    use std::os::unix::fs::PermissionsExt as _;
    let mode = std::fs::metadata(&log).expect("stat").permissions().mode();
    assert_eq!(mode & 0o777, 0o600);
}

/// What `runtime.wait` is for, through the binary: a wait on something that
/// will never finish yields to a line typed while it waits, the operation it
/// was waiting on is still running afterwards, and the model carries on in the
/// same round.
#[tokio::test]
async fn a_typed_line_ends_a_wait_and_the_round_goes_on() {
    let (addr, mut requests) = start_mock_http(vec![
        submit(
            "toolu_wait",
            "await runtime.channels['user'].receive()\n\
             forever = asyncio.create_task(asyncio.Event().wait(), name='forever')\n\
             asyncio.get_running_loop().call_soon(lambda: open('waiting', 'w').close())\n\
             done, pending = await runtime.wait({forever})",
        ),
        submit(
            "toolu_read",
            "print((await runtime.channels['user'].receive()).body, forever.done())",
        ),
        text_reply("redirected"),
    ])
    .await;
    let repo = repo(addr);
    let sessions = tempfile::tempdir().expect("a session root");
    let mut session = Session::start(repo.path(), sessions.path());

    session.type_line("alpha-9").await;
    // Created once the wait has suspended.
    wait_for_file(&repo.path().join("waiting"), TEST_TIMEOUT).await;
    session.type_line("bravo-9").await;
    wait_for(&session.stderr, "redirected", STEP_TIMEOUT).await;

    let (status, stdout, stderr) = session.exit(true).await;
    assert!(status.success(), "{status}: {stderr}");
    assert_eq!(stdout, "", "the agent sent nothing");
    assert!(
        stderr.contains("[outrig] queued for the agent (1 waiting)"),
        "the line typed mid-wait was queued, and said so: {stderr}"
    );

    // One round, three model calls: the mock repeats its last answer, so only
    // the count would show a second round.
    let recorded = drain_recorded(&mut requests);
    assert_eq!(recorded.len(), 3, "{recorded:#?}");
    let waited = tool_result(&recorded[1], "toolu_wait");
    assert!(
        waited.starts_with(
            "[1 message is waiting on runtime.channels[\"user\"]]\n[this code raised \
             MessageAvailable: input is waiting on runtime.channels[\"user\"]"
        ),
        "{waited}"
    );
    assert_eq!(
        tool_result(&recorded[2], "toolu_read"),
        "bravo-9 False\n",
        "the second line reached the code, and the operation outlived the wait"
    );
}

/// Once the interpreter has exited, nothing typed could reach the agent, so
/// the session ends -- with input still open -- and says why.
#[tokio::test]
async fn the_session_ends_when_the_interpreter_exits() {
    let (addr, _requests) = start_mock_http(vec![
        submit("toolu_exit", "import os\nos._exit(3)"),
        text_reply("it is gone"),
    ])
    .await;
    let repo = repo(addr);
    let sessions = tempfile::tempdir().expect("a session root");
    let mut session = Session::start(repo.path(), sessions.path());

    session.type_line("exit").await;
    let (status, _, stderr) = session.exit(false).await;
    assert_eq!(status.code(), Some(1), "{stderr}");
    assert!(
        stderr.contains("it is gone"),
        "the round finished first: {stderr}"
    );
    assert!(
        stderr.contains("[outrig] the Python interpreter exited; the session is over"),
        "{stderr}"
    );
}

/// A flood of messages from the agent does not crowd out the round or what
/// the user types. The terminal here is slower than the agent, so there is
/// always a message waiting to be written; the round still reports, and
/// `/quit` still ends the session.
#[tokio::test]
async fn a_flood_of_sends_does_not_starve_the_round_or_input() {
    let (addr, _requests) = start_mock_http(vec![
        submit(
            "toolu_flood",
            "async def flood():\n    while True:\n        \
             await runtime.channels['user'].send('tick ' + 'x' * 1000)\n\
             flooding = asyncio.create_task(flood())",
        ),
        text_reply("flooding"),
    ])
    .await;
    let repo = repo(addr);
    let sessions = tempfile::tempdir().expect("a session root");
    let mut session = Session::spawn(repo.path(), sessions.path(), Some(Duration::from_millis(1)));

    session.type_line("go").await;
    wait_for(&session.stdout, "tick ", TEST_TIMEOUT).await;
    wait_for(&session.stderr, "flooding", STEP_TIMEOUT).await;
    session.type_line("/quit").await;
    let (status, _, stderr) = session.exit(false).await;
    assert!(status.success(), "{status}: {stderr}");
}

/// Input ending is a graceful exit, and a graceful exit shows everything the
/// agent had already sent. Here a task the round left sends a burst, all of it
/// at once, to a terminal slower than the agent, and input ends while most of
/// the burst is still waiting to be shown; every message reaches stdout, in
/// order.
#[tokio::test]
async fn end_of_input_shows_everything_already_sent() {
    const BURST: usize = 12;
    let (addr, _requests) = start_mock_http(vec![
        submit(
            "toolu_burst",
            &format!(
                "import os\nasync def burst():\n    while not os.path.exists('release'):\n        \
                 await asyncio.sleep(0.01)\n    for i in range({BURST}):\n        \
                 await runtime.channels['user'].send(f'm{{i}} ' + 'x' * 32000)\n\
                 task = asyncio.create_task(burst())"
            ),
        ),
        text_reply("armed"),
    ])
    .await;
    let repo = repo(addr);
    let sessions = tempfile::tempdir().expect("a session root");
    let mut session = Session::spawn(
        repo.path(),
        sessions.path(),
        Some(Duration::from_millis(10)),
    );

    session.type_line("go").await;
    wait_for(&session.stderr, "armed", TEST_TIMEOUT).await;
    std::fs::write(repo.path().join("release"), b"").expect("release the burst");
    wait_for(&session.stdout, "m0 ", STEP_TIMEOUT).await;
    let (status, stdout, stderr) = session.exit(true).await;
    assert!(status.success(), "{status}: {stderr}");
    let shown: Vec<&str> = stdout
        .lines()
        .map(|line| line.split(' ').next().expect("a word"))
        .collect();
    let sent: Vec<String> = (0..BURST).map(|i| format!("m{i}")).collect();
    assert_eq!(shown, sent);
}

/// What a `kill -9` of outrig leaves (#469): a container named `outrig-<sid>`
/// and labeled `org.outrig.session=<sid>`, both in `session.json`, so that
/// with the record gone `outrig clean` finds it as a stray. A running stray is
/// reported, never removed, for `run` and `run-new` alike; the primary runs
/// with `--rm`, so a stopped one is gone on its own, and removing a stopped
/// labeled stray is the sweep `mcp_sidecar_smoke.rs` covers.
#[tokio::test]
async fn a_killed_session_leaves_a_labeled_stray_clean_can_find() {
    let (addr, _requests) = start_mock_http(vec![text_reply("unused")]).await;
    let repo = repo(addr);
    let sessions = tempfile::tempdir().expect("a session root");
    let session = Session::start(repo.path(), sessions.path());
    wait_for(&session.stderr, "(Ctrl-D to exit", TEST_TIMEOUT).await;

    let sid = banner_session_id(&session.stderr);
    let container = format!("outrig-{sid}");
    let tidy = RemoveOnDrop(container.clone());
    assert!(
        session
            .stderr
            .lock()
            .expect("unpoisoned")
            .contains(&format!("ready in {container}")),
        "the banner names the container for the session"
    );
    assert_eq!(
        podman_names(&format!("label=org.outrig.session={sid}")),
        container,
        "exactly the primary carries the session label"
    );
    let session_dir = sessions.path().join(&sid);
    let record: Value = serde_json::from_str(
        &std::fs::read_to_string(session_dir.join("session.json")).expect("a record"),
    )
    .expect("the record parses");
    assert_eq!(record["id"], json!(sid), "{record:#}");
    assert_eq!(record["container_name"], json!(container), "{record:#}");

    sigkill(&session.pid);
    let (status, _, _) = session.exit(false).await;
    use std::os::unix::process::ExitStatusExt as _;
    assert_eq!(status.signal(), Some(9), "{status}");
    assert_eq!(
        podman_names(&format!("name={container}")),
        container,
        "podman's container outlives outrig"
    );

    // With the record gone, as a later `clean` or a person removing the
    // directory leaves it, the label is all that ties the container to the
    // session.
    std::fs::remove_dir_all(&session_dir).expect("remove the record");
    let out = Command::new(env!("CARGO_BIN_EXE_outrig"))
        .arg("--global-config")
        .arg(repo.path().join("no-such-global.toml"))
        .arg("--session-root")
        .arg(sessions.path())
        .args(["clean", "-y", "--older-than", "2s", "--session", &sid])
        .output()
        .await
        .expect("outrig clean");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stderr}");
    assert!(
        stderr.contains("[outrig] skipped running labeled containers (no session record):\n"),
        "{stderr}"
    );
    assert!(
        stderr.contains(&format!("  {container}  session {sid}\n")),
        "{stderr}"
    );
    assert!(
        stderr.contains("no stopped sessions or stray containers older than 2s"),
        "{stderr}"
    );

    drop(tidy);
    wait_until(STEP_TIMEOUT, "the container to be removed", || {
        podman_names(&format!("name={container}")).is_empty()
    })
    .await;
}

/// Lines refused as input ends are still reported: the answer to what was
/// typed is shown before a graceful exit, as what the agent sent is.
#[tokio::test]
async fn lines_refused_as_input_ends_are_still_reported() {
    const REFUSED: usize = 10;
    let (addr, requests) = start_mock_http(vec![text_reply("unused")]).await;
    let repo = repo(addr);
    let sessions = tempfile::tempdir().expect("a session root");
    let mut session = Session::start(repo.path(), sessions.path());

    wait_for(&session.stderr, "(Ctrl-D to exit", TEST_TIMEOUT).await;
    for _ in 0..REFUSED {
        session.type_line(&"x".repeat((1 << 20) + 1)).await;
    }
    let (status, _, stderr) = session.exit(true).await;
    assert!(status.success(), "{status}: {stderr}");
    assert_eq!(
        stderr
            .matches("[outrig] error: not delivered: the message is 1048577 bytes")
            .count(),
        REFUSED,
        "{stderr}"
    );
    let mut requests = requests;
    assert!(
        drain_recorded(&mut requests).is_empty(),
        "nothing was delivered, so no round ran"
    );
}

/// Code that caught its cancellation holds the interpreter after a second
/// Ctrl-C gives up on it, and every submission is refused behind it (#468). A
/// Ctrl-C at the prompt stops it, and the next line's Python runs, with how
/// the holder ended reported ahead of its result. Code that catches that too
/// keeps the interpreter, and the second of two Ctrl-Cs at the prompt still
/// exits.
#[tokio::test]
async fn a_ctrl_c_at_the_prompt_stops_python_an_earlier_one_left_running() {
    let (addr, mut requests) = start_mock_http(vec![
        submit(
            "toolu_stubborn",
            "open('running', 'w').close()\n\
             try:\n    await asyncio.get_running_loop().create_future()\n\
             except asyncio.CancelledError:\n    open('caught', 'w').close()\n\
             try:\n    await asyncio.get_running_loop().create_future()\n\
             finally:\n    open('ended', 'w').close()",
        ),
        text_reply("gave up"),
        submit("toolu_next", "print('again')"),
        text_reply("ran again"),
        submit(
            "toolu_unkillable",
            "open('holding', 'w').close()\nwhile True:\n    try:\n        \
             await asyncio.get_running_loop().create_future()\n    except BaseException:\n        \
             pass",
        ),
        text_reply("gave up again"),
    ])
    .await;
    let repo = repo(addr);
    let sessions = tempfile::tempdir().expect("a session root");
    let mut session = Session::start(repo.path(), sessions.path());

    session.type_line("wait").await;
    wait_for_file(&repo.path().join("running"), TEST_TIMEOUT).await;
    ctrl_c(&session.pid);
    wait_for_file(&repo.path().join("caught"), STEP_TIMEOUT).await;
    ctrl_c(&session.pid);
    wait_for(&session.stderr, "gave up", STEP_TIMEOUT).await;
    // The round after the reply finds nothing new, and the prompt is drawn.
    tokio::time::sleep(Duration::from_millis(500)).await;

    // At the prompt, with execution 1 holding the interpreter.
    ctrl_c(&session.pid);
    wait_for(&session.stderr, "nothing waiting for it", STEP_TIMEOUT).await;
    wait_for(&session.stderr, "(Ctrl-C again exits)", STEP_TIMEOUT).await;
    wait_for_file(&repo.path().join("ended"), STEP_TIMEOUT).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    session.type_line("again").await;
    wait_for(&session.stderr, "ran again", STEP_TIMEOUT).await;

    // Code nothing stops: two Ctrl-Cs give up on it, and two more at the
    // prompt -- the first stopping nothing -- end the session.
    session.type_line("hold").await;
    wait_for_file(&repo.path().join("holding"), STEP_TIMEOUT).await;
    ctrl_c(&session.pid);
    tokio::time::sleep(Duration::from_millis(500)).await;
    ctrl_c(&session.pid);
    wait_for(&session.stderr, "gave up again", STEP_TIMEOUT).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    ctrl_c(&session.pid);
    tokio::time::sleep(Duration::from_millis(500)).await;
    ctrl_c(&session.pid);
    let (status, stdout, stderr) = session.exit(false).await;
    assert!(status.success(), "{status}: {stderr}");
    assert_eq!(
        stdout, "",
        "the agent sent nothing; its commentary is on stderr"
    );
    assert_eq!(
        stderr.matches("nothing waiting for it").count(),
        2,
        "one stop per held execution: {stderr}"
    );

    let recorded = drain_recorded(&mut requests);
    assert_eq!(recorded.len(), 6, "three rounds of two calls");
    let unknown = tool_result(&recorded[1], "toolu_stubborn");
    assert!(unknown.contains("interrupted it twice"), "{unknown}");
    let next = tool_result(&recorded[3], "toolu_next");
    assert!(next.contains("CancelledError"), "{next}");
    assert!(next.contains("[this call]\nagain\n"), "{next}");
}
