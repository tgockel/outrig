//! `outrig run-new` end to end: the binary, a real podman, an image with no
//! Python in it, and a scripted Anthropic endpoint standing in for the model.
//!
//! One session is driven the way a person drives it -- a line, the reply, a
//! Ctrl-C at the prompt, another line -- and everything is asserted from
//! outside: what reached the model, what the terminal showed, and what the
//! session left on disk.

#![cfg(feature = "e2e")]

mod common;

use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::time::timeout;

use common::{CannedResponse, RecordedRequest, drain_recorded, start_mock_http, stream_lines};

const TEST_TIMEOUT: Duration = Duration::from_secs(300);
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
"#
        ),
    )
    .expect("write config");
    repo
}

/// Read `stdout` until a line contains `want`.
async fn read_until<R: tokio::io::AsyncBufRead + Unpin>(stdout: &mut R, want: &str) {
    let mut line = String::new();
    loop {
        line.clear();
        let n = timeout(TEST_TIMEOUT, stdout.read_line(&mut line))
            .await
            .unwrap_or_else(|_| panic!("no {want:?} on stdout in time"))
            .expect("read stdout");
        assert!(n > 0, "stdout closed before {want:?}");
        if line.contains(want) {
            return;
        }
    }
}

fn podman_names(filter: &str) -> String {
    let out = std::process::Command::new("podman")
        .args(["ps", "-a", "--format", "{{.Names}}", "--filter"])
        .arg(filter)
        .output()
        .expect("podman ps");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// The phase's premise, through the binary: a name bound in one round is still
/// bound in the next. Between the two, a Ctrl-C at the prompt returns to the
/// prompt. The primary MCP server holding a secret is not started -- asserted on
/// startup, since starting it would have failed the launch -- and the session
/// is recorded under the container it really ran in.
#[tokio::test]
async fn names_survive_rounds_and_ctrl_c_returns_to_the_prompt() {
    let (addr, mut requests) = start_mock_http(vec![
        submit("toolu_1", "import os\nx = 41\nprint(os.getcwd())"),
        text_reply("bound x"),
        submit("toolu_2", "print(x + 1)"),
        text_reply("x + 1 is 42"),
    ])
    .await;
    let repo = repo(addr);
    let sessions = tempfile::tempdir().expect("a session root");

    let mut child = Command::new(env!("CARGO_BIN_EXE_outrig"))
        .args(["--global-config"])
        .arg(repo.path().join("no-such-global.toml"))
        .arg("--session-root")
        .arg(sessions.path())
        .arg("run-new")
        .current_dir(repo.path())
        .env(KEY_VAR, "sk-ant-mock-key")
        .env_remove(SECRET_VAR)
        .env("OUTRIG_LOG", "info")
        .env("NO_PROXY", "127.0.0.1,localhost")
        .env("no_proxy", "127.0.0.1,localhost")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn outrig");
    let pid = child.id().expect("a pid").to_string();
    let mut stdin = child.stdin.take().expect("stdin");
    let mut stdout = BufReader::new(child.stdout.take().expect("stdout"));
    // Mirrored as it arrives, so a hang or an early exit shows why.
    let stderr_sink = Arc::new(Mutex::new(String::new()));
    let stderr_task = tokio::spawn(stream_lines(
        child.stderr.take().expect("stderr"),
        Arc::clone(&stderr_sink),
        "stderr",
    ));

    stdin.write_all(b"bind x\n").await.expect("first line");
    read_until(&mut stdout, "bound x").await;

    // At the prompt now. One Ctrl-C there is a fresh line, not an exit.
    let kill = std::process::Command::new("kill")
        .args(["-INT", &pid])
        .status()
        .expect("kill -INT");
    assert!(kill.success());
    tokio::time::sleep(Duration::from_millis(500)).await;

    stdin.write_all(b"use x\n").await.expect("second line");
    read_until(&mut stdout, "x + 1 is 42").await;
    drop(stdin);

    let status = timeout(TEST_TIMEOUT, child.wait())
        .await
        .expect("exits after EOF")
        .expect("wait");
    stderr_task.await.expect("stderr drained");
    let stderr = stderr_sink.lock().expect("unpoisoned").clone();
    assert!(status.success(), "{status}");

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

    let recorded = drain_recorded(&mut requests);
    assert_eq!(recorded.len(), 4, "two model calls per round");
    assert_eq!(
        tool_result(&recorded[1], "toolu_1"),
        "/workspace\n",
        "the Python ran against the workspace"
    );
    assert_eq!(
        tool_result(&recorded[3], "toolu_2"),
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
    assert!(container.starts_with("outrig-"), "{record:#}");
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
