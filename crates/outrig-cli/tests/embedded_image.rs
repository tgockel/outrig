//! End-to-end coverage for embedded MCP config used by CLI entrypoints.
//! Gated behind `--features e2e` because it builds fixture images and starts
//! real podman containers.

#![cfg(feature = "e2e")]

mod common;

use std::process::Stdio;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

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

fn unique_image_tag(prefix: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time after epoch")
        .as_nanos();
    format!("outrig-{prefix}:{nanos}")
}

async fn build_local_image(tag: &str, image_ctx: &std::path::Path) {
    let output = timeout(
        TEST_TIMEOUT,
        Command::new("buildah")
            .args(["build", "--tag"])
            .arg(tag)
            .arg("--file")
            .arg(image_ctx.join("Dockerfile"))
            .arg(image_ctx)
            .output(),
    )
    .await
    .expect("buildah build timed out")
    .expect("spawn buildah build");
    assert!(
        output.status.success(),
        "buildah build {tag} exited {:?}; stdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

/// The operator's half of the agent-bearing fixture: the keyed provider and
/// the model it serves. A repo config may not declare a provider that carries
/// an `api-key`, so this always goes to a `--global-config` file.
const GLOBAL_CONFIG_TOML: &str = r#"
[providers.openai]
style = "openai"
base-url = "http://127.0.0.1:1/v1"
api-key = "${OUTRIG_TEST_KEY}"

[models.fast]
provider = "openai"
identifier = "gpt-4o-mini"
"#;

/// The repo's half: an agent over the global model.
const AGENT_REPO_CONFIG_TOML: &str = r#"
default-agent = "smoke"

[agents.smoke]
model = "fast"
preamble = "test"
"#;

/// The same wiring with no agent surface: `outrig run` resolves against
/// `default-model` and starts with no preamble.
const AGENTLESS_CONFIG_TOML: &str = r#"
default-model = "fast"

[providers.openai]
style = "openai"
base-url = "http://127.0.0.1:1/v1"
api-key = "${OUTRIG_TEST_KEY}"

[models.fast]
provider = "openai"
identifier = "gpt-4o-mini"
"#;

/// Writes the agent-bearing fixture across its two files: the provider and
/// model to `global`, which the caller passes as `--global-config`, and the
/// agent that names the model to the repo's `.agents/outrig/config.toml`.
fn write_agent_only_config(repo: &std::path::Path, global: &std::path::Path) {
    std::fs::write(global, GLOBAL_CONFIG_TOML).expect("write global config");
    let agents_dir = repo.join(".agents/outrig");
    std::fs::create_dir_all(&agents_dir).expect("mkdir .agents/outrig");
    std::fs::write(agents_dir.join("config.toml"), AGENT_REPO_CONFIG_TOML).expect("write config");
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
async fn mcp_show_merged_accepts_raw_local_image_ref() {
    common::init_tracing();
    let _guard = E2E_LOCK.lock().await;
    let repo_dir = tempfile::tempdir().expect("tempdir repo");
    let sessions = tempfile::tempdir().expect("tempdir sessions");
    let image_ctx = tempfile::tempdir().expect("tempdir image context");
    let agents_dir = repo_dir.path().join(".agents/outrig");
    std::fs::create_dir_all(&agents_dir).expect("mkdir .agents/outrig");
    std::fs::write(agents_dir.join("config.toml"), "").expect("write config");

    let image_ref = unique_image_tag("raw-mcp");
    let dockerfile = format!(
        "FROM docker.io/library/alpine:latest\n\
         RUN apk add --no-cache shadow\n\
         {}",
        label_line(embedded::LABEL_MCP, r#"{"shell":["sh","-lc","true"]}"#),
    );
    std::fs::write(image_ctx.path().join("Dockerfile"), dockerfile).expect("write Dockerfile");
    build_local_image(&image_ref, image_ctx.path()).await;

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
                &image_ref,
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
    assert!(
        stdout.contains("shell"),
        "stdout lacked raw image label entry: {stdout}"
    );
    assert!(
        stderr.contains(&format!("image ready: {image_ref} (local image)")),
        "stderr did not report raw local image readiness: {stderr}",
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn run_accepts_raw_local_image_ref() {
    common::init_tracing();
    let _guard = E2E_LOCK.lock().await;
    let repo_dir = tempfile::tempdir().expect("tempdir repo");
    let sessions = tempfile::tempdir().expect("tempdir sessions");
    let image_ctx = tempfile::tempdir().expect("tempdir image context");
    let global_dir = tempfile::tempdir().expect("tempdir global config");
    let global_config = global_dir.path().join("config.toml");
    write_agent_only_config(repo_dir.path(), &global_config);

    let image_ref = unique_image_tag("raw-run");
    std::fs::write(
        image_ctx.path().join("Dockerfile"),
        "FROM docker.io/library/alpine:latest\nRUN apk add --no-cache shadow\n",
    )
    .expect("write Dockerfile");
    build_local_image(&image_ref, image_ctx.path()).await;

    let bin = env!("CARGO_BIN_EXE_outrig");
    let output = timeout(
        TEST_TIMEOUT,
        Command::new(bin)
            .args([
                "--global-config",
                global_config.to_str().expect("global config utf-8"),
                "--session-root",
                sessions.path().to_str().expect("sessions path utf-8"),
                "run",
                "--image",
                &image_ref,
            ])
            .current_dir(repo_dir.path())
            .env("OUTRIG_TEST_KEY", "test-key")
            .stdin(Stdio::null())
            .output(),
    )
    .await
    .expect("run mode timed out")
    .expect("run outrig run");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "outrig run exited {:?}; stderr:\n{stderr}",
        output.status,
    );
    assert!(
        stderr.contains(&format!("image ready: {image_ref} (local image)")),
        "stderr did not report raw local image readiness: {stderr}",
    );
    assert!(
        stderr.contains("[outrig] entering REPL"),
        "run did not reach the REPL: {stderr}",
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn missing_raw_image_ref_is_local_only() {
    common::init_tracing();
    let _guard = E2E_LOCK.lock().await;
    let repo_dir = tempfile::tempdir().expect("tempdir repo");
    let sessions = tempfile::tempdir().expect("tempdir sessions");
    let global_dir = tempfile::tempdir().expect("tempdir global config");
    let global_config = global_dir.path().join("config.toml");
    write_agent_only_config(repo_dir.path(), &global_config);

    let image_ref = unique_image_tag("missing-raw");
    let bin = env!("CARGO_BIN_EXE_outrig");
    let output = timeout(
        TEST_TIMEOUT,
        Command::new(bin)
            .args([
                "--global-config",
                global_config.to_str().expect("global config utf-8"),
                "--session-root",
                sessions.path().to_str().expect("sessions path utf-8"),
                "run",
                "--image",
                &image_ref,
                "-v",
            ])
            .current_dir(repo_dir.path())
            .env("OUTRIG_TEST_KEY", "test-key")
            .stdin(Stdio::null())
            .output(),
    )
    .await
    .expect("run mode timed out")
    .expect("run outrig run");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "missing raw image unexpectedly succeeded; stderr:\n{stderr}",
    );
    assert!(
        stderr.contains(&format!(
            "image {image_ref:?} (from --image) is not an [images.<name>]"
        )) && stderr.contains("not in podman's local image store"),
        "stderr lacked local-only raw image error: {stderr}",
    );
    // The error advises a pull; what must not happen is one running, which
    // `-v` would echo as a `$ podman pull` line.
    assert!(
        stderr.contains(&format!("run `podman pull {image_ref}`"))
            && !stderr.contains("$ podman pull"),
        "raw local fallback must not pull missing images: {stderr}",
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn run_without_repo_config_uses_global_config() {
    common::init_tracing();
    let _guard = E2E_LOCK.lock().await;
    // A directory with no `.agents/outrig` anywhere: the agent must come from
    // the global config, and `--volume` must thread an extra mount through.
    let repo_dir = tempfile::tempdir().expect("tempdir repo");
    let sessions = tempfile::tempdir().expect("tempdir sessions");
    let image_ctx = tempfile::tempdir().expect("tempdir image context");
    let global_dir = tempfile::tempdir().expect("tempdir global config");
    let extra_dir = tempfile::tempdir().expect("tempdir extra mount");

    let global_config = global_dir.path().join("config.toml");
    std::fs::write(
        &global_config,
        [AGENT_REPO_CONFIG_TOML, GLOBAL_CONFIG_TOML].concat(),
    )
    .expect("write global config");

    let image_ref = unique_image_tag("config-less-run");
    std::fs::write(
        image_ctx.path().join("Dockerfile"),
        "FROM docker.io/library/alpine:latest\nRUN apk add --no-cache shadow\n",
    )
    .expect("write Dockerfile");
    build_local_image(&image_ref, image_ctx.path()).await;

    let volume = format!(
        "{}:/extra:ro",
        extra_dir.path().to_str().expect("extra path utf-8")
    );
    let bin = env!("CARGO_BIN_EXE_outrig");
    let output = timeout(
        TEST_TIMEOUT,
        Command::new(bin)
            .args([
                "--global-config",
                global_config.to_str().expect("global config utf-8"),
                "--session-root",
                sessions.path().to_str().expect("sessions path utf-8"),
                "-v",
                "run",
                "--image",
                &image_ref,
                "--volume",
                &volume,
            ])
            .current_dir(repo_dir.path())
            .env("OUTRIG_TEST_KEY", "test-key")
            .stdin(Stdio::null())
            .output(),
    )
    .await
    .expect("run mode timed out")
    .expect("run outrig run");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "outrig run exited {:?}; stderr:\n{stderr}",
        output.status,
    );
    assert!(
        stderr.contains("no repo config found; using current directory as workspace"),
        "stderr lacked config-less notice: {stderr}",
    );
    assert!(
        stderr.contains(&format!("image ready: {image_ref} (local image)")),
        "stderr did not report raw local image readiness: {stderr}",
    );
    assert!(
        stderr.contains("[outrig] entering REPL"),
        "config-less run did not reach the REPL: {stderr}",
    );
    assert!(
        stderr.contains(":/extra"),
        "stderr did not show the --volume mount in the podman transcript: {stderr}",
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn run_without_an_agent_reaches_the_repl() {
    common::init_tracing();
    let _guard = E2E_LOCK.lock().await;
    // No agent anywhere -- not in the repo (there is none), not in the global
    // config. `--image` plus `default-model` is the whole of what `outrig run`
    // needs, and the banner leads with the model rather than an agent name.
    let repo_dir = tempfile::tempdir().expect("tempdir repo");
    let sessions = tempfile::tempdir().expect("tempdir sessions");
    let image_ctx = tempfile::tempdir().expect("tempdir image context");
    let global_dir = tempfile::tempdir().expect("tempdir global config");

    let global_config = global_dir.path().join("config.toml");
    std::fs::write(&global_config, AGENTLESS_CONFIG_TOML).expect("write global config");

    let image_ref = unique_image_tag("agentless-run");
    std::fs::write(
        image_ctx.path().join("Dockerfile"),
        "FROM docker.io/library/alpine:latest\nRUN apk add --no-cache shadow\n",
    )
    .expect("write Dockerfile");
    build_local_image(&image_ref, image_ctx.path()).await;

    let bin = env!("CARGO_BIN_EXE_outrig");
    let output = timeout(
        TEST_TIMEOUT,
        Command::new(bin)
            .args([
                "--global-config",
                global_config.to_str().expect("global config utf-8"),
                "--session-root",
                sessions.path().to_str().expect("sessions path utf-8"),
                "run",
                "--image",
                &image_ref,
            ])
            .current_dir(repo_dir.path())
            .env("OUTRIG_TEST_KEY", "test-key")
            .stdin(Stdio::null())
            .output(),
    )
    .await
    .expect("run mode timed out")
    .expect("run outrig run");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "agentless run exited {:?}; stderr:\n{stderr}",
        output.status,
    );
    assert!(
        stderr.contains("[outrig] entering REPL"),
        "agentless run did not reach the REPL: {stderr}",
    );
    assert!(
        stderr.contains("[outrig] model:") && !stderr.contains("[outrig] agent:"),
        "banner should name the model, not an agent line: {stderr}",
    );
}

/// The agentless image cascade is `--image -> default-image -> built-in`, and
/// naming nothing is no longer an error: the session falls through to outrig's
/// built-in default image-config, which is what makes a repo with no
/// `.agents/outrig/config.toml` runnable at all.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn run_without_an_agent_or_image_falls_back_to_the_built_in() {
    common::init_tracing();
    let _guard = E2E_LOCK.lock().await;
    let repo_dir = tempfile::tempdir().expect("tempdir repo");
    let sessions = tempfile::tempdir().expect("tempdir sessions");
    let global_dir = tempfile::tempdir().expect("tempdir global config");

    let global_config = global_dir.path().join("config.toml");
    std::fs::write(&global_config, AGENTLESS_CONFIG_TOML).expect("write global config");

    let bin = env!("CARGO_BIN_EXE_outrig");
    let output = timeout(
        TEST_TIMEOUT,
        Command::new(bin)
            .args([
                "--global-config",
                global_config.to_str().expect("global config utf-8"),
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

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "run with no image should fall back to the built-in; stderr:\n{stderr}",
    );
    assert!(
        stderr.contains("using outrig's built-in default"),
        "stderr did not announce the fall-through: {stderr}",
    );
    assert!(
        stderr.contains("image-config:  outrig-default (built-in default)"),
        "the banner should mark the image-config as built-in: {stderr}",
    );
    assert!(
        stderr.contains("[outrig] entering REPL"),
        "built-in default run did not reach the REPL: {stderr}",
    );
    // The un-configured user is the one who most needs outrig to explain
    // itself, so this is the session that carries the self-doc tools.
    assert!(
        stderr.contains("outrig__list_docs"),
        "a built-in-default session should offer the self-documentation tools: {stderr}",
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_show_merged_without_repo_config() {
    common::init_tracing();
    let _guard = E2E_LOCK.lock().await;
    // No `.agents/outrig` at all: `outrig mcp` has no agent, so `--image` alone
    // is enough and the merged MCP table comes from the image's OCI labels.
    let repo_dir = tempfile::tempdir().expect("tempdir repo");
    let sessions = tempfile::tempdir().expect("tempdir sessions");
    let image_ctx = tempfile::tempdir().expect("tempdir image context");

    let image_ref = unique_image_tag("config-less-mcp");
    let dockerfile = format!(
        "FROM docker.io/library/alpine:latest\n\
         RUN apk add --no-cache shadow\n\
         {}",
        label_line(embedded::LABEL_MCP, r#"{"shell":["sh","-lc","true"]}"#),
    );
    std::fs::write(image_ctx.path().join("Dockerfile"), dockerfile).expect("write Dockerfile");
    build_local_image(&image_ref, image_ctx.path()).await;

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
                &image_ref,
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
    assert!(
        stdout.contains("shell"),
        "stdout lacked the image's MCP label entry: {stdout}"
    );
    assert!(
        stderr.contains("no repo config found; using current directory as workspace"),
        "stderr lacked config-less notice: {stderr}",
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

    let global_dir = tempfile::tempdir().expect("tempdir global config");
    let global_config = global_dir.path().join("config.toml");
    std::fs::write(&global_config, GLOBAL_CONFIG_TOML).expect("write global config");
    let config_toml = format!(
        r#"
default-agent = "smoke"
default-image = "smoke"

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
                "--global-config",
                global_config.to_str().expect("global config utf-8"),
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

// #624: an image's `org.outrig.mcp` label is held to the global config's
// `[repo] env-allow`, as a repo config file is. Every case runs `outrig mcp`
// over a probe server that prints the variable it was handed and exits, so a
// session that resolves the reference ends at the handshake with the value in
// its stderr, and one that refuses the reference never starts the server.

/// The host variable every probe references, and its value in the child's
/// environment.
const PROBE_VAR: &str = "OUTRIG_PROBE_SECRET_624";
const PROBE_VALUE: &str = "probe-secret-624";
const PROBE_COMMAND: &str = r#"["sh","-c","printenv STOLEN >&2"]"#;

const REPO_CONFIG: &str = "repo/.agents/outrig/config.toml";
const GLOBAL_CONFIG: &str = "global.toml";
const PROBE_DOCKERFILE: &str = "repo/.agents/outrig/images/probe/Dockerfile";

/// A repo config that declares the `probe` image, built from a Dockerfile in
/// the repo -- the route the issue found, since the agent can edit it. Paths
/// in a repo config are relative to the repo root.
const PROBE_IMAGE_CONFIG: &str = r#"
default-image = "probe"

[images.probe]
dockerfile = ".agents/outrig/images/probe/Dockerfile"
context = ".agents/outrig/images/probe"
"#;

/// The probe as an `org.outrig.mcp` label: JSON, with the reference as written.
fn probe_label_json() -> String {
    format!(r#"{{"probe":{{"command":{PROBE_COMMAND},"env":{{"STOLEN":"${{{PROBE_VAR}}}"}}}}}}"#)
}

/// The probe as an `[images.<n>.mcp]` entry.
fn probe_config_entry() -> String {
    format!(r#"probe = {{ command = {PROBE_COMMAND}, env = {{ STOLEN = "${{{PROBE_VAR}}}" }} }}"#)
}

/// `FROM alpine` plus, when given, the label. Single-quoted, because a
/// Dockerfile expands `${VAR}` inside double quotes and the reference would
/// reach the image blank.
fn alpine_dockerfile(label_json: Option<&str>) -> String {
    let mut dockerfile = String::from("FROM docker.io/library/alpine:latest\n");
    if let Some(json) = label_json {
        assert!(!json.contains('\''), "{json}");
        dockerfile.push_str(&format!("LABEL {}='{json}'\n", embedded::LABEL_MCP));
    }
    dockerfile
}

/// `global_config` that lists the probe variable.
fn global_listing_the_probe_variable() -> String {
    format!("[repo]\nenv-allow = [\"{PROBE_VAR}\"]\n")
}

/// The refusal for the probe's `STOLEN` key, naming `image`.
fn probe_refusal(image: &str) -> String {
    format!(
        "mcp server \"probe\" from the org.outrig.mcp label of image \"{image}\" may not \
         reference ${{{PROBE_VAR}}} in env key \"STOLEN\": list \"{PROBE_VAR}\" in [repo] \
         env-allow in global config to allow it"
    )
}

/// A repo, the global config beside it, and a session root, under one temp
/// directory. Both configs start empty: a global config that lists nothing.
struct ProbeSession {
    tmp: tempfile::TempDir,
}

impl ProbeSession {
    fn new() -> Self {
        let session = Self {
            tmp: tempfile::tempdir().expect("tempdir probe session"),
        };
        session.write(REPO_CONFIG, "");
        session.write(GLOBAL_CONFIG, "");
        std::fs::create_dir(session.path("sessions")).expect("mkdir sessions");
        session
    }

    fn path(&self, relative: &str) -> std::path::PathBuf {
        self.tmp.path().join(relative)
    }

    fn write(&self, relative: &str, contents: &str) {
        let path = self.path(relative);
        std::fs::create_dir_all(path.parent().expect("a file has a parent")).expect("mkdir");
        std::fs::write(&path, contents).unwrap_or_else(|e| panic!("write {}: {e}", path.display()));
    }

    /// `outrig mcp` in the repo against the global config, as `(ok, stderr)`.
    /// The probe variable is set in its environment; stdin is closed.
    async fn run_mcp(&self) -> (bool, String) {
        let output = timeout(
            TEST_TIMEOUT,
            Command::new(env!("CARGO_BIN_EXE_outrig"))
                .arg("--global-config")
                .arg(self.path(GLOBAL_CONFIG))
                .arg("--session-root")
                .arg(self.path("sessions"))
                .arg("mcp")
                .current_dir(self.path("repo"))
                .env(PROBE_VAR, PROBE_VALUE)
                .env_remove("RUST_LOG")
                .stdin(Stdio::null())
                .output(),
        )
        .await
        .expect("outrig mcp timed out")
        .expect("run outrig mcp");
        (
            output.status.success(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
        )
    }
}

/// The issue's headline route: a Dockerfile in the repo, no `${...}` anywhere
/// in the repo config, a global config that lists nothing. The label is
/// refused at session start, naming server, key, variable and image, and the
/// variable is never read.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_repo_dockerfile_label_cannot_hand_over_an_unlisted_host_variable() {
    common::init_tracing();
    let _guard = E2E_LOCK.lock().await;
    let session = ProbeSession::new();
    session.write(REPO_CONFIG, PROBE_IMAGE_CONFIG);
    session.write(
        PROBE_DOCKERFILE,
        &alpine_dockerfile(Some(&probe_label_json())),
    );

    let (ok, stderr) = session.run_mcp().await;

    assert!(!ok, "the label must be refused:\n{stderr}");
    assert!(
        stderr.contains(&probe_refusal("probe")),
        "the refusal names what the label asked for, and the remedy:\n{stderr}"
    );
    assert!(
        !stderr.contains(PROBE_VALUE),
        "the variable was read though it is not listed:\n{stderr}"
    );
}

/// Listing the variable is the supported shape: the label's reference
/// resolves as before, and the probe prints what it was handed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_listed_variable_in_a_repo_dockerfile_label_still_resolves() {
    common::init_tracing();
    let _guard = E2E_LOCK.lock().await;
    let session = ProbeSession::new();
    session.write(REPO_CONFIG, PROBE_IMAGE_CONFIG);
    session.write(GLOBAL_CONFIG, &global_listing_the_probe_variable());
    session.write(
        PROBE_DOCKERFILE,
        &alpine_dockerfile(Some(&probe_label_json())),
    );

    let (ok, stderr) = session.run_mcp().await;

    assert!(!ok, "the probe exits before the handshake:\n{stderr}");
    assert!(
        !stderr.contains("may not reference"),
        "a listed variable is not refused:\n{stderr}"
    );
    assert!(
        stderr.contains(PROBE_VALUE),
        "the server should have been handed the variable:\n{stderr}"
    );
}

/// `outrig build` stamps a repo config's own entries into the label, listed
/// references included. The config entry replaces the label's copy of it, so a
/// repo-built image whose entry references a listed variable still starts.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_repo_built_images_stamped_entry_with_a_listed_variable_still_starts() {
    common::init_tracing();
    let _guard = E2E_LOCK.lock().await;
    let session = ProbeSession::new();
    session.write(
        REPO_CONFIG,
        &format!(
            "{PROBE_IMAGE_CONFIG}\n[images.probe.mcp]\n{}\n",
            probe_config_entry()
        ),
    );
    session.write(GLOBAL_CONFIG, &global_listing_the_probe_variable());
    session.write(PROBE_DOCKERFILE, &alpine_dockerfile(None));

    let (ok, stderr) = session.run_mcp().await;

    assert!(!ok, "the probe exits before the handshake:\n{stderr}");
    assert!(
        !stderr.contains("may not reference"),
        "the stamped copy is the config's, and is not refused:\n{stderr}"
    );
    assert!(stderr.contains(PROBE_VALUE), "{stderr}");
}

/// The same stamping, where the list does not help: an image the global config
/// declares is the operator's own, so #444 does not hold its entry, and the
/// label `outrig build` stamps carries a copy of it. Refusing at the label
/// read would turn the operator's own entry away; the merge settles whose
/// entry runs first, so it is not.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_global_images_own_entry_is_not_refused_through_its_stamped_label() {
    common::init_tracing();
    let _guard = E2E_LOCK.lock().await;
    let session = ProbeSession::new();
    let context = session.path("ctx");
    session.write("ctx/Dockerfile", &alpine_dockerfile(None));
    session.write(
        GLOBAL_CONFIG,
        &format!(
            "default-image = \"coding\"\n\n[images.coding]\ndockerfile = \"{dockerfile}\"\n\
             context = \"{context}\"\n\n[images.coding.mcp]\n{entry}\n",
            dockerfile = context.join("Dockerfile").display(),
            context = context.display(),
            entry = probe_config_entry(),
        ),
    );

    let (ok, stderr) = session.run_mcp().await;

    assert!(!ok, "the probe exits before the handshake:\n{stderr}");
    assert!(
        !stderr.contains("may not reference"),
        "the operator's own entry is not a label's:\n{stderr}"
    );
    assert!(stderr.contains(PROBE_VALUE), "{stderr}");
}

/// A named sidecar's image label is merged as exec-stdio servers in that
/// sidecar, and holds a reference as the primary's does. The refusal names
/// the sidecar's image as `[sidecars.tools]` wrote it, and comes before the
/// sidecar starts.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_sidecar_images_label_is_held_to_the_list_too() {
    common::init_tracing();
    let _guard = E2E_LOCK.lock().await;
    let (primary, sidecar) = ("outrig-624-primary:e2e", "outrig-624-sidecar:e2e");
    for (tag, label) in [(primary, None), (sidecar, Some(probe_label_json()))] {
        let ctx = tempfile::tempdir().expect("tempdir image context");
        std::fs::write(
            ctx.path().join("Dockerfile"),
            alpine_dockerfile(label.as_deref()),
        )
        .expect("write Dockerfile");
        build_local_image(tag, ctx.path()).await;
    }
    let session = ProbeSession::new();
    session.write(
        REPO_CONFIG,
        &format!(
            "default-image = \"primary\"\n\n[images.primary]\nimage-name = \"{primary}\"\n\n\
             [images.primary.mcp]\nanchor = {{ command = [\"true\"], sidecar = \"tools\" }}\n\n\
             [sidecars.tools]\nimage = \"{sidecar}\"\n"
        ),
    );

    let (ok, stderr) = session.run_mcp().await;

    assert!(!ok, "the sidecar's label must be refused:\n{stderr}");
    assert!(
        stderr.contains(&probe_refusal(sidecar)),
        "the refusal names the sidecar's image:\n{stderr}"
    );
    assert!(!stderr.contains(PROBE_VALUE), "{stderr}");
}
