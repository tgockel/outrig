//! End-to-end coverage for embedded MCP config used by CLI entrypoints.
//! Gated behind `--features e2e` because it builds fixture images and starts
//! real podman containers.

#![cfg(feature = "e2e")]

mod common;

use std::process::Stdio;
use std::time::Duration;

use outrig::container::embedded;
use rmcp::service::serve_client;
use tokio::process::Command;
use tokio::time::timeout;

const TEST_TIMEOUT: Duration = Duration::from_secs(120);
static E2E_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Dockerfile-escape a label value (backslashes first, then double quotes) so a
/// JSON value survives `LABEL "key"="value"` parsing.
fn dockerfile_escape(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

fn label_line(key: &str, value: &str) -> String {
    format!("LABEL \"{key}\"=\"{}\"\n", dockerfile_escape(value))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_show_merged_prints_effective_toml() {
    common::init_tracing();
    let _guard = E2E_LOCK.lock().await;
    let repo_dir = tempfile::tempdir().expect("tempdir repo");
    let sessions = tempfile::tempdir().expect("tempdir sessions");
    let image_ctx = tempfile::tempdir().expect("tempdir image context");
    let agents_dir = repo_dir.path().join(".agents/outrig");
    std::fs::create_dir_all(&agents_dir).expect("mkdir .agents/outrig");

    let dockerfile = format!(
        "FROM docker.io/library/alpine:latest\n\
         RUN apk add --no-cache nodejs npm shadow\n\
         RUN npm install -g @modelcontextprotocol/server-filesystem\n\
         {}",
        label_line(
            embedded::LABEL_MCP,
            r#"{"fs":["node","-e","process.exit(42)"],"shell":["mcp-server-filesystem","/workspace"]}"#,
        ),
    );
    std::fs::write(image_ctx.path().join("Dockerfile"), dockerfile).expect("write Dockerfile");

    let config_toml = format!(
        r#"
default-image = "smoke"

[images.smoke]
dockerfile = "{dockerfile}"
context = "{context}"

  [images.smoke.mcp]
  fs = ["mcp-server-filesystem", "/workspace"]
"#,
        dockerfile = image_ctx.path().join("Dockerfile").display(),
        context = image_ctx.path().display(),
    );
    std::fs::write(agents_dir.join("config.toml"), config_toml).expect("write config");

    let bin = env!("CARGO_BIN_EXE_outrig");
    let output = timeout(
        TEST_TIMEOUT,
        Command::new(bin)
            .args([
                "--session-root",
                sessions.path().to_str().expect("sessions path utf-8"),
                "mcp",
                "show-merged",
                "--image",
                "smoke",
            ])
            .current_dir(repo_dir.path())
            .stdin(Stdio::null())
            .output(),
    )
    .await
    .expect("show-merged timed out")
    .expect("run show-merged");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "show-merged exited {:?}; stdout:\n{stdout}\nstderr:\n{stderr}",
        output.status,
    );
    assert!(stdout.contains("[mcp]"), "stdout lacked [mcp]: {stdout}");
    assert!(stdout.contains("fs"), "stdout lacked fs entry: {stdout}");
    assert!(
        stdout.contains("mcp-server-filesystem"),
        "stdout lacked config override command: {stdout}",
    );
    assert!(
        stdout.contains("shell"),
        "stdout lacked additive image entry: {stdout}",
    );
    assert!(
        !stdout.contains("process.exit(42)"),
        "stdout should not contain overridden image command: {stdout}",
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn run_mode_uses_embedded_image_entries() {
    common::init_tracing();
    let _guard = E2E_LOCK.lock().await;
    let repo_dir = tempfile::tempdir().expect("tempdir repo");
    let sessions = tempfile::tempdir().expect("tempdir sessions");
    let image_ctx = tempfile::tempdir().expect("tempdir image context");
    let agents_dir = repo_dir.path().join(".agents/outrig");
    std::fs::create_dir_all(&agents_dir).expect("mkdir .agents/outrig");

    let dockerfile = format!(
        "FROM docker.io/library/alpine:latest\n\
         RUN apk add --no-cache nodejs npm shadow\n\
         RUN npm install -g @modelcontextprotocol/server-filesystem\n\
         {}",
        label_line(
            embedded::LABEL_MCP,
            r#"{"fs":["mcp-server-filesystem","/workspace"]}"#,
        ),
    );
    std::fs::write(image_ctx.path().join("Dockerfile"), dockerfile).expect("write Dockerfile");

    let config_toml = format!(
        r#"
default-agent = "smoke"
default-image = "smoke"

[providers.openai]
style = "openai"
base-url = "http://127.0.0.1:1/v1"
api-key = "${{OUTRIG_TEST_KEY}}"

[models.fast]
provider = "openai"
identifier = "gpt-4o-mini"

[agents.smoke]
model = "fast"
preamble = "test"

[images.smoke]
dockerfile = "{dockerfile}"
context = "{context}"
"#,
        dockerfile = image_ctx.path().join("Dockerfile").display(),
        context = image_ctx.path().display(),
    );
    std::fs::write(agents_dir.join("config.toml"), config_toml).expect("write config");

    let bin = env!("CARGO_BIN_EXE_outrig");
    let output = timeout(
        TEST_TIMEOUT,
        Command::new(bin)
            .args([
                "--session-root",
                sessions.path().to_str().expect("sessions path utf-8"),
                "run",
            ])
            .current_dir(repo_dir.path())
            .env("OUTRIG_TEST_KEY", "test-key")
            .stdin(Stdio::null())
            .output(),
    )
    .await
    .expect("run mode timed out")
    .expect("run outrig run");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "outrig run exited {:?}; stdout:\n{stdout}\nstderr:\n{stderr}",
        output.status,
    );
    assert!(
        stderr.contains("[outrig] mcp fs: initialized"),
        "run banner lacked embedded fs server: {stderr}",
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_server_mode_uses_embedded_image_entries() {
    common::init_tracing();
    let _guard = E2E_LOCK.lock().await;
    let repo_dir = tempfile::tempdir().expect("tempdir repo");
    let agents_dir = repo_dir.path().join(".agents/outrig");
    std::fs::create_dir_all(&agents_dir).expect("mkdir .agents/outrig");
    std::fs::write(repo_dir.path().join("HELLO.txt"), "hi\n").expect("write HELLO.txt");

    let image_ctx = tempfile::tempdir().expect("tempdir image context");
    let dockerfile = format!(
        "FROM docker.io/library/alpine:latest\n\
         RUN apk add --no-cache nodejs npm shadow\n\
         RUN npm install -g @modelcontextprotocol/server-filesystem\n\
         {}",
        label_line(
            embedded::LABEL_MCP,
            r#"{"fs":["mcp-server-filesystem","/workspace"]}"#,
        ),
    );
    std::fs::write(image_ctx.path().join("Dockerfile"), dockerfile).expect("write Dockerfile");

    let config_toml = format!(
        r#"
default-image = "smoke"

[images.smoke]
dockerfile = "{dockerfile}"
context = "{context}"
"#,
        dockerfile = image_ctx.path().join("Dockerfile").display(),
        context = image_ctx.path().display(),
    );
    std::fs::write(agents_dir.join("config.toml"), config_toml).expect("write config");

    let bin = env!("CARGO_BIN_EXE_outrig");
    let mut child = Command::new(bin)
        .args(["mcp"])
        .current_dir(repo_dir.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn outrig mcp");
    let child_stdin = child.stdin.take().expect("stdin piped");
    let child_stdout = child.stdout.take().expect("stdout piped");

    let work = async {
        let service = serve_client((), (child_stdout, child_stdin))
            .await
            .expect("serve_client");
        let listing = service
            .list_tools(Default::default())
            .await
            .expect("tools/list");
        let names: Vec<String> = listing
            .tools
            .iter()
            .map(|tool| tool.name.as_ref().to_string())
            .collect();
        assert!(
            names.iter().any(|name| name == "fs__list_directory"),
            "expected embedded fs tool in {names:?}",
        );
        let _ = service.cancel().await;
    };

    timeout(TEST_TIMEOUT, work)
        .await
        .expect("mcp server mode timed out");
    let status = timeout(TEST_TIMEOUT, child.wait())
        .await
        .expect("child wait timed out")
        .expect("child wait");
    assert!(status.success(), "outrig mcp exited with {status:?}");
}
