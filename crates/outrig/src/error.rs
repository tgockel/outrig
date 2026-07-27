//! Top-level error type.

use std::ffi::OsString;
use std::path::PathBuf;

use thiserror::Error;

use crate::config::{ApiKeyError, ConfigValidationError, EnvValueError};
use crate::container::embedded::EmbeddedImageConfigError;

#[derive(Debug, Error)]
pub enum OutrigError {
    #[error("configuration: {0}")]
    Configuration(String),

    #[error(
        "this outrig was built without the filesystem-view helper\n\
         help: install the musl target (`rustup target add x86_64-unknown-linux-musl`, or the \
         aarch64 equivalent) and rebuild"
    )]
    FilesystemHelperUnavailable,

    #[error(
        "no .agents/outrig/config.toml found in current directory or any parent\n\
         help: run `outrig init` to initialize"
    )]
    NoRepoConfig,

    #[error("{0}")]
    Io(#[from] std::io::Error),

    #[error("{0}")]
    Config(#[from] toml::de::Error),

    #[error(
        "{source}\n\
         help: key names containing `.` must be quoted, e.g. `[models.\"opus-4.7\"]` instead of `[models.opus-4.7]`"
    )]
    ConfigDottedKey {
        #[source]
        source: toml::de::Error,
    },

    #[error("{0}")]
    ApiKey(#[from] ApiKeyError),

    #[error("{0}")]
    ConfigValidation(#[from] ConfigValidationError),

    #[error("{}", format_process(program, argv, *exit_code, stderr_tail))]
    Process {
        program: &'static str,
        argv: Vec<OsString>,
        exit_code: Option<i32>,
        stderr_tail: String,
    },

    /// The command could never be started -- the binary is missing from `PATH`,
    /// is not executable, or the fork itself failed. Distinct from
    /// [`OutrigError::Process`], which means the command ran and exited badly.
    /// `command` is the [`crate::process::Cmd::render`] output, carried
    /// pre-rendered so this module stays independent of `process`.
    #[error("{}", format_spawn(program, command, source))]
    Spawn {
        program: &'static str,
        command: String,
        #[source]
        source: std::io::Error,
    },

    /// A filesystem operation that failed with the path it was operating on.
    /// Prefer this over bare [`OutrigError::Io`]: the naked `io::Error` renders
    /// as "No such file or directory (os error 2)" with nothing to act on.
    #[error("failed to {op} `{}`: {source}", path.display())]
    Path {
        op: &'static str,
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("could not allocate {kind} name during container bootstrap after retries")]
    BootstrapExhausted { kind: &'static str },

    /// The user bootstrap entered the container's namespaces and then failed
    /// inside them. Distinct from a failure on the way *in*, which is not an
    /// error at all: that falls back to the `podman exec` bootstrap, because
    /// nothing in the container has been touched yet.
    #[error("container {container}: bootstrapping the runtime user failed at {step}: {source}")]
    BootstrapNamespace {
        container: String,
        step: String,
        #[source]
        source: std::io::Error,
    },

    #[error("mcp service: {0}")]
    McpService(#[from] rmcp::service::ServiceError),

    #[error("mcp server initialize: {0}")]
    McpServerInitialize(#[source] Box<rmcp::service::ServerInitializeError>),

    #[error("mcp server {name:?} env key {key:?}: {source}")]
    McpEnvResolveFailed {
        name: String,
        key: String,
        #[source]
        source: EnvValueError,
    },

    #[error("image {image:?} build-arg {key:?}: {source}")]
    BuildArgResolveFailed {
        image: String,
        key: String,
        #[source]
        source: EnvValueError,
    },

    #[error("image {image:?} embedded config: {source}")]
    EmbeddedImageConfigParse {
        image: String,
        #[source]
        source: Box<EmbeddedImageConfigError>,
    },

    #[error("{0}")]
    McpStartupFailed(Box<McpStartupFailure>),

    #[error("mcp server {name:?} tools/list failed: {source}")]
    McpToolsListFailed {
        name: String,
        #[source]
        source: Box<rmcp::service::ServiceError>,
    },

    #[error("mcp call_tool: arguments must be a JSON object or null, got {kind}")]
    McpArgsNotObject { kind: &'static str },
}

impl From<tempfile::PersistError> for OutrigError {
    fn from(e: tempfile::PersistError) -> Self {
        OutrigError::Io(e.error)
    }
}

impl From<rmcp::service::ServerInitializeError> for OutrigError {
    fn from(e: rmcp::service::ServerInitializeError) -> Self {
        OutrigError::McpServerInitialize(Box::new(e))
    }
}

/// Boxed payload for [`OutrigError::McpStartupFailed`]. Carried behind a `Box`
/// so the variant doesn't bloat the size of `OutrigError` (which is what
/// `clippy::result_large_err` watches).
#[derive(Debug, Error)]
#[error(
    "mcp server {name:?}{declaration} failed to start: {source}\n  \
     exit: {exit}\n  \
     command: {command}\n  \
     stderr ({stderr_path}):\n{stderr_tail}",
    declaration = format_mcp_declaration_source(declaration_source),
    stderr_path = stderr_path.display(),
)]
pub struct McpStartupFailure {
    pub name: String,
    pub declaration_source: Option<String>,
    pub command: String,
    pub exit: String,
    pub stderr_path: PathBuf,
    pub stderr_tail: String,
    #[source]
    pub source: Box<dyn std::error::Error + Send + Sync>,
}

pub type Result<T> = std::result::Result<T, OutrigError>;

/// Attach the path an I/O operation was working on, turning a context-free
/// `io::Error` into [`OutrigError::Path`].
///
/// `op` is a verb phrase that reads into the message: `"read"`, `"create"`,
/// `"remove"` produce "failed to read `/etc/hosts`: ...".
pub trait IoPathExt<T> {
    fn path_ctx(self, op: &'static str, path: impl Into<PathBuf>) -> Result<T>;
}

impl<T> IoPathExt<T> for std::result::Result<T, std::io::Error> {
    fn path_ctx(self, op: &'static str, path: impl Into<PathBuf>) -> Result<T> {
        self.map_err(|source| OutrigError::Path {
            op,
            path: path.into(),
            source,
        })
    }
}

fn format_mcp_declaration_source(source: &Option<String>) -> String {
    source
        .as_ref()
        .map(|source| format!(" from {source}"))
        .unwrap_or_default()
}

fn format_spawn(program: &str, command: &str, source: &std::io::Error) -> String {
    let mut msg = format!("failed to run `{program}`: {source}\n  command: {command}");
    // A missing binary is by far the most common spawn failure and the one a
    // user can actually act on, so it earns a pointer at the prerequisites.
    // Other kinds (PermissionDenied, ENOEXEC) speak for themselves.
    if source.kind() == std::io::ErrorKind::NotFound {
        let base = crate::PUBLIC_DOC_BASE_URL;
        msg.push_str(&format!(
            "\n  help: `{program}` was not found on PATH -- \
             see {base}quickstart.html for prerequisites"
        ));
    }
    msg
}

fn format_process(
    program: &str,
    argv: &[OsString],
    exit_code: Option<i32>,
    stderr_tail: &str,
) -> String {
    let exit = match exit_code {
        Some(c) => format!("code {c}"),
        None => "signal".to_string(),
    };
    format!(
        "process `{program}` exited with {exit}\nargv: {argv:?}\n\
         --- stderr (tail) ---\n{stderr_tail}"
    )
}

#[cfg(test)]
mod tests {
    use std::io::ErrorKind;

    use super::*;

    fn spawn_err(kind: ErrorKind) -> OutrigError {
        OutrigError::Spawn {
            program: "buildah",
            command: "buildah images --quiet outrig-standard:ab12cd34".to_string(),
            source: std::io::Error::new(kind, "boom"),
        }
    }

    #[test]
    fn spawn_not_found_names_program_command_and_help() {
        let rendered = spawn_err(ErrorKind::NotFound).to_string();
        assert!(rendered.contains("failed to run `buildah`"), "{rendered}");
        assert!(
            rendered.contains("command: buildah images --quiet outrig-standard:ab12cd34"),
            "{rendered}"
        );
        assert!(
            rendered.contains("help: `buildah` was not found on PATH"),
            "{rendered}"
        );
        assert!(
            rendered.contains("https://tgockel.github.io/outrig/quickstart.html"),
            "{rendered}"
        );
    }

    #[test]
    fn spawn_other_kinds_omit_the_help_line() {
        let rendered = spawn_err(ErrorKind::PermissionDenied).to_string();
        assert!(rendered.contains("failed to run `buildah`"), "{rendered}");
        assert!(!rendered.contains("help:"), "{rendered}");
    }

    #[test]
    fn path_error_names_the_path_and_operation() {
        let err = OutrigError::Path {
            op: "read",
            path: PathBuf::from(".agents/outrig/config.toml"),
            source: std::io::Error::from(ErrorKind::NotFound),
        };
        let rendered = err.to_string();
        assert!(
            rendered.starts_with("failed to read `.agents/outrig/config.toml`: "),
            "{rendered}"
        );
    }

    #[test]
    fn path_ctx_attaches_the_path_to_a_bare_io_error() {
        let result: std::result::Result<(), std::io::Error> =
            Err(std::io::Error::from(ErrorKind::NotFound));
        let err = result.path_ctx("open", "/tmp/nope").unwrap_err();
        assert!(err.to_string().contains("`/tmp/nope`"), "{err}");
    }
}
