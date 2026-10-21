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

use std::io::Read;
use std::path::Path;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::task::JoinHandle;
use tokio::time::timeout;

use common::{
    CannedResponse, RecordedRequest, RepoDir, drain_recorded, start_mock_http, stream_lines,
};

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
/// server carrying a secret, beside a global config -- see [`global_config`]
/// -- holding the provider at `addr`.
fn repo(addr: std::net::SocketAddr) -> RepoDir {
    repo_with(addr, "")
}

/// The global config [`repo_with`] writes beside the repo `repo`, outside the
/// workspace: the provider, which only it may declare, and the `env-allow`
/// that lets the repo's image name the secret.
fn global_config(repo: &Path) -> std::path::PathBuf {
    repo.parent()
        .expect("a fixture repo is in a directory of its own")
        .join("global.toml")
}

/// Add `text` to the end of the config file at `path`.
fn append(path: &Path, text: &str) {
    let mut config = std::fs::read_to_string(path).expect("read the config");
    config.push_str(text);
    std::fs::write(path, config).expect("write the config");
}

/// [`repo`], its config ending with `extra`.
fn repo_with(addr: std::net::SocketAddr, extra: &str) -> RepoDir {
    let repo = RepoDir::new();
    std::fs::write(
        global_config(repo.path()),
        format!(
            r#"
[providers.claude]
style                = "anthropic"
base-url             = "http://{addr}"
api-key              = "${{{KEY_VAR}}}"
request-timeout-secs = 30

[repo]
env-allow = ["{SECRET_VAR}"]
"#
        ),
    )
    .expect("write the global config");
    let cfg_dir = repo.path().join(".agents/outrig");
    std::fs::create_dir_all(&cfg_dir).expect("create config dir");
    std::fs::write(
        cfg_dir.join("config.toml"),
        format!(
            r#"
default-model = "sonnet"
default-image = "primary"

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

/// `outrig run-new` in `repo`, with stdin piped and everything but stdout and
/// stderr, which the caller wires, set up.
fn run_new_command(repo: &Path, sessions: &Path, global: &Path, env: &[(&str, String)]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_outrig"));
    command
        .args(["--global-config"])
        .arg(global)
        .arg("--session-root")
        .arg(sessions)
        .arg("run-new")
        .current_dir(repo)
        .envs(env.iter().map(|(name, value)| (name, value)))
        .env(KEY_VAR, "sk-ant-mock-key")
        .env_remove(SECRET_VAR)
        .env("OUTRIG_LOG", "info")
        .env("NO_PROXY", "127.0.0.1,localhost")
        .env("no_proxy", "127.0.0.1,localhost")
        .stdin(Stdio::piped())
        .kill_on_drop(true)
        // A group of its own, led by outrig, as a shell's foreground job is.
        .process_group(0);
    command
}

/// `line` and a newline on outrig's stdin, as a terminal sends a typed line.
async fn type_line(stdin: &mut ChildStdin, line: &str) {
    stdin
        .write_all(format!("{line}\n").as_bytes())
        .await
        .expect("typed");
}

/// How a test reads the binary's stdout.
enum Reader {
    /// As it arrives.
    Live,
    /// A line at a time with this pause after each: a terminal slower than the
    /// agent writing to it.
    Slow(Duration),
    /// Never, with the pipe held open: a terminal that has stopped, as a
    /// paused pager or Ctrl-S leaves it.
    Stalled,
}

/// A running `outrig run-new`, its output mirrored into sinks as it arrives,
/// so a hang or an early exit shows why.
struct Session {
    child: Child,
    pid: String,
    stdin: ChildStdin,
    stdout: Arc<Mutex<String>>,
    stderr: Arc<Mutex<String>>,
    streams: Vec<JoinHandle<()>>,
    /// The read end of a [`Reader::Stalled`] stdout, kept open and unread.
    stalled: Option<ChildStdout>,
}

impl Session {
    fn start(repo: &Path, sessions: &Path) -> Self {
        Self::spawn(repo, sessions, Reader::Live)
    }

    fn spawn(repo: &Path, sessions: &Path, reader: Reader) -> Self {
        Self::launch(repo, sessions, &global_config(repo), &[], reader)
    }

    /// [`Session::start`], with `global` as the global config and `env` added
    /// to the binary's environment.
    fn with_global(repo: &Path, sessions: &Path, global: &Path, env: &[(&str, String)]) -> Self {
        Self::launch(repo, sessions, global, env, Reader::Live)
    }

    fn launch(
        repo: &Path,
        sessions: &Path,
        global: &Path,
        env: &[(&str, String)],
        reader: Reader,
    ) -> Self {
        let mut child = run_new_command(repo, sessions, global, env)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn outrig");
        let pid = child.id().expect("a pid").to_string();
        let stdin = child.stdin.take().expect("stdin");
        let stdout = Arc::new(Mutex::new(String::new()));
        let stderr = Arc::new(Mutex::new(String::new()));
        let child_stdout = child.stdout.take().expect("stdout");
        let mut streams = Vec::new();
        let mut stalled = None;
        match reader {
            Reader::Live => streams.push(tokio::spawn(stream_lines(
                child_stdout,
                Arc::clone(&stdout),
                "stdout",
            ))),
            Reader::Slow(pause) => streams.push(tokio::spawn(read_slowly(
                child_stdout,
                Arc::clone(&stdout),
                pause,
            ))),
            Reader::Stalled => stalled = Some(child_stdout),
        }
        streams.push(tokio::spawn(stream_lines(
            child.stderr.take().expect("stderr"),
            Arc::clone(&stderr),
            "stderr",
        )));
        Self {
            child,
            pid,
            stdin,
            stdout,
            stderr,
            streams,
            stalled,
        }
    }

    /// Stop reading what outrig writes and close the read ends: the reader of
    /// `outrig run-new 2>&1 | head` exiting. What was read before stays in the
    /// sinks.
    async fn hang_up(&mut self) {
        for stream in self.streams.drain(..) {
            stream.abort();
            // Cancelled, which is the point; the read end is closed once it is.
            let _ = stream.await;
        }
    }

    async fn type_line(&mut self, line: &str) {
        type_line(&mut self.stdin, line).await;
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
            stalled,
            ..
        } = self;
        let open = (!close_input).then_some(stdin);
        let status = timeout(STEP_TIMEOUT, child.wait())
            .await
            .expect("exits")
            .expect("wait");
        drop((open, stalled));
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
    // Each step of the start, as it began and as it ended.
    let at = |line: &str| {
        stderr
            .find(line)
            .unwrap_or_else(|| panic!("no {line:?}: {stderr}"))
    };
    assert!(
        at("[outrig] starting container") < at("[outrig] container ready")
            && at("[outrig] container ready") < at("[outrig] starting python")
            && at("[outrig] starting python") < at("[outrig] python ready"),
        "{stderr}"
    );
    // End of input leaves through the report, and nothing was cut off.
    assert!(
        stderr.contains("[outrig] session closed: stopped, every outcome known"),
        "{stderr}"
    );
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
    // The agent stopped, then the session reported, and its last state is
    // that it has.
    let kinds: Vec<&str> = events
        .iter()
        .map(|e| e["type"].as_str().expect("a type"))
        .collect();
    assert_eq!(
        kinds[kinds.len() - 3..],
        [
            "org.outrig.agent.stopped",
            "org.outrig.session.report",
            "org.outrig.session.state"
        ],
        "{text}"
    );
    assert_eq!(
        events.last().map(|e| e["data"].clone()),
        Some(json!({"state": "reported"})),
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
    let exited = stderr
        .find("[outrig] the Python interpreter exited; the session is over")
        .unwrap_or_else(|| panic!("{stderr}"));
    assert!(
        stderr
            .find("it is gone")
            .is_some_and(|reply| reply < exited),
        "the reply came before the note: {stderr}"
    );
    // The report says so, and that the execution it took with it is unknown.
    assert!(
        stderr.contains("[outrig]   closed because the Python interpreter exited"),
        "{stderr}"
    );
    assert!(
        stderr
            .lines()
            .any(|line| line.starts_with("[outrig]   execution ") && line.ends_with(": unknown")),
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
    let mut session = Session::spawn(
        repo.path(),
        sessions.path(),
        Reader::Slow(Duration::from_millis(1)),
    );

    session.type_line("go").await;
    wait_for(&session.stdout, "tick ", TEST_TIMEOUT).await;
    wait_for(&session.stderr, "flooding", STEP_TIMEOUT).await;
    session.type_line("/quit").await;
    let (status, _, stderr) = session.exit(false).await;
    assert!(status.success(), "{status}: {stderr}");
}

/// A session whose stdout has stalled -- a pager paused, Ctrl-S -- and whose
/// agent is flooding it (#471). The pipe fills within a few messages, and the
/// round's code then waits half a second, so the terminal is blocked well
/// before the round has anything to report. The round still reports on
/// stderr, since a stalled stdout delays output and not the round, and the
/// session can still be left, by `leave`; the status it left with is returned,
/// with stderr.
async fn leave_a_session_whose_stdout_stalled(leave: Leave) -> (std::process::ExitStatus, String) {
    let (addr, _requests) = start_mock_http(vec![
        submit(
            "toolu_stalled",
            "async def flood():\n    while True:\n        \
             await runtime.channels['user'].send('tick ' + 'x' * 16000)\n\
             flooding = asyncio.create_task(flood())\nawait asyncio.sleep(0.5)",
        ),
        text_reply("the flood is running"),
    ])
    .await;
    let repo = repo(addr);
    let sessions = tempfile::tempdir().expect("a session root");
    let mut session = Session::spawn(repo.path(), sessions.path(), Reader::Stalled);

    session.type_line("go").await;
    // Not "flooding", which the submission shown on stderr already says.
    wait_for(&session.stderr, "the flood is running", STEP_TIMEOUT).await;
    let close_input = matches!(leave, Leave::InputEndsThenReaderGoes);
    match leave {
        Leave::Quit => session.type_line("/quit").await,
        Leave::CtrlC => {
            ctrl_c(&session.pid);
            tokio::time::sleep(Duration::from_millis(500)).await;
            ctrl_c(&session.pid);
        }
        Leave::InputEndsThenReaderGoes => {
            // Ending input waits on the stopped stdout; a second later its
            // reader is gone, and the write that was waiting fails.
            let reader = session.stalled.take();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_secs(1)).await;
                drop(reader);
            });
        }
    }
    let (status, _, stderr) = session.exit(close_input).await;
    (status, stderr)
}

/// How the session is left.
enum Leave {
    /// `/quit` is typed at the prompt.
    Quit,
    /// Ctrl-C is pressed twice at the prompt.
    CtrlC,
    /// Input ends, and stdout's reader then goes away.
    InputEndsThenReaderGoes,
}

#[tokio::test]
async fn a_stalled_stdout_does_not_stop_the_round_or_quit() {
    let (status, stderr) = leave_a_session_whose_stdout_stalled(Leave::Quit).await;
    assert!(status.success(), "{status}: {stderr}");
}

#[tokio::test]
async fn a_stalled_stdout_does_not_stop_ctrl_c() {
    let (status, stderr) = leave_a_session_whose_stdout_stalled(Leave::CtrlC).await;
    assert!(status.success(), "{status}: {stderr}");
}

/// What the agent sent never reached anyone, so a session that ends this way
/// did not end cleanly: it exits 1, as one whose stdout fails while it runs
/// does, and not 0.
#[tokio::test]
async fn output_lost_as_input_ends_is_not_a_clean_exit() {
    let (status, stderr) =
        leave_a_session_whose_stdout_stalled(Leave::InputEndsThenReaderGoes).await;
    assert_eq!(status.code(), Some(1), "{status}: {stderr}");
}

/// A session whose readers go away while it runs (#574), as they do when
/// `outrig run-new 2>&1 | head` is left by `head`. The next write to stderr
/// fails and ends the conversation; nothing is sent to stdout here, so it is
/// stderr's write that does. A diagnostic that cannot be delivered is no reason
/// to skip the teardown, so the session is still stopped and recorded as
/// finished, and leaves with the conversation's status 1 -- not the 101 of a
/// panic.
#[tokio::test]
async fn a_vanished_stdout_and_stderr_reader_ends_the_session_with_1_and_a_finished_record() {
    let (addr, _requests) =
        start_mock_http(vec![submit("toolu_1", "x = 1"), text_reply("set")]).await;
    let repo = repo(addr);
    let sessions = tempfile::tempdir().expect("a session root");
    let mut session = Session::start(repo.path(), sessions.path());

    wait_for(&session.stderr, "(Ctrl-D to exit", TEST_TIMEOUT).await;
    session.hang_up().await;
    session.type_line("hello").await;
    let (status, _, stderr) = session.exit(false).await;

    assert_eq!(status.code(), Some(1), "{status}: {stderr}");
    let record = the_record(sessions.path());
    assert_eq!(record["exit_code"], 1, "{record:#}");
    assert!(!record["ended_at"].is_null(), "{record:#}");
    // Gone as the process exits, since the stop it waited for ran; left behind
    // it was still stopping, and removed only as the unwind dropped it.
    let container = record["container_name"].as_str().expect("a name");
    let _tidy = RemoveOnDrop(container.to_string());
    assert!(
        podman_names(&format!("name={container}")).is_empty(),
        "{record:#}"
    );
}

/// An `outrig run-new 2>&1`: stdout and stderr are the write end of one pipe,
/// which a thread of the test's reads, up to 4 KiB with 20 ms between reads, so
/// a message of the agent's is written to it in many parts. Reading can be
/// stopped with the read end held open, as a paused pager does, or ended by
/// closing it, as a pager that quits does.
struct Merged {
    child: Child,
    stdin: ChildStdin,
    /// What has been read from the pipe.
    read: Arc<Mutex<Vec<u8>>>,
    reading: Arc<AtomicBool>,
    hung_up: Arc<AtomicBool>,
    reader: std::thread::JoinHandle<()>,
}

impl Merged {
    fn start(repo: &Path, sessions: &Path) -> Self {
        let (mut pipe, writer) = std::io::pipe().expect("a pipe");
        let mut child = run_new_command(repo, sessions, &global_config(repo), &[])
            .stdout(writer.try_clone().expect("a second write end"))
            .stderr(writer)
            .spawn()
            .expect("spawn outrig");
        let stdin = child.stdin.take().expect("stdin");
        let read = Arc::new(Mutex::new(Vec::new()));
        let reading = Arc::new(AtomicBool::new(true));
        let hung_up = Arc::new(AtomicBool::new(false));
        let reader = std::thread::spawn({
            let (read, reading) = (Arc::clone(&read), Arc::clone(&reading));
            let hung_up = Arc::clone(&hung_up);
            move || {
                let mut buf = [0; 4096];
                loop {
                    std::thread::sleep(Duration::from_millis(20));
                    if hung_up.load(Ordering::SeqCst) {
                        return;
                    }
                    if !reading.load(Ordering::SeqCst) {
                        continue;
                    }
                    match pipe.read(&mut buf) {
                        Ok(0) | Err(_) => return,
                        Ok(n) => read
                            .lock()
                            .expect("unpoisoned")
                            .extend_from_slice(&buf[..n]),
                    }
                }
            }
        });
        Self {
            child,
            stdin,
            read,
            reading,
            hung_up,
            reader,
        }
    }

    async fn type_line(&mut self, line: &str) {
        type_line(&mut self.stdin, line).await;
    }

    /// Wait until `what` of the message of `A`s has been read: past the
    /// banner and everything else of stderr's, none of which has that many.
    async fn wait_for_a_s(&self, what: usize) {
        let count = || {
            self.read
                .lock()
                .expect("unpoisoned")
                .iter()
                .filter(|b| **b == b'A')
                .count()
        };
        wait_until(TEST_TIMEOUT, "the agent's message", || count() >= what).await;
    }

    /// Stop reading, with the pipe still open.
    fn stop_reading(&self) {
        self.reading.store(false, Ordering::SeqCst);
    }

    /// Close the read end, so that a write to the pipe fails.
    async fn hang_up(&self) {
        self.hung_up.store(true, Ordering::SeqCst);
        // The reader checks between reads, and a paused one only then.
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    /// Wait for outrig to exit, with input still open as a terminal's is, then
    /// read the rest of the pipe and return how it exited and all it wrote.
    async fn exit(self) -> (std::process::ExitStatus, String) {
        let Self {
            mut child,
            stdin,
            read,
            reading,
            hung_up: _,
            reader,
        } = self;
        let status = timeout(STEP_TIMEOUT, child.wait())
            .await
            .expect("exits")
            .expect("wait");
        drop(stdin);
        reading.store(true, Ordering::SeqCst);
        reader.join().expect("the reader finished");
        let read = read.lock().expect("unpoisoned");
        (status, String::from_utf8_lossy(&read).into_owned())
    }
}

/// `text` with each run of 64 or more `A`s as `<N A>`, so that what a message
/// of 900000 of them was written between shows.
fn squeeze_runs(text: &str) -> String {
    regex::Regex::new("A{64,}")
        .expect("a pattern")
        .replace_all(text, |run: &regex::Captures| {
            format!("<{} A>", run[0].len())
        })
        .into_owned()
}

/// What the agent's code sends, and then waits out: a message much larger than
/// a pipe holds, written to it in many parts as it is read, and a round that is
/// still running at `/quit`.
const BIG_MESSAGE: &str = "await runtime.channels['user'].send('A' * 900000)\n\
                           await asyncio.sleep(60)";

/// With stdout and stderr one pipe (#579), `/quit` while a message larger than
/// the pipe holds is being written leaves the message whole: what the session
/// says as it closes is written before or after it, and never inside it.
#[tokio::test]
async fn closing_a_session_whose_stdout_and_stderr_are_one_pipe_does_not_cut_a_message() {
    let (addr, _requests) = start_mock_http(vec![submit("toolu_big", BIG_MESSAGE)]).await;
    let repo = repo(addr);
    let sessions = tempfile::tempdir().expect("a session root");
    let mut session = Merged::start(repo.path(), sessions.path());

    session.type_line("go").await;
    session.wait_for_a_s(4096).await;
    session.type_line("/quit").await;
    let (status, output) = session.exit().await;

    let shown = squeeze_runs(&output);
    assert_eq!(status.code(), Some(0), "{shown}");
    assert!(
        shown.contains("<900000 A>"),
        "the message was cut:\n{shown}"
    );
    assert!(shown.contains("[outrig] closing the session"), "{shown}");
    assert!(shown.contains("[outrig] session closed:"), "{shown}");
}

/// With that pipe no longer read (#579), `/quit` still stops the session: the
/// container is removed and the record finished while the pipe is unread, and
/// `run-new` exits on its own, not once something reads again.
#[tokio::test]
async fn quit_stops_a_session_whose_one_pipe_is_no_longer_read() {
    let (addr, _requests) = start_mock_http(vec![submit("toolu_big", BIG_MESSAGE)]).await;
    let repo = repo(addr);
    let sessions = tempfile::tempdir().expect("a session root");
    let mut session = Merged::start(repo.path(), sessions.path());

    session.type_line("go").await;
    session.wait_for_a_s(4096).await;
    session.stop_reading();
    tokio::time::sleep(Duration::from_secs(1)).await;
    session.type_line("/quit").await;

    wait_until(
        STEP_TIMEOUT,
        "finished record while the pipe is unread",
        || !the_record(sessions.path())["ended_at"].is_null(),
    )
    .await;
    let record = the_record(sessions.path());
    let container = record["container_name"].as_str().expect("a name");
    let _tidy = RemoveOnDrop(container.to_string());
    assert!(
        podman_names(&format!("name={container}")).is_empty(),
        "{record:#}"
    );
    let (status, output) = session.exit().await;
    assert_eq!(status.code(), Some(0), "{}", squeeze_runs(&output));
}

/// What the agent's code sends, and then holds the interpreter against every
/// interrupt: the stop waits out its five seconds for the code, so it lasts
/// long enough to act in, and cuts the code off, which exits 2.
const BIG_MESSAGE_AND_HOLD: &str = "await runtime.channels['user'].send('A' * 900000)\n\
                                    import time\n\
                                    while True:\n    try:\n        time.sleep(0.1)\n    \
                                    except BaseException:\n        pass";

/// With that pipe no longer read, a write the conversation queued -- here
/// `/help`'s text -- is still waiting when `/quit` leaves, which does not wait
/// for it. When the reader then goes away as the session stops, the write fails
/// and the session did not end cleanly: the record and the exit status both say
/// 1, not the 2 the stop would have decided alone.
#[tokio::test]
async fn a_write_that_fails_as_the_session_stops_ends_it_with_1() {
    let (addr, _requests) = start_mock_http(vec![submit("toolu_big", BIG_MESSAGE_AND_HOLD)]).await;
    let repo = repo(addr);
    let sessions = tempfile::tempdir().expect("a session root");
    let mut session = Merged::start(repo.path(), sessions.path());

    session.type_line("go").await;
    session.wait_for_a_s(4096).await;
    session.stop_reading();
    tokio::time::sleep(Duration::from_secs(1)).await;
    session.type_line("/help").await;
    session.type_line("/quit").await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    let record = the_record(sessions.path());
    assert!(
        record["ended_at"].is_null(),
        "the stop was over before the reader went away: {record:#}"
    );
    session.hang_up().await;
    let (status, output) = session.exit().await;

    let shown = squeeze_runs(&output);
    assert_eq!(status.code(), Some(1), "{shown}");
    let record = the_record(sessions.path());
    assert_eq!(record["exit_code"], 1, "{record:#}");
    assert!(!record["ended_at"].is_null(), "{record:#}");
    let container = record["container_name"].as_str().expect("a name");
    let _tidy = RemoveOnDrop(container.to_string());
    assert!(
        podman_names(&format!("name={container}")).is_empty(),
        "{record:#}"
    );
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
        Reader::Slow(Duration::from_millis(10)),
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
        .arg(global_config(repo.path()))
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
    // Written before `run-new` goes on to close the session, not queued
    // behind it.
    let last_refusal = stderr
        .rfind("[outrig] error: not delivered")
        .expect("a refusal");
    let closing = stderr
        .find("[outrig] closing the session")
        .expect("a closing line");
    assert!(last_refusal < closing, "{stderr}");
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
    // Leaving cut the unkillable code off, and the status and the report say
    // so.
    assert_eq!(status.code(), Some(2), "{status}: {stderr}");
    assert!(
        stderr.contains(
            "[outrig] session closed: stopped, but some Python or binding call was cut off"
        ),
        "{stderr}"
    );
    assert!(
        stderr
            .lines()
            .any(|line| line.starts_with("[outrig]   execution ") && line.ends_with(": unknown")),
        "{stderr}"
    );
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

/// The one session dir under `sessions`, and its record.
fn the_record(sessions: &Path) -> Value {
    let dir = std::fs::read_dir(sessions)
        .expect("the session root")
        .next()
        .expect("one session")
        .expect("an entry")
        .path();
    serde_json::from_str(&std::fs::read_to_string(dir.join("session.json")).expect("a record"))
        .expect("the record parses")
}

/// `/quit` with nothing running leaves through the report, which reads
/// stopped with every outcome known, and exits 0. A round that left nothing
/// running closes on no line about it.
#[tokio::test]
async fn quit_with_nothing_running_exits_0_and_reports_a_clean_stop() {
    let (addr, _requests) =
        start_mock_http(vec![submit("toolu_1", "x = 1"), text_reply("set")]).await;
    let repo = repo(addr);
    let sessions = tempfile::tempdir().expect("a session root");
    let mut session = Session::start(repo.path(), sessions.path());

    session.type_line("set x").await;
    wait_for(&session.stderr, "set", TEST_TIMEOUT).await;
    session.type_line("/quit").await;
    let (status, _, stderr) = session.exit(false).await;
    assert_eq!(status.code(), Some(0), "{stderr}");
    assert!(!stderr.contains("(still running"), "{stderr}");
    assert!(
        stderr.contains("[outrig] session closed: stopped, every outcome known"),
        "{stderr}"
    );
    let record = the_record(sessions.path());
    assert_eq!(record["exit_code"], 0, "{record:#}");
    let container = record["container_name"].as_str().expect("a name");
    assert!(podman_names(&format!("name={container}")).is_empty());
}

/// A binding the repository's config declares, with no terminal to ask the
/// operator on, does not start: `run-new` says which and how to approve it,
/// and the session runs without it.
#[tokio::test]
async fn an_unapproved_repo_binding_with_no_terminal_does_not_start() {
    let (addr, mut requests) = start_mock_http(vec![
        submit("toolu_1", "print(list(runtime.bindings))"),
        text_reply("listed"),
    ])
    .await;
    let repo = repo_with(
        addr,
        "[bindings.rec]\ndescription = \"Records.\"\nfactory = \"recorder:make\"\n",
    );
    let sessions = tempfile::tempdir().expect("a session root");
    let mut session = Session::start(repo.path(), sessions.path());
    session.type_line("list them").await;
    wait_for(&session.stderr, "listed", TEST_TIMEOUT).await;
    session.type_line("/quit").await;
    let (status, _, stderr) = session.exit(false).await;
    assert_eq!(status.code(), Some(0), "{stderr}");
    assert!(
        stderr.contains("the binding `rec`, declared in")
            && stderr.contains("is not approved and there is no terminal to ask on"),
        "{stderr}"
    );
    assert!(
        stderr.contains("move [bindings.rec] to the global config"),
        "{stderr}"
    );
    assert!(!stderr.contains("starting bindings"), "{stderr}");
    let recorded = drain_recorded(&mut requests);
    assert_eq!(
        tool_result(&recorded[1], "toolu_1"),
        "[]\n",
        "the agent sees no binding"
    );
}

/// A round whose code leaves a task running says so as it closes, so a prompt
/// that returns is not taken for work done.
#[tokio::test]
async fn a_round_that_leaves_a_task_running_says_so() {
    let (addr, _requests) = start_mock_http(vec![
        submit(
            "toolu_ci",
            "ci = asyncio.create_task(asyncio.sleep(60), name='ci_run')",
        ),
        text_reply("started ci"),
    ])
    .await;
    let repo = repo(addr);
    let sessions = tempfile::tempdir().expect("a session root");
    let mut session = Session::start(repo.path(), sessions.path());

    session.type_line("start ci").await;
    wait_for(&session.stderr, "(still running: ci_run)", TEST_TIMEOUT).await;
    let (status, _, stderr) = session.exit(true).await;
    // A task is not an execution: leaving cuts no execution off.
    assert_eq!(status.code(), Some(0), "{stderr}");
    let reply = stderr.find("started ci").expect("the reply");
    let closing = stderr.find("(still running: ci_run)").expect("the line");
    assert!(reply < closing, "the line closes the round: {stderr}");
}

/// `/quit` while an execution catches every interrupt: the session stops
/// anyway, and leaving says the code was cut off -- in the report, naming the
/// execution unknown, and in the exit status, 2.
#[tokio::test]
async fn quit_while_an_execution_ignores_interrupts_exits_2() {
    let (addr, _requests) = start_mock_http(vec![
        submit(
            "toolu_hold",
            "import time\nopen('holding', 'w').close()\nwhile True:\n    try:\n        \
             time.sleep(0.1)\n    except BaseException:\n        pass",
        ),
        text_reply("never"),
    ])
    .await;
    let repo = repo(addr);
    let sessions = tempfile::tempdir().expect("a session root");
    let mut session = Session::start(repo.path(), sessions.path());

    session.type_line("hold").await;
    wait_for_file(&repo.path().join("holding"), TEST_TIMEOUT).await;
    session.type_line("/quit").await;
    let (status, _, stderr) = session.exit(false).await;
    assert_eq!(status.code(), Some(2), "{stderr}");
    assert!(
        stderr.contains(
            "[outrig] session closed: stopped, but some Python or binding call was cut off"
        ),
        "{stderr}"
    );
    assert!(
        stderr
            .lines()
            .any(|line| line.starts_with("[outrig]   execution ") && line.ends_with(": unknown")),
        "{stderr}"
    );
    let record = the_record(sessions.path());
    assert_eq!(record["exit_code"], 2, "{record:#}");
    let container = record["container_name"].as_str().expect("a name");
    assert!(podman_names(&format!("name={container}")).is_empty());
}

/// The hosted-call fixture library the `outrig` crate's tests host, as a
/// module.
const FIXTURE: &str = include_str!("../../outrig/src/python/outrig_fixture.py");

/// Build the fixture's wheel into `dir` with the host's `python3`, versioned
/// by its content -- `1.0+<digest>` -- so the install cache, keyed by the
/// requirement, never serves an older fixture; and return the requirement
/// that names it.
fn fixture_wheel(dir: &Path) -> String {
    const BUILD: &str = r#"
import hashlib, os, sys, zipfile
source, out = sys.argv[1], sys.argv[2]
body = open(source, "rb").read()
version = "1.0+" + hashlib.sha256(body).hexdigest()[:16]
dist = f"outrig_fixture-{version}.dist-info"
with zipfile.ZipFile(os.path.join(out, f"outrig_fixture-{version}-py3-none-any.whl"), "w") as z:
    z.writestr("outrig_fixture.py", body)
    z.writestr(f"{dist}/METADATA", f"Metadata-Version: 2.1\nName: outrig_fixture\nVersion: {version}\n")
    z.writestr(
        f"{dist}/WHEEL",
        "Wheel-Version: 1.0\nGenerator: outrig-tests\nRoot-Is-Purelib: true\nTag: py3-none-any\n",
    )
    z.writestr(f"{dist}/RECORD", "")
print(f"outrig_fixture=={version}")
"#;
    let source = dir.join("outrig_fixture.py");
    std::fs::write(&source, FIXTURE).expect("write the fixture's source");
    let output = std::process::Command::new("python3")
        .args(["-I", "-c", BUILD])
        .arg(&source)
        .arg(dir)
        .output()
        .expect("the host's python3 builds the fixture's wheel");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    std::fs::remove_file(&source).expect("only wheels in the links directory");
    String::from_utf8(output.stdout)
        .expect("UTF-8")
        .trim()
        .to_string()
}

/// pip's environment for installing from the wheels in `dir` alone.
fn wheels_only(dir: &Path) -> Vec<(&'static str, String)> {
    vec![
        ("PIP_NO_INDEX", "1".into()),
        ("PIP_FIND_LINKS", format!("file://{}", dir.display())),
        ("PIP_CONFIG_FILE", "/dev/null".into()),
    ]
}

/// Every record of the one session under `sessions`.
fn the_log(sessions: &Path) -> Vec<Value> {
    let dir = std::fs::read_dir(sessions)
        .expect("the session root")
        .next()
        .expect("one session")
        .expect("an entry")
        .path();
    let text = std::fs::read_to_string(dir.join("logs/events.jsonl")).expect("the event log");
    text.lines()
        .map(|line| serde_json::from_str(line).expect("a whole record"))
        .collect()
}

/// Each hosted request in `log`, in the order received: its id and the kinds
/// and data of its events.
fn hosted_requests(log: &[Value]) -> Vec<(u64, Vec<(String, Value)>)> {
    let mut requests: Vec<(u64, Vec<(String, Value)>)> = Vec::new();
    for record in log {
        let kind = record["type"].as_str().expect("a type");
        let Some(kind) = kind.strip_prefix("org.outrig.hosted.") else {
            continue;
        };
        if kind == "callback" {
            continue;
        }
        let id = record["data"]["request"].as_u64().expect("a request id");
        let data = record["data"].clone();
        match requests.iter_mut().find(|(request, _)| *request == id) {
            Some((_, events)) => events.push((kind.to_string(), data)),
            None => requests.push((id, vec![(kind.to_string(), data)])),
        }
    }
    requests
}

/// Every hosted request agent code makes through `run-new` is recorded,
/// receipt first, as RPyC's proxy sends it, the binding's name used as plain
/// Python: a read through two attributes is two requests, a method call two,
/// `for` a request for the iterator and one per item and for the end, and `len`,
/// truthiness, `in`, `==`, assignment, deletion, `with` and `repr` one each --
/// an execution that ends in the name one `repr` too. A call that raises names
/// the fixture's exception while the agent catches it by its own class.
#[tokio::test]
async fn every_hosted_request_is_recorded_through_run_new() {
    let wheels = tempfile::tempdir().expect("a wheels directory");
    let requirement = fixture_wheel(wheels.path());
    let (addr, mut requests) = start_mock_http(vec![
        submit(
            "toolu_1",
            "from outrig_fixture import FixtureError\n\
             chain = fx.a.b\n\
             print(fx.method(2, y=3))\n\
             items = []\n\
             for x in fx.sequence:\n    items.append(x)\n\
             print(items)\n\
             print(len(fx.sequence), bool(fx), 1 in fx.sequence, fx.a == 1)\n\
             fx.writable = 'v'\n\
             print(fx.writable)\n\
             del fx.writable\n\
             with fx.manager:\n    pass\n\
             print(repr(fx).startswith('<outrig_fixture.Root object'))\n\
             try:\n    fx.raise_fixture()\n\
             except FixtureError as e:\n    print('caught', e.status)\n\
             print(fx.truth_tested, fx.manager.entered, hasattr(fx, 'writable'))\n\
             fx",
        ),
        text_reply("recorded"),
    ])
    .await;
    let repo = repo_with(addr, "[events]\nmode = \"record\"");
    let global = global_config(repo.path());
    append(
        &global,
        &format!(
            "[bindings.fx]\ndescription = \"The fixture.\"\nrequires = [{requirement:?}]\n\
             factory = \"outrig_fixture:make\"\n"
        ),
    );
    let sessions = tempfile::tempdir().expect("a session root");
    let mut session = Session::with_global(
        repo.path(),
        sessions.path(),
        &global,
        &wheels_only(wheels.path()),
    );
    session.type_line("record it").await;
    wait_for(&session.stderr, "recorded", TEST_TIMEOUT).await;
    session.type_line("/quit").await;
    let (status, _, stderr) = session.exit(false).await;
    assert_eq!(status.code(), Some(0), "{stderr}");
    let recorded = drain_recorded(&mut requests);
    let printed = tool_result(&recorded[1], "toolu_1");
    let lines: Vec<&str> = printed.lines().collect();
    assert_eq!(
        lines[..7],
        [
            "5",
            "[10, 20, 30]",
            "3 True False False",
            "v",
            "True",
            "caught 3",
            "1 1 False",
        ],
        "{printed}"
    );
    assert!(
        lines[7].starts_with("<outrig_fixture.Root object at "),
        "{printed}"
    );
    let log = the_log(sessions.path());
    let requests = hosted_requests(&log);
    let asked: Vec<(&str, Option<&str>)> = requests
        .iter()
        .map(|(_, events)| {
            let kinds: Vec<&str> = events.iter().map(|(kind, _)| kind.as_str()).collect();
            assert_eq!(kinds, ["received", "dispatched", "ended"], "{events:?}");
            let receipt = &events[0].1;
            (
                receipt["operation"].as_str().expect("an operation"),
                receipt["member"].as_str(),
            )
        })
        .collect();
    assert_eq!(
        asked,
        [
            ("getattr", Some("a")),
            ("getattr", Some("b")),
            ("getattr", Some("method")),
            ("call", None),
            ("getattr", Some("sequence")),
            ("callattr", Some("__iter__")),
            ("callattr", Some("__next__")),
            ("callattr", Some("__next__")),
            ("callattr", Some("__next__")),
            ("callattr", Some("__next__")),
            ("getattr", Some("sequence")),
            ("callattr", Some("__len__")),
            ("callattr", Some("__bool__")),
            ("getattr", Some("sequence")),
            ("callattr", Some("__contains__")),
            ("getattr", Some("a")),
            ("cmp", Some("__eq__")),
            ("setattr", Some("writable")),
            ("getattr", Some("writable")),
            ("delattr", Some("writable")),
            ("getattr", Some("manager")),
            ("callattr", Some("__enter__")),
            ("ctxexit", None),
            ("repr", None),
            ("getattr", Some("raise_fixture")),
            ("call", None),
            ("getattr", Some("truth_tested")),
            ("getattr", Some("manager")),
            ("getattr", Some("entered")),
            ("getattr", Some("writable")),
            ("repr", None),
        ]
    );
    let raised: Vec<&str> = requests
        .iter()
        .filter_map(|(_, events)| {
            let end = &events.last().expect("an end").1;
            (end["outcome"] == "raised").then(|| end["exception"].as_str().expect("a type"))
        })
        .collect();
    assert_eq!(
        raised,
        [
            "builtins.StopIteration",
            "outrig_fixture.FixtureError",
            "builtins.AttributeError"
        ]
    );
}

/// GitPython, as a binding over the repository the session works in, used as
/// GitPython documents it: agent code reads the head commit, commits a change,
/// pushes it to a bare repository over `file://`, walks the history, checks a
/// commit against the library's own class and catches the library's own
/// exception. The bare repository then holds the commit, and the stream holds
/// every request, each to its end. Installs GitPython from the index, so it
/// needs the network.
#[tokio::test]
async fn a_gitpython_binding_is_used_as_plain_code() {
    let bare = tempfile::tempdir().expect("a bare repository");
    let git = |dir: &Path, args: &[&str]| {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .expect("git runs");
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).expect("UTF-8")
    };
    git(bare.path(), &["init", "--bare", "-b", "main"]);
    let (addr, mut requests) = start_mock_http(vec![
        submit(
            "toolu_1",
            "import os\n\
             os.environ['GIT_PYTHON_REFRESH'] = 'quiet'\n\
             import git\n\
             start = repo.head.commit.hexsha\n\
             open('change.txt', 'w').write('a change\\n')\n\
             repo.index.add(['change.txt'])\n\
             commit = repo.index.commit('add a change')\n\
             repo.remotes.origin.push('HEAD:refs/heads/main')\n\
             shas = []\n\
             for c in repo.iter_commits():\n    shas.append(c.hexsha)\n\
             print(commit.hexsha)\n\
             print(shas == [commit.hexsha, start], len(commit.parents),\n\
             \x20     isinstance(commit, git.Commit))\n\
             try:\n    repo.git.rev_parse('no-such-ref')\n\
             except git.GitCommandError as e:\n    print('GitCommandError', e.status)",
        ),
        text_reply("pushed"),
    ])
    .await;
    let work = repo_with(addr, "[events]\nmode = \"record\"");
    git(work.path(), &["init", "-b", "main"]);
    git(work.path(), &["config", "user.name", "OutRig Test"]);
    git(
        work.path(),
        &["config", "user.email", "test@outrig.invalid"],
    );
    git(work.path(), &["add", "-A"]);
    git(work.path(), &["commit", "-m", "the start"]);
    let url = format!("file://{}", bare.path().display());
    git(work.path(), &["remote", "add", "origin", &url]);
    let global = global_config(work.path());
    append(
        &global,
        "[bindings.repo]\ndescription = \"The repository, through GitPython.\"\n\
         requires = [\"GitPython==3.2.0\"]\nfactory = \"git:Repo\"\nargs = [{ path = \".\" }]\n",
    );
    let sessions = tempfile::tempdir().expect("a session root");
    let mut session = Session::with_global(work.path(), sessions.path(), &global, &[]);
    session.type_line("commit and push").await;
    wait_for(&session.stderr, "pushed", TEST_TIMEOUT).await;
    session.type_line("/quit").await;
    let (status, _, stderr) = session.exit(false).await;
    assert_eq!(status.code(), Some(0), "{stderr}");
    let recorded = drain_recorded(&mut requests);
    let printed = tool_result(&recorded[1], "toolu_1");
    let lines: Vec<&str> = printed.lines().collect();
    assert_eq!(lines.len(), 3, "{printed}");
    let sha = lines[0];
    assert_eq!(sha.len(), 40, "{printed}");
    assert_eq!(lines[1], "True 1 True", "{printed}");
    assert_eq!(lines[2], "GitCommandError 128", "{printed}");
    assert_eq!(
        git(bare.path(), &["rev-parse", "refs/heads/main"]).trim(),
        sha,
        "the bare repository holds the commit"
    );
    let requests = hosted_requests(&the_log(sessions.path()));
    let members: Vec<&str> = requests
        .iter()
        .filter_map(|(_, events)| events[0].1["member"].as_str())
        .collect();
    for member in [
        "head",
        "commit",
        "hexsha",
        "index",
        "add",
        "remotes",
        "origin",
        "push",
        "iter_commits",
        "__iter__",
        "__next__",
        "parents",
        "git",
        "rev_parse",
    ] {
        assert!(
            members.contains(&member),
            "no request for {member}: {members:?}"
        );
    }
    let mut raised = Vec::new();
    for (id, events) in &requests {
        let end = &events.last().expect("an end");
        assert_eq!(end.0, "ended", "request {id} did not end: {events:?}");
        match end.1["outcome"].as_str() {
            Some("returned") => {}
            Some("raised") => raised.push(end.1["exception"].as_str().expect("a type")),
            _ => panic!("request {id}: {events:?}"),
        }
    }
    assert_eq!(
        raised,
        ["builtins.StopIteration", "git.exc.GitCommandError"],
        "{requests:?}"
    );
}
