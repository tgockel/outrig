//! Top-level error type.

use std::ffi::OsString;
use std::path::PathBuf;
use std::time::Duration;

use thiserror::Error;

use crate::config::{ApiKeyError, ConfigValidationError, EnvValueError};
use crate::container::embedded::EmbeddedImageConfigError;

/// Why `build.rs` could not embed the `outrig-enter` helper. It sets
/// `OUTRIG_ENTER_UNAVAILABLE_REASON` only when it degrades, so `None` is the
/// build that succeeded -- in which case this error is unreachable and the
/// fallback is what a mismatched build would be told.
fn filesystem_helper_unavailable_reason() -> &'static str {
    option_env!("OUTRIG_ENTER_UNAVAILABLE_REASON").unwrap_or(
        "install the matching `x86_64-unknown-linux-musl` or \
         `aarch64-unknown-linux-musl` target and rebuild",
    )
}

#[derive(Debug, Error)]
#[non_exhaustive]
pub enum OutrigError {
    #[error("configuration: {0}")]
    Configuration(String),

    #[error(
        "this outrig was built without the filesystem-view helper\n\
         build detail: {}",
        filesystem_helper_unavailable_reason()
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
    #[non_exhaustive]
    ConfigDottedKey {
        #[source]
        source: toml::de::Error,
    },

    #[error("{0}")]
    ApiKey(#[from] ApiKeyError),

    #[error("{0}")]
    ConfigValidation(#[from] ConfigValidationError),

    #[error("{}", format_process(program, argv, *exit_code, stderr_tail))]
    #[non_exhaustive]
    Process {
        program: &'static str,
        argv: Vec<OsString>,
        exit_code: Option<i32>,
        stderr_tail: String,
    },

    /// The caller's stop signal fired before the command finished. The child
    /// was killed **and reaped** before this error was produced, so receiving
    /// it means the process is gone -- not that a kill is in flight.
    #[error("`{program}` was stopped before it finished\nargv: {argv:?}")]
    #[non_exhaustive]
    Canceled {
        program: &'static str,
        argv: Vec<OsString>,
    },

    /// The command could never be started -- the binary is missing from `PATH`,
    /// is not executable, or the fork itself failed. Distinct from
    /// [`OutrigError::Process`], which means the command ran and exited badly.
    /// `command` is the [`crate::process::Cmd::render`] output, carried
    /// pre-rendered so this module stays independent of `process`.
    #[error("{}", format_spawn(program, command, source))]
    #[non_exhaustive]
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
    #[non_exhaustive]
    Path {
        op: &'static str,
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("could not allocate {kind} name during container bootstrap after retries")]
    #[non_exhaustive]
    BootstrapExhausted { kind: &'static str },

    /// The user bootstrap failed somewhere in the chain into the container's
    /// namespaces, or inside them. `step` names how far it got, which is the
    /// whole diagnostic: an early step means the container was never entered,
    /// a late one means it was.
    #[error("container {container}: bootstrapping the runtime user failed at {step}: {source}")]
    #[non_exhaustive]
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
    #[non_exhaustive]
    McpEnvResolveFailed {
        name: String,
        key: String,
        #[source]
        source: EnvValueError,
    },

    #[error("image {image:?} build-arg {key:?}: {source}")]
    #[non_exhaustive]
    BuildArgResolveFailed {
        image: String,
        key: String,
        #[source]
        source: EnvValueError,
    },

    #[error("image {image:?} embedded config: {source}")]
    #[non_exhaustive]
    EmbeddedImageConfigParse {
        image: String,
        #[source]
        source: Box<EmbeddedImageConfigError>,
    },

    // Transparent for the same reason as the network pair below: its payload
    // carries the failure that actually stopped the server, and that is the
    // link a consumer wants. Left inconsistent with them for a while, which
    // is the only reason it is mentioned here.
    #[error(transparent)]
    McpStartupFailed(Box<McpStartupFailure>),

    #[error("mcp server {name:?} tools/list failed: {source}")]
    #[non_exhaustive]
    McpToolsListFailed {
        name: String,
        #[source]
        source: Box<rmcp::service::ServiceError>,
    },

    #[error("mcp call_tool: arguments must be a JSON object or null, got {kind}")]
    #[non_exhaustive]
    McpArgsNotObject { kind: &'static str },

    /// Obligations a network detach or shutdown could not discharge. Every
    /// obligation it owes is attempted; this carries the ones that failed, so
    /// a container left holding the interceptor's resolver -- or a stray nft
    /// table in its namespace -- reaches the caller rather than a log line.
    // Transparent, and so `source()` is `None` -- deliberately. An aggregate
    // has no single lower-level cause to point at, and its `Display` already
    // renders every one of them with the container it belongs to, so a chain
    // walker that stops here has still printed the whole story. Naming the
    // boxed field as the source instead would make the same text appear twice
    // in every rendered chain, and hand a downcaster a `Box<_>` to guess at;
    // the structured route is matching this variant, which is why it is public
    // and why the payload's fields are.
    #[error(transparent)]
    NetworkTeardown(Box<NetworkTeardownFailure>),

    #[error(transparent)]
    NetworkAttachNotUndone(Box<NetworkAttachFailure>),

    #[error(transparent)]
    SidecarNotUnwound(Box<SidecarUnwindFailure>),

    #[error(
        "{records} audit record(s) for container {container:?} could not be written; \
         the first failed with: {source}{}",
        match integrity {
            Some(why) => format!(
                "\n  and the log may hold a partial record that could not be removed: {why}"
            ),
            None => String::new(),
        }
    )]
    #[non_exhaustive]
    NetworkAuditUnwritten {
        container: String,
        /// Set when the log may hold a partial record: what stopped the writer
        /// proving it had been removed. Kept alongside the failure that broke
        /// the append rather than in place of it, because they are different
        /// facts and only together do they say what state the file is in.
        integrity: Option<Box<OutrigError>>,
        /// How many records were lost, saturating. A container that can open
        /// connections can make this fail as often as it likes, so what is
        /// kept is one representative error and a count rather than one entry
        /// apiece.
        records: u64,
        #[source]
        source: Box<OutrigError>,
    },

    /// The interceptor's tasks for one container did not stop within the
    /// grace its detach gave them and were aborted. The connections they held
    /// are gone either way; what this reports is that they did not go on
    /// their own, which is the symptom of one wedged somewhere it does not
    /// watch its cancellation token.
    #[error("network tasks did not stop within {grace:?} and were aborted")]
    #[non_exhaustive]
    NetworkTasksAborted { grace: Duration },

    /// The connections an attachment had accepted were still running after
    /// its tasks had been aborted.
    ///
    /// A different and worse thing than [`Self::NetworkTasksAborted`], which
    /// is why it is a variant of its own: that one says the accept and DNS
    /// loops had to be stopped, and this one says a *bridge* outlived them --
    /// a connection possibly still moving bytes for a container the caller has
    /// been told is detached, and possibly still owing an audit record. Both
    /// windows can expire in one teardown, and reporting them with the same
    /// variant left the more serious of the two unreadable.
    #[error("connections were still running {grace:?} after this attachment's tasks were stopped")]
    #[non_exhaustive]
    NetworkConnectionsUnfinished { grace: Duration },

    /// A network interceptor task ended in a panic.
    #[error("a network interceptor task panicked: {source}")]
    #[non_exhaustive]
    NetworkTaskPanicked {
        #[source]
        source: tokio::task::JoinError,
    },
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
#[non_exhaustive]
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

/// One obligation a network teardown owed and did not discharge, named to the
/// container it was owed to. The obligation itself is whatever failed --
/// [`OutrigError::Process`] names the exact argv of a resolver restore or an
/// nft delete that exited non-zero -- so a caller can match on the cause
/// rather than read it.
#[derive(Debug, Error)]
#[error("container {container:?}: {source}")]
#[non_exhaustive]
pub struct NetworkTeardownCause {
    pub container: String,
    #[source]
    pub source: Box<OutrigError>,
}

/// Boxed payload for [`OutrigError::NetworkTeardown`], behind a `Box` for the
/// same reason as [`McpStartupFailure`]: the variant must not bloat
/// `OutrigError`.
///
/// A teardown owes obligations of unlike kinds -- a resolver restore, an nft
/// delete, the tasks holding a container's connections -- and attempts every
/// one of them whatever the others do, so there is rarely exactly one thing
/// to report. `shutdown` pools every attachment's into one of these.
#[derive(Debug, Error)]
#[error(
    "network teardown left {} obligation(s) undischarged:\n  {}",
    causes.len(),
    render_teardown_causes(causes)
)]
#[non_exhaustive]
pub struct NetworkTeardownFailure {
    pub causes: Vec<NetworkTeardownCause>,
}

/// What is left of `failed` once the container it names has been confirmed
/// gone, with whatever it said was left behind logged under `what`.
///
/// A `NetworkAttachNotUndone` says an attach could not be fully undone and the
/// container may still be carrying interception nothing owns. Stopping that
/// container takes its namespaces and everything in them, so the claim stops
/// being true the moment the stop is confirmed -- and a caller handed it
/// anyway reads "could not be fully undone, N obligation(s) left" about
/// something that no longer exists. What started the attach failure is what
/// remains worth telling them.
pub fn superseded_by_a_confirmed_stop(failed: OutrigError, what: &str) -> OutrigError {
    let OutrigError::NetworkAttachNotUndone(attach) = failed else {
        return failed;
    };
    let NetworkAttachFailure {
        container,
        source,
        residue,
        ..
    } = *attach;
    for obligation in residue {
        tracing::warn!(
            target: "outrig::error",
            container,
            "attaching {container:?} for {what} left this undone ({obligation}); \
             stopping the container afterwards worked, so nothing is left behind"
        );
    }
    *source
}

/// Boxed payload for [`OutrigError::NetworkAttachNotUndone`].
///
/// An attach that fails and is fully undone reports the failure that stopped
/// it and nothing else: the container is as it was, and the call can simply be
/// tried again. This is the other case -- the attach failed *and* putting the
/// container back did not fully succeed -- and it is a different thing to be
/// told, because the container may still be carrying interception that no
/// attachment owns. `residue` is what could not be undone; each entry is still
/// armed for the destructor to reissue, so this is a report rather than the
/// last word.
#[derive(Debug, Error)]
#[error(
    "attaching container {container:?} failed and could not be fully undone: {source}\n  \
     {} obligation(s) left:\n  {}",
    residue.len(),
    render_errors(residue)
)]
#[non_exhaustive]
pub struct NetworkAttachFailure {
    pub container: String,
    /// The failure that stopped the attach.
    #[source]
    pub source: Box<OutrigError>,
    /// What undoing it could not put back.
    pub residue: Vec<OutrigError>,
}

/// Boxed payload for [`OutrigError::SidecarNotUnwound`].
///
/// A sidecar that fails to come up is torn down again: detached from the
/// interceptor, then stopped. When that teardown also fails the caller is
/// owed both halves -- the reason the sidecar was abandoned, and the fact that
/// a live container is still carrying whatever was done to it, with no
/// attachment left to try again through.
#[derive(Debug, Error)]
#[error(
    "sidecar {sidecar:?} failed to start and could not be fully unwound: {source}\n  \
     {} obligation(s) left:\n  {}",
    residue.len(),
    render_errors(residue)
)]
#[non_exhaustive]
pub struct SidecarUnwindFailure {
    pub sidecar: String,
    /// Why the sidecar was being torn down.
    #[source]
    pub source: Box<OutrigError>,
    /// What tearing it down could not finish.
    pub residue: Vec<OutrigError>,
}

impl SidecarUnwindFailure {
    /// Report that `sidecar` could not be fully unwound. The type is
    /// `#[non_exhaustive]`, so this is how anything outside the crate -- the
    /// CLI, which starts sidecars of its own -- builds one.
    pub fn new(sidecar: impl Into<String>, source: OutrigError, residue: Vec<OutrigError>) -> Self {
        Self {
            sidecar: sidecar.into(),
            source: Box::new(source),
            residue,
        }
    }
}

fn render_errors(errors: &[OutrigError]) -> String {
    errors
        .iter()
        .map(|e| e.to_string())
        .collect::<Vec<_>>()
        .join("\n  ")
}

fn render_teardown_causes(causes: &[NetworkTeardownCause]) -> String {
    causes
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n  ")
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

    /// A consumer walking `source()` reaches what actually went wrong. These
    /// carry their payload in a box, and a box named only by `{0}` in the
    /// format string is a `Display` detail -- `source()` stopped there, so the
    /// cause the payload carries was reachable only by matching the variant.
    #[test]
    fn a_boxed_failure_does_not_end_the_source_chain() {
        use std::error::Error;

        let cause = || OutrigError::Configuration("the table would not go".to_string());
        let unwound = OutrigError::SidecarNotUnwound(Box::new(SidecarUnwindFailure::new(
            "tools",
            cause(),
            vec![cause()],
        )));
        let source = unwound
            .source()
            .unwrap_or_else(|| panic!("a boxed failure has a cause: {unwound}"));
        assert!(
            source.to_string().contains("the table would not go"),
            "and it is the failure that started the unwind: {source}"
        );

        let attach = OutrigError::NetworkAttachNotUndone(Box::new(NetworkAttachFailure {
            container: "outrig-a".to_string(),
            source: Box::new(cause()),
            residue: vec![cause()],
        }));
        assert!(
            attach
                .source()
                .is_some_and(|e| e.to_string().contains("would not go")),
            "{attach}"
        );

        // The aggregate is the other shape. It has no single lower-level
        // cause, so there is nothing for `source()` to point at -- what makes
        // that acceptable is that its own rendering carries every cause, so a
        // chain that stops here has still said all of it.
        let teardown = OutrigError::NetworkTeardown(Box::new(NetworkTeardownFailure {
            causes: vec![
                NetworkTeardownCause {
                    container: "outrig-a".to_string(),
                    source: Box::new(cause()),
                },
                NetworkTeardownCause {
                    container: "outrig-b".to_string(),
                    source: Box::new(OutrigError::Configuration(
                        "nor would the resolver".to_string(),
                    )),
                },
            ],
        }));
        let rendered = teardown.to_string();
        for named in [
            "outrig-a",
            "the table would not go",
            "outrig-b",
            "nor would the resolver",
        ] {
            assert!(
                rendered.contains(named),
                "every cause has to be in the rendering, missing {named:?}: {rendered}"
            );
        }
    }

    fn spawn_err(kind: ErrorKind) -> OutrigError {
        OutrigError::Spawn {
            program: "buildah",
            command: "buildah images --quiet outrig-standard:ab12cd34".to_string(),
            source: std::io::Error::new(kind, "boom"),
        }
    }

    /// Which of the two arms runs depends on how *this* build went, so the
    /// assertions have to differ: asserting the union of both would pass on
    /// the fallback alone and never notice `build.rs` going quiet.
    #[test]
    fn missing_filesystem_helper_error_carries_the_build_reason() {
        let rendered = OutrigError::FilesystemHelperUnavailable.to_string();
        assert!(rendered.contains("filesystem-view helper"), "{rendered}");
        if crate::container::enter::is_available() {
            // Nothing degraded, so there is no build-time reason to report and
            // the message falls back to naming both musl targets.
            assert!(rendered.contains("x86_64-unknown-linux-musl"), "{rendered}");
            assert!(
                rendered.contains("aarch64-unknown-linux-musl"),
                "{rendered}"
            );
        } else {
            // `build.rs` degraded, so it emitted a reason -- which must have
            // displaced the fallback rather than sitting unread beside it.
            assert!(
                !rendered.contains("install the matching"),
                "expected build.rs's reason, got the fallback: {rendered}"
            );
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
