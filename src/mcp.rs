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

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use rmcp::model::{CallToolRequestParam, RawContent, ResourceContents};
use rmcp::service::{RoleClient, RunningService, serve_client};
use serde_json::Value;
use tokio::process::Child;

use crate::config::McpServerSpec;
use crate::container::Container;
use crate::error::{OutrigError, Result};

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
    stderr_path: PathBuf,
    service: RunningService<RoleClient, ()>,
    child: Child,
}

impl McpClient {
    /// Spawn `server_cfg`'s command via `podman exec -i` against `container`,
    /// redirect its stderr to `<log_dir>/<name>.stderr`, and drive the MCP
    /// `initialize` handshake. Returns the live client on success.
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
    ) -> Result<Self> {
        let (command, env) = server_cfg.normalize();

        tokio::fs::create_dir_all(log_dir).await?;
        let stderr_path = log_dir.join(format!("{name}.stderr"));
        let stderr_file = tokio::fs::File::create(&stderr_path).await?;
        let stderr_std = stderr_file.into_std().await;

        let mut cmd = container.build_exec_argv(&command, &env).to_tokio_command();
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
        let service = serve_client((), (stdout, stdin)).await?;

        Ok(Self {
            name: name.to_string(),
            stderr_path,
            service,
            child,
        })
    }

    /// The local name this client was constructed with (e.g. `"fs"`).
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Path to the file capturing this server's stderr.
    pub fn stderr_path(&self) -> &Path {
        &self.stderr_path
    }

    /// Issue an MCP `tools/list` (paginating internally) and project the
    /// results into our own [`McpTool`] type.
    pub async fn list_tools(&self) -> Result<Vec<McpTool>> {
        let tools = self.service.list_all_tools().await?;
        Ok(tools
            .into_iter()
            .map(|t| {
                let input_schema = t.schema_as_json_value();
                let description = if t.description.is_empty() {
                    None
                } else {
                    Some(t.description.into_owned())
                };
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

        let result = self
            .service
            .call_tool(CallToolRequestParam {
                name: name.to_string().into(),
                arguments,
            })
            .await?;

        let mut content_text = String::new();
        for (i, content) in result.content.iter().enumerate() {
            if i > 0 {
                content_text.push('\n');
            }
            match &content.raw {
                RawContent::Text(t) => content_text.push_str(&t.text),
                RawContent::Image(img) => {
                    let _ = write!(
                        content_text,
                        "[image: {}, {} base64 bytes]",
                        img.mime_type,
                        img.data.len()
                    );
                }
                RawContent::Resource(r) => match &r.resource {
                    ResourceContents::TextResourceContents { text, .. } => {
                        content_text.push_str(text)
                    }
                    ResourceContents::BlobResourceContents {
                        mime_type, blob, ..
                    } => {
                        let mime = mime_type.as_deref().unwrap_or("application/octet-stream");
                        let _ = write!(content_text, "[blob: {mime}, {} base64 bytes]", blob.len());
                    }
                },
            }
        }

        Ok(McpToolResult {
            content_text,
            is_error: result.is_error.unwrap_or(false),
        })
    }

    /// Cancel the rmcp service (which closes the child's stdin -- the MCP
    /// spec's normal shutdown signal), wait up to [`SHUTDOWN_GRACE`] for the
    /// server to exit on its own, then SIGKILL if it doesn't.
    pub async fn shutdown(self) -> Result<()> {
        let Self {
            service, mut child, ..
        } = self;

        // A `JoinError` here just means the rmcp task panicked, which doesn't
        // affect our ability to clean up the child below.
        let _ = service.cancel().await;

        match tokio::time::timeout(SHUTDOWN_GRACE, child.wait()).await {
            Ok(Ok(_)) => Ok(()),
            Ok(Err(e)) => Err(e.into()),
            Err(_) => {
                let _ = child.start_kill();
                let _ = child.wait().await;
                Ok(())
            }
        }
    }
}

fn kind_of(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}
