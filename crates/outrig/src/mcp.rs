//! rmcp client over `podman exec` stdio.
//!
//! Spawns the MCP server as a child of `podman exec -i` against a running
//! [`Container`], hands the resulting `(stdout, stdin)` pair to
//! [`rmcp::service::serve_client`], and exposes our own thin facade so
//! callers don't depend on rmcp directly. Per-server stderr is redirected to
//! `<log_dir>/<name>.stderr` -- written to the file even if the server
//! crashes during the `initialize` handshake.
//!
//! The child handle is owned by [`McpClient`] (not by rmcp's transport
//! wrapper) so [`McpClient::shutdown`] can implement the close-stdin -> wait
//! grace -> kill sequence the MCP spec calls for.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use rmcp::model::{CallToolRequestParams, ContentBlock, ResourceContents};
use rmcp::service::{RoleClient, RunningService, serve_client};
use serde_json::Value;
use tokio::process::Child;

use crate::config::{EnvValue, McpServerSpec};
use crate::container::{Container, embedded::McpDeclarationSource};
use crate::error::{OutrigError, Result};
use crate::process::{Cmd, Transcript};

const SHUTDOWN_GRACE: Duration = Duration::from_secs(2);

#[derive(Debug, Clone)]
pub struct McpTool {
    pub name: String,
    pub description: Option<String>,
    pub input_schema: Value,
}

#[derive(Debug, Clone)]
pub struct McpToolResult {
    pub content_text: String,
    pub is_error: bool,
}

/// On `Drop` without an explicit [`McpClient::shutdown`], the underlying
/// `tokio::process::Child` was spawned with `kill_on_drop(true)`, so the
/// server gets SIGKILLed -- not graceful, but no leaked process.
#[derive(Debug)]
pub struct McpClient {
    name: String,
    service: RunningService<RoleClient, ()>,
    child: Child,
}

impl McpClient {
    /// Spawn `server_cfg`'s command via `podman exec -i` against `container`,
    /// redirect its stderr to `<log_dir>/<name>.stderr`, and drive the MCP
    /// `initialize` handshake. Returns the live client on success.
    ///
    /// `extra_env` carries `--env` CLI overlay entries already merged for this
    /// specific server (global + per-server). They are layered on top of the
    /// config-file env (overriding on key conflict) before resolution.
    ///
    /// `log_dir` is created (and any missing parents) if it doesn't exist.
    /// The `name` is used both for the stderr filename and for diagnostic
    /// messages; callers should pass the server's local config name (e.g.
    /// `"fs"`).
    pub async fn connect_via_podman_exec(
        container: &Container,
        server_cfg: &McpServerSpec,
        name: &str,
        log_dir: &Path,
        extra_env: &BTreeMap<String, EnvValue>,
    ) -> Result<Self> {
        Self::connect_via_podman_exec_inner(container, server_cfg, name, None, log_dir, extra_env)
            .await
    }

    pub async fn connect_via_podman_exec_with_source(
        container: &Container,
        server_cfg: &McpServerSpec,
        name: &str,
        declaration_source: McpDeclarationSource,
        log_dir: &Path,
        extra_env: &BTreeMap<String, EnvValue>,
    ) -> Result<Self> {
        Self::connect_via_podman_exec_inner(
            container,
            server_cfg,
            name,
            Some(declaration_source.description()),
            log_dir,
            extra_env,
        )
        .await
    }

    async fn connect_via_podman_exec_inner(
        container: &Container,
        server_cfg: &McpServerSpec,
        name: &str,
        declaration_source: Option<&'static str>,
        log_dir: &Path,
        extra_env: &BTreeMap<String, EnvValue>,
    ) -> Result<Self> {
        let (command, env_spec) = server_cfg.normalize();
        let env = resolve_mcp_env(name, env_spec, extra_env)?;
        let exec_cmd = container.build_exec_argv(&command, &env);
        Self::connect_stdio_cmd(
            exec_cmd,
            name,
            declaration_source,
            log_dir,
            container.transcript(),
            &command,
        )
        .await
    }

    /// Connect an entrypoint-stdio server: spawn
    /// `podman start --attach --interactive <container>` as the owned child,
    /// so the image's ENTRYPOINT becomes the server and container lifetime
    /// equals server lifetime. The container must be created+initialized
    /// (`Container::create_initialized`) with the server's env baked in --
    /// `podman start` carries no `--env`.
    ///
    /// [`McpClient::shutdown`]'s close-stdin -> grace -> kill sequence works
    /// unchanged: EOF on the attached stdin reaches the entrypoint, which
    /// exits and takes the container with it (`--rm`). A server that ignores
    /// EOF gets the *attach child* SIGKILLed, which does not stop the
    /// container itself -- session teardown's `Container::stop` covers that.
    pub async fn connect_via_podman_start(
        container: &Container,
        name: &str,
        declaration_source: McpDeclarationSource,
        log_dir: &Path,
    ) -> Result<Self> {
        let cmd = Cmd::new("podman")
            .args(["start", "--attach", "--interactive"])
            .arg(container.name());
        // Derived from the one Cmd so failure diagnostics can't drift from
        // what actually ran.
        let argv: Vec<String> = std::iter::once(cmd.program.to_string())
            .chain(cmd.args.iter().map(|a| a.to_string_lossy().into_owned()))
            .collect();
        Self::connect_stdio_cmd(
            cmd,
            name,
            Some(declaration_source.description()),
            log_dir,
            container.transcript(),
            &argv,
        )
        .await
    }

    /// Transport-agnostic connection tail: spawn `cmd` with piped stdio and
    /// stderr redirected to `<log_dir>/<name>.stderr`, then drive the MCP
    /// `initialize` handshake. `display_command` is what a startup failure
    /// reports as the attempted command.
    async fn connect_stdio_cmd(
        cmd: Cmd,
        name: &str,
        declaration_source: Option<&'static str>,
        log_dir: &Path,
        transcript: Option<Transcript>,
        display_command: &[String],
    ) -> Result<Self> {
        tokio::fs::create_dir_all(log_dir).await?;
        let stderr_path = log_dir.join(format!("{name}.stderr"));
        let stderr_file = tokio::fs::File::create(&stderr_path).await?;
        let stderr_std = stderr_file.into_std().await;

        if let Some(transcript) = transcript {
            transcript
                .line("podman", &format!("$ {}", cmd.render()))
                .await?;
        }

        let mut cmd = cmd.to_tokio_command();
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::from(stderr_std))
            .kill_on_drop(true);

        let mut child = cmd.spawn()?;
        let stdin = child.stdin.take().expect("stdin was piped above");
        let stdout = child.stdout.take().expect("stdout was piped above");

        // The unwrapped pipe halves go straight to rmcp via the blanket
        // `IntoTransport for (R, W)` impl. Going through
        // `rmcp::transport::TokioChildProcess::new` would spawn its own child
        // internally, leaving us no `Child` handle for graceful shutdown.
        let service = match serve_client((), (stdout, stdin)).await {
            Ok(s) => s,
            Err(source) => {
                return Err(enrich_startup_error(
                    name,
                    declaration_source,
                    display_command,
                    &stderr_path,
                    &mut child,
                    source,
                )
                .await);
            }
        };

        Ok(Self {
            name: name.to_string(),
            service,
            child,
        })
    }

    /// The local name this client was constructed with (e.g. `"fs"`).
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Issue an MCP `tools/list` (paginating internally) and project the
    /// results into our own [`McpTool`] type.
    pub async fn list_tools(&self) -> Result<Vec<McpTool>> {
        let tools = self.service.list_all_tools().await.map_err(|source| {
            OutrigError::McpToolsListFailed {
                name: self.name.clone(),
                source: Box::new(source),
            }
        })?;
        Ok(tools
            .into_iter()
            .map(|t| {
                let input_schema = t.schema_as_json_value();
                let description = t.description.and_then(|description| {
                    (!description.is_empty()).then(|| description.into_owned())
                });
                McpTool {
                    name: t.name.into_owned(),
                    description,
                    input_schema,
                }
            })
            .collect())
    }

    /// Issue an MCP `tools/call`. `args` must be a JSON object (forwarded as
    /// the call's `arguments`) or `Value::Null` (no arguments). Other shapes
    /// are a programmer error and return [`OutrigError::McpArgsNotObject`].
    /// Content blocks in the response are flattened into a single string;
    /// `is_error` mirrors the server's flag.
    pub async fn call_tool(&self, name: &str, args: Value) -> Result<McpToolResult> {
        let arguments = match args {
            Value::Object(map) => Some(map),
            Value::Null => None,
            other => {
                return Err(OutrigError::McpArgsNotObject {
                    kind: kind_of(&other),
                });
            }
        };

        let mut request = CallToolRequestParams::new(name.to_string());
        if let Some(arguments) = arguments {
            request = request.with_arguments(arguments);
        }
        let result = self.service.call_tool(request).await?;

        let mut content_text = String::new();
        for (i, content) in result.content.iter().enumerate() {
            if i > 0 {
                content_text.push('\n');
            }
            match content {
                ContentBlock::Text(t) => content_text.push_str(&t.text),
                ContentBlock::Image(img) => {
                    let _ = write!(
                        content_text,
                        "[image: {}, {} base64 bytes]",
                        img.mime_type,
                        img.data.len()
                    );
                }
                ContentBlock::Resource(r) => match &r.resource {
                    ResourceContents::TextResourceContents { text, .. } => {
                        content_text.push_str(text)
                    }
                    ResourceContents::BlobResourceContents {
                        mime_type, blob, ..
                    } => {
                        let mime = mime_type.as_deref().unwrap_or("application/octet-stream");
                        let _ = write!(content_text, "[blob: {mime}, {} base64 bytes]", blob.len());
                    }
                    _ => content_text.push_str("[unsupported resource contents]"),
                },
                ContentBlock::Audio(audio) => {
                    let _ = write!(
                        content_text,
                        "[audio: {}, {} base64 bytes]",
                        audio.mime_type,
                        audio.data.len()
                    );
                }
                ContentBlock::ResourceLink(link) => {
                    let _ = write!(content_text, "[resource link: {}]", link.uri);
                }
                _ => content_text.push_str("[unsupported content block]"),
            }
        }

        Ok(McpToolResult {
            content_text,
            is_error: result.is_error.unwrap_or(false),
        })
    }

    /// Cancel the rmcp service (which closes the child's stdin -- the MCP
    /// spec's normal shutdown signal), wait up to `SHUTDOWN_GRACE` for the
    /// server to exit on its own, then SIGKILL if it doesn't.
    pub async fn shutdown(self) -> Result<()> {
        let Self {
            service, mut child, ..
        } = self;

        // A `JoinError` here just means the rmcp task panicked, which doesn't
        // affect our ability to clean up the child below. Bound cancellation
        // too: if the owning container disappeared, the transport task can be
        // wedged behind already-broken podman exec pipes.
        let _ = tokio::time::timeout(SHUTDOWN_GRACE, service.cancel()).await;

        match tokio::time::timeout(SHUTDOWN_GRACE, child.wait()).await {
            Ok(Ok(_)) => Ok(()),
            Ok(Err(e)) => Err(e.into()),
            Err(_) => {
                let _ = child.start_kill();
                let _ = tokio::time::timeout(SHUTDOWN_GRACE, child.wait()).await;
                Ok(())
            }
        }
    }
}

/// Layer the CLI `--env` overlay onto the config-file env (overlay wins on
/// key conflict) and resolve each value -- literals verbatim, `${VAR}` refs
/// from the host environment. `name` labels resolution failures with the
/// owning server. Shared by both stdio transports: exec-stdio resolves at
/// connect time (`podman exec --env`), entrypoint-stdio at container-create
/// time (`podman create --env`).
pub fn resolve_mcp_env(
    name: &str,
    config_env: BTreeMap<String, EnvValue>,
    extra_env: &BTreeMap<String, EnvValue>,
) -> Result<BTreeMap<String, String>> {
    let mut env_spec = config_env;
    for (key, value) in extra_env {
        env_spec.insert(key.clone(), value.clone());
    }
    let mut env = BTreeMap::new();
    for (key, value) in env_spec {
        let resolved = value
            .resolve()
            .map_err(|source| OutrigError::McpEnvResolveFailed {
                name: name.to_string(),
                key: key.clone(),
                source,
            })?;
        env.insert(key, resolved);
    }
    Ok(env)
}

/// Convert rmcp's bare transport error from `serve_client` into a richer
/// `McpStartupFailed` carrying the server name, the command we tried to run,
/// the child's exit status (so the user sees *why* the pipe closed), and a
/// tail of whatever the child wrote to stderr. The brief `child.wait()`
/// timeout lets a child that crashed mid-write flush its last few bytes
/// before we read them.
async fn enrich_startup_error(
    name: &str,
    declaration_source: Option<&str>,
    command: &[String],
    stderr_path: &Path,
    child: &mut Child,
    source: rmcp::service::ClientInitializeError,
) -> OutrigError {
    let exit_status = match tokio::time::timeout(Duration::from_millis(250), child.wait()).await {
        Ok(Ok(status)) => Some(status),
        _ => None,
    };
    let exit = format_exit(exit_status);
    let stderr_tail = read_stderr_tail(stderr_path).await;

    if !exit_status.is_some_and(|s| s.success()) {
        tracing::error!(
            target: "outrig::mcp",
            server = name,
            "mcp server {name:?} terminated before initialize ({exit}); \
             see {} for details",
            stderr_path.display()
        );
    }

    OutrigError::McpStartupFailed(Box::new(crate::error::McpStartupFailure {
        name: name.to_string(),
        declaration_source: declaration_source.map(str::to_string),
        command: render_command(command),
        exit,
        stderr_path: stderr_path.to_path_buf(),
        stderr_tail,
        source: Box::new(source),
    }))
}

fn format_exit(status: Option<std::process::ExitStatus>) -> String {
    let Some(status) = status else {
        return "still running (wait timed out)".to_string();
    };
    if let Some(code) = status.code() {
        return format!("code {code}");
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(sig) = status.signal() {
            return format!("signal {sig}");
        }
    }
    "terminated".to_string()
}

async fn read_stderr_tail(path: &Path) -> String {
    use tokio::io::{AsyncReadExt, AsyncSeekExt, SeekFrom};
    const MAX: u64 = 2048;

    async fn inner(path: &Path) -> std::io::Result<Vec<u8>> {
        let mut file = tokio::fs::File::open(path).await?;
        let len = file.metadata().await?.len();
        if len > MAX {
            file.seek(SeekFrom::End(-(MAX as i64))).await?;
        }
        let mut buf = Vec::with_capacity(len.min(MAX) as usize);
        file.read_to_end(&mut buf).await?;
        Ok(buf)
    }

    match inner(path).await {
        Ok(bytes) if bytes.is_empty() => "(empty)".to_string(),
        Ok(bytes) => String::from_utf8_lossy(&bytes).trim_end().to_string(),
        Err(_) => "(could not read stderr file)".to_string(),
    }
}

fn render_command(argv: &[String]) -> String {
    argv.iter()
        .map(|a| {
            if a.is_empty()
                || a.chars()
                    .any(|c| c.is_whitespace() || matches!(c, '\'' | '"' | '$' | '`' | '\\'))
            {
                format!("'{}'", a.replace('\'', "'\\''"))
            } else {
                a.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

pub(crate) fn kind_of(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_mcp_env_overlay_wins_and_resolves_refs() {
        // SAFETY: test-local var; std::env::set_var is unsafe in edition 2024
        // because of thread-unsafety, acceptable in this single-purpose test.
        unsafe { std::env::set_var("OUTRIG_TEST_MCP_ENV", "from-host") };
        let config_env = BTreeMap::from([
            (
                "KEEP".to_string(),
                EnvValue::from_raw("literal".to_string()),
            ),
            ("BOTH".to_string(), EnvValue::from_raw("config".to_string())),
        ]);
        let extra_env = BTreeMap::from([
            (
                "BOTH".to_string(),
                EnvValue::from_raw("overlay".to_string()),
            ),
            (
                "REF".to_string(),
                EnvValue::from_raw("${OUTRIG_TEST_MCP_ENV}".to_string()),
            ),
        ]);

        let env = resolve_mcp_env("svc", config_env, &extra_env).expect("resolves");
        assert_eq!(env["KEEP"], "literal");
        assert_eq!(env["BOTH"], "overlay");
        assert_eq!(env["REF"], "from-host");
    }

    #[test]
    fn resolve_mcp_env_missing_ref_names_server_and_key() {
        let config_env = BTreeMap::from([(
            "TOKEN".to_string(),
            EnvValue::from_raw("${OUTRIG_TEST_MCP_ENV_MISSING}".to_string()),
        )]);
        let err = resolve_mcp_env("svc", config_env, &BTreeMap::new())
            .expect_err("unset host var must fail resolution");
        let OutrigError::McpEnvResolveFailed { name, key, .. } = &err else {
            panic!("expected McpEnvResolveFailed, got {err:?}");
        };
        assert_eq!(name, "svc");
        assert_eq!(key, "TOKEN");
    }

    #[test]
    fn render_command_passes_through_simple_argv() {
        let argv = vec![
            "mcp-server-git".to_string(),
            "--repository".to_string(),
            "/workspace".to_string(),
        ];
        assert_eq!(
            render_command(&argv),
            "mcp-server-git --repository /workspace"
        );
    }

    #[test]
    fn render_command_quotes_args_with_whitespace_or_specials() {
        let argv = vec![
            "echo".to_string(),
            "hello world".to_string(),
            "it's".to_string(),
            "".to_string(),
        ];
        assert_eq!(render_command(&argv), "echo 'hello world' 'it'\\''s' ''");
    }

    #[tokio::test]
    async fn read_stderr_tail_handles_empty_missing_and_long() {
        let dir = tempfile::tempdir().unwrap();

        let missing = dir.path().join("nope.stderr");
        assert_eq!(
            read_stderr_tail(&missing).await,
            "(could not read stderr file)"
        );

        let empty = dir.path().join("empty.stderr");
        tokio::fs::write(&empty, b"").await.unwrap();
        assert_eq!(read_stderr_tail(&empty).await, "(empty)");

        let normal = dir.path().join("normal.stderr");
        tokio::fs::write(&normal, b"line one\nline two\n")
            .await
            .unwrap();
        assert_eq!(read_stderr_tail(&normal).await, "line one\nline two");

        let big = dir.path().join("big.stderr");
        let payload: Vec<u8> = (0..10_000).map(|i| b'A' + (i % 26) as u8).collect();
        tokio::fs::write(&big, &payload).await.unwrap();
        let tail = read_stderr_tail(&big).await;
        assert!(tail.len() <= 2048, "tail was {} bytes", tail.len());
        assert!(payload.ends_with(tail.trim_end().as_bytes()));
    }

    #[tokio::test]
    async fn enrich_startup_error_carries_name_command_and_stderr() {
        // Spawn `false` (fast, deterministic non-zero exit) with stderr piped
        // into a temp file, then drive the helper as if rmcp had returned EOF.
        let dir = tempfile::tempdir().unwrap();
        let stderr_path = dir.path().join("svc.stderr");
        let stderr_file = tokio::fs::File::create(&stderr_path).await.unwrap();

        let mut child = tokio::process::Command::new("sh")
            .args(["-c", "echo boom 1>&2; exit 7"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::from(stderr_file.into_std().await))
            .kill_on_drop(true)
            .spawn()
            .unwrap();

        let argv = vec![
            "sh".to_string(),
            "-c".to_string(),
            "echo boom 1>&2; exit 7".to_string(),
        ];
        let source = rmcp::service::ClientInitializeError::ConnectionClosed(
            "expect initialize response".to_string(),
        );

        let err = enrich_startup_error(
            "svc",
            Some("launch spec"),
            &argv,
            &stderr_path,
            &mut child,
            source,
        )
        .await;

        let OutrigError::McpStartupFailed(payload) = &err else {
            panic!("expected McpStartupFailed, got {err:?}");
        };
        assert_eq!(payload.name, "svc");
        assert_eq!(payload.declaration_source.as_deref(), Some("launch spec"));
        assert!(payload.command.contains("sh"));
        assert_eq!(payload.exit, "code 7");
        assert!(
            payload.stderr_tail.contains("boom"),
            "stderr_tail={:?}",
            payload.stderr_tail
        );

        let display = err.to_string();
        assert!(display.contains("svc"));
        assert!(display.contains("from launch spec"));
        assert!(display.contains("expect initialize response"));
        assert!(display.contains("code 7"));
        assert!(display.contains("boom"));
    }

    #[test]
    fn format_exit_renders_codes_signals_and_unknown() {
        use std::os::unix::process::ExitStatusExt;
        use std::process::ExitStatus;

        assert_eq!(format_exit(Some(ExitStatus::from_raw(0))), "code 0");
        assert_eq!(format_exit(Some(ExitStatus::from_raw(2 << 8))), "code 2");
        // Raw status with no exit code but a signal in the low 7 bits.
        assert_eq!(format_exit(Some(ExitStatus::from_raw(9))), "signal 9");
        assert_eq!(format_exit(None), "still running (wait timed out)");
    }
}
