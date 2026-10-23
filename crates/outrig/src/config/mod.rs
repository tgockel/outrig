//! Config schema, parsing, merge, and validation.

mod api_key;
mod binding;
mod env_ref;
mod env_value;
mod lenient;
mod merge;
mod validate;

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::net::IpAddr;
use std::path::{Path, PathBuf};

use nix::unistd::{Uid, User};
use schemars::JsonSchema;
use serde::de::value::{MapAccessDeserializer, SeqAccessDeserializer};
use serde::de::{MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};

pub use api_key::{ApiKeyError, ApiKeyRef, EnvSecrets, Secrets};
pub use binding::{Binding, FactoryValue};
pub use env_value::{EnvValue, EnvValueError, ResolvedEnvValue};
pub use lenient::{ConfigWarning, ConfigWarningKind};
pub use merge::merge;
pub use validate::{
    BuildImageNameError, ConfigValidationError, MountRuleViolation, check_build_image_name,
};
pub(crate) use validate::{
    check_entrypoint_hosting, check_mcp_call_timeout_secs, check_sidecar_image, check_sidecar_name,
    check_view_exclusions, is_valid_mcp_server_name, mcp_call_timeout_secs_in_range,
    mcp_command_is_empty, validate_binding, validate_session_paths,
};

use crate::error::{IoPathExt, OutrigError, Result};

pub const DEFAULT_TOOL_CALL_MAX: u32 = 50;
pub const TOOL_CALL_MAX_LIMIT: u32 = 2000;
pub const DEFAULT_TOOL_RESULT_MAX_BYTES: u32 = 256 * 1024;
pub const TOOL_RESULT_MAX_FLOOR_BYTES: u32 = 1024;
pub const TOOL_RESULT_MAX_CEILING_BYTES: u32 = 16 * 1024 * 1024;

/// How deeply subagents may nest by default. The primary agent is the root at
/// depth 1; an agent at depth `D` may launch subagents while `D < max`. A value
/// of 1 disables subagents entirely, 2 is a single layer, 3 is two layers.
pub const DEFAULT_SUBAGENT_DEPTH_MAX: u32 = 3;
/// Upper bound accepted for `subagent-depth-max`, to keep a runaway config from
/// authorizing an unbounded launch tree.
pub const SUBAGENT_DEPTH_MAX_CEILING: u32 = 16;
/// How many live subagents one launching agent may hold by default.
pub const DEFAULT_SUBAGENT_WIDTH_MAX: u32 = 8;
/// Upper bound accepted for `subagent-width-max`, keeping fan-out finite.
pub const SUBAGENT_WIDTH_MAX_CEILING: u32 = 16;

/// How many children a `run-new` session holds at once by default, idle and
/// wedged ones included: each is a thread and its memory in the interpreter.
pub(crate) const DEFAULT_CHILDREN_MAX: u32 = 64;
/// Upper bound accepted for `children-max`: past about a thousand kernels of
/// some 16 MiB each, the interpreter's memory ceiling is the real limit.
pub(crate) const CHILDREN_MAX_CEILING: u32 = 1024;
/// How many model requests a session has in flight at once by default, every
/// agent's together.
pub(crate) const DEFAULT_MODEL_CONCURRENCY_MAX: u32 = 8;
/// Upper bound accepted for `model-concurrency-max`, past what one provider
/// key is likely to allow.
pub(crate) const MODEL_CONCURRENCY_MAX_CEILING: u32 = 128;
/// How many requests one child holds unsettled by default.
pub(crate) const DEFAULT_REQUESTS_MAX: u32 = 256;
/// Upper bound accepted for `requests-max`: each unsettled request holds a
/// body, and a child answering one per round would spend that many rounds.
pub(crate) const REQUESTS_MAX_CEILING: u32 = 4096;

/// The limits a `run-new` session's agents run under, as the session's agent
/// and the top level set them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Limits {
    /// The deepest an agent may be and still launch a child; the primary is
    /// at depth 1.
    pub(crate) depth_max: u32,
    pub(crate) children_max: u32,
    pub(crate) model_concurrency_max: u32,
    pub(crate) requests_max: u32,
    /// Tokens the session's model calls may spend in all; `None` caps nothing.
    pub(crate) token_budget: Option<u64>,
}

/// Scratch state for [`Config::model_candidates`]' depth-first walk.
///
/// Four collections rather than one, because they answer four different
/// questions and two of them are load-bearing for complexity:
///
/// - `flattened` is the ordered result.
/// - `emitted` is its membership index. `flattened.contains` would be a linear
///   scan per leaf, so `N` leaves cost `N^2` comparisons -- and validation
///   walks once per root, making an ordinary "many names, one shared alias"
///   config cubic.
/// - `expanded` maps an alias node already fully walked to its subtree height.
///   Without it a diamond re-enters a shared node once per path reaching it, so
///   `aN = [aN-1, aN-1]` costs `2^N` visits -- around 134 million for 26 rows,
///   on a valid config walked at every load. Skipping one changes no output:
///   re-expanding could only re-emit names `emitted` already holds. The height
///   is kept so the depth bound survives the skip -- see `walk_model_alias`.
/// - `on_path` is the current path, for cycle detection and the depth bound.
///   Distinct from `expanded`: `alias = ["b", "b"]` is a repeat, while
///   `a -> b -> a` is a cycle, and only the path distinguishes them.
#[derive(Default)]
struct AliasWalk<'a> {
    flattened: Vec<&'a str>,
    emitted: BTreeSet<&'a str>,
    expanded: BTreeMap<&'a str, usize>,
    on_path: Vec<&'a str>,
}

/// How many `[models.<name>].alias` hops one name may take before
/// [`Config::model_candidates`] gives up.
///
/// Bounds the traversal's recursion against the config rather than against the
/// stack: without it, depth is limited only by the number of model entries, and
/// a long enough chain aborts the process instead of reporting a bad config.
/// Well above anything an alias graph plausibly needs -- a chain this deep is a
/// mistake, and saying so beats crashing.
pub const MODEL_ALIAS_DEPTH_MAX: usize = 32;

/// How long one LLM HTTP call may keep retrying transient failures before the
/// turn gives up. Ten minutes: long enough to ride out a provider-side
/// rate-limit window -- a shared endpoint's `Retry-After` runs to minutes --
/// and short enough that a genuinely dead endpoint is reported inside the
/// session that hit it. A value of `0` disables retries: the first failure is
/// final.
///
/// The budget is wall clock from the first attempt, so it includes time spent
/// in flight and not only time spent sleeping. `doc/reference/config.md` has
/// what that means when it and `request-timeout-secs` are set close together;
/// `doc/concepts/llm-providers.md` has which failures are retried at all.
pub const DEFAULT_RETRY_BUDGET_SECS: u64 = 600;
/// Upper bound accepted for `retry-budget-secs`, so a fat-fingered value cannot
/// wedge an interactive turn for hours.
pub const RETRY_BUDGET_SECS_CEILING: u64 = 3600;

/// Upper bound accepted for `request-timeout-secs`. The same hour as
/// [`RETRY_BUDGET_SECS_CEILING`], and for the same reason: an endpoint that has
/// not answered inside a minute is not going to, so an hour is already the
/// degenerate case rather than a tuning range. Inclusive -- exactly `3600` is
/// accepted.
///
/// Unlike the budget this key also has a floor, enforced as a rejection of `0`
/// rather than as a constant: `Duration::ZERO` is an *immediate* timeout in
/// reqwest, not a disabled one, so a `0` here fails every request before it can
/// be answered. A `0` on the budget means "do not retry" and stays legal; the
/// keys differ because one counts attempts and the other bounds a single one.
pub const REQUEST_TIMEOUT_SECS_CEILING: u64 = 3600;

/// How long a program `outrig run <script.py>` runs on the host has to end on its own after the
/// first `SIGINT` or `SIGTERM` forwarded to it, before the engine stops its process group. Five
/// seconds: long enough for a program that handles the signal to leave its sessions, short
/// enough that one that ignores it is not waited on for long. The engine's own shutdown of a
/// session the program asked it to shut down does not count against it.
pub const DEFAULT_HOST_GRACE_SECS: u64 = 5;
/// Upper bound accepted for `[host] grace-secs`: the same hour as
/// [`RETRY_BUDGET_SECS_CEILING`], so a mistyped value cannot leave a program that ignores signals
/// running for days.
pub const HOST_GRACE_SECS_CEILING: u64 = 3600;

/// How long one MCP `tools/call` may run before outrig gives up on it, sends
/// the server `notifications/cancelled`, and hands the model a timeout in
/// place of a result. Ten minutes, like [`DEFAULT_RETRY_BUDGET_SECS`]: long
/// enough for a build or a test run, short enough that a server which will
/// never answer gives the turn back inside the session. A server's own
/// `call-timeout-secs` and the top-level `mcp-call-timeout-secs` override it,
/// in that order.
///
/// `initialize` and `tools/list` are not covered: they are bounded by a fixed
/// startup deadline, because nothing a server does at startup should take as
/// long as a tool may.
pub const DEFAULT_MCP_CALL_TIMEOUT_SECS: u64 = 600;
/// Upper bound accepted for `call-timeout-secs` and `mcp-call-timeout-secs`.
/// The same hour, floor, and reasoning as [`REQUEST_TIMEOUT_SECS_CEILING`]: a
/// `0` is an immediate timeout rather than none, so it is rejected.
pub const MCP_CALL_TIMEOUT_SECS_CEILING: u64 = 3600;

/// The `tools/call` deadline for one server: its own `call-timeout-secs`, else
/// the session's `mcp-call-timeout-secs`, else
/// [`DEFAULT_MCP_CALL_TIMEOUT_SECS`]. Most specific wins, and the values are
/// read after every merge, so a label entry a config entry replaced brings
/// nothing with it.
pub(crate) fn effective_mcp_call_timeout(
    per_server: Option<u64>,
    session_default: Option<u64>,
) -> std::time::Duration {
    std::time::Duration::from_secs(
        per_server
            .or(session_default)
            .unwrap_or(DEFAULT_MCP_CALL_TIMEOUT_SECS),
    )
}

/// Which config file an entry was declared in. Recorded per entry at load time
/// -- before [`merge`], which is where origin would otherwise be lost -- so a
/// relative path can resolve against the directory that gives it meaning
/// rather than against whichever repo happens to be current.
///
/// The two accessors are not the same value and are not derivable from one
/// another by a single `parent()`: the repo config sits three levels below the
/// root its paths resolve against.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ConfigSource {
    /// The repo config, `<root>/.agents/outrig/config.toml`.
    Repo { root: PathBuf },
    /// The global config, as resolved from `--global-config`,
    /// `$XDG_CONFIG_HOME`, or `~/.outrig/`.
    Global { path: PathBuf },
    /// A repo config read from a file named outright rather than found under
    /// a repo root, as [`Config::load_file`] reads one. Nothing ties the file
    /// to a root, so its relative paths resolve beside it, as the global
    /// config's do.
    RepoFile { path: PathBuf },
    /// A standalone image project -- the directory holding an `image.toml`.
    Project { dir: PathBuf },
}

impl ConfigSource {
    /// Directory that this file's relative paths resolve against.
    pub fn base_dir(&self) -> &Path {
        match self {
            Self::Repo { root } => root,
            Self::Global { path } | Self::RepoFile { path } => {
                path.parent().unwrap_or(Path::new(""))
            }
            Self::Project { dir } => dir,
        }
    }

    /// The file that declared the entry, for diagnostics. A path reported
    /// without this reads as a repo problem even when it came from elsewhere.
    pub fn config_path(&self) -> PathBuf {
        match self {
            Self::Repo { root } => crate::repo::repo_config_path(root),
            Self::Global { path } | Self::RepoFile { path } => path.clone(),
            // The literal rather than a shared constant: the two sites that
            // actually read this file live in `outrig-cli`, which cannot see a
            // `pub(crate)` constant here, so a constant would centralize
            // nothing while looking like it did.
            Self::Project { dir } => dir.join("image.toml"),
        }
    }
}

/// Base directory a config-declared relative path resolves against: the
/// directory of the file that declared it, or `repo_root` for an entry that
/// never went through [`Config::load`]. The other half of [`resolve_against`]'s
/// rule, stated once so the fallback cannot drift between the types that carry
/// a [`ConfigSource`].
fn source_base_dir<'a>(source: Option<&'a ConfigSource>, repo_root: &'a Path) -> &'a Path {
    source.map_or(repo_root, ConfigSource::base_dir)
}

/// `path` resolved as [`resolve_against`] and [`source_base_dir`] would, with
/// `.` and `..` resolved, where that can be known: always with a repository
/// root, and without one only for a path from a config file, an absolute
/// path, or one under `~`. Validation without a root uses this to check what
/// it can and skip the rest.
fn known_path(
    source: Option<&ConfigSource>,
    path: &Path,
    repo_root: Option<&Path>,
) -> Option<PathBuf> {
    let resolved = match (source, repo_root) {
        (Some(source), _) => resolve_against(source.base_dir(), path),
        (None, Some(root)) => resolve_against(root, path),
        (None, None) => resolve_against(Path::new(""), path),
    };
    resolved.is_absolute().then(|| lexical(&resolved))
}

/// A config file named by path, made absolute, and the outcome of reading it.
/// Resolve before reading, and read through the resolved path: a
/// `--global-config` or `--config` path may be relative, and would otherwise
/// consult the working directory twice -- once to find the file, once to
/// record where its relative paths point -- so a directory change in between
/// could load one file and stamp another's origin. Every path inherited from
/// the file rides on that origin, the read-write primary bind mount included.
/// Lexical: no I/O, no symlink resolution.
///
/// Failing to resolve means the working directory is unreadable or the path
/// is empty, in which case no relative path in the file can be given a
/// meaning; that is an error rather than grounds to keep the relative origin.
/// What a failed read means is the caller's call.
fn read_resolved(path: &Path) -> Result<(PathBuf, std::io::Result<String>)> {
    let path = std::path::absolute(path).path_ctx("resolve", path)?;
    let text = fs::read_to_string(&path);
    Ok((path, text))
}

/// Resolve `path` against `base`: a leading `~` is the invoking user's home
/// directory, an absolute path is used as-is, and anything else joins `base`.
/// The one rule, shared by every config-declared host path.
pub(crate) fn resolve_against(base: &Path, path: &Path) -> PathBuf {
    let path = expand_tilde(path, home_dir().as_deref());
    if path.is_absolute() {
        path
    } else {
        base.join(path)
    }
}

/// `path` with `.` and `..` resolved textually, as a container runtime resolves
/// a mount's destination: the host filesystem has nothing to say about a path
/// in the container, and `..` at the root stays at the root. What two config
/// paths are compared by, since [`resolve_against`] keeps `..` -- a host path
/// it gives is what podman mounts, symlinks and all.
pub(crate) fn lexical(path: &Path) -> PathBuf {
    let mut resolved = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::ParentDir => {
                resolved.pop();
            }
            std::path::Component::CurDir => {}
            other => resolved.push(other),
        }
    }
    resolved
}

/// The invoking user's home directory: `HOME`, or the passwd entry's when
/// `HOME` is unset or empty. Not `std::env::home_dir`, which takes an empty
/// `HOME` as the answer before Rust 1.90, and the MSRV is 1.88.
fn home_dir() -> Option<PathBuf> {
    match std::env::var_os("HOME") {
        Some(home) if !home.is_empty() => Some(home.into()),
        _ => User::from_uid(Uid::current())
            .ok()
            .flatten()
            .map(|user| user.dir),
    }
}

/// `path` with a leading `~` component replaced by `home`, and anything else
/// as written. Only a whole `~` component counts, so `~user/...` stays a
/// relative path and `./~` names a directory that is really called `~`. With
/// no `home`, or one that is not absolute, `~` is an ordinary directory name,
/// and [`resolve_against`] joins it onto the base like any other.
fn expand_tilde(path: &Path, home: Option<&Path>) -> PathBuf {
    let home = home.filter(|home| home.is_absolute());
    match (path.strip_prefix("~"), home) {
        // `home.join("")` would add a trailing separator to a bare `~`.
        (Ok(rest), Some(home)) if rest.as_os_str().is_empty() => home.to_path_buf(),
        (Ok(rest), Some(home)) => home.join(rest),
        _ => path.to_path_buf(),
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
#[non_exhaustive]
pub struct Config {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_image: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_agent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_model: Option<String>,
    /// Global-only: a repo config that sets it fails
    /// [`validate_as_repo`](Self::validate_as_repo), and [`merge`] never reads
    /// it from the repo side. A config read from a file holds it resolved
    /// against that file's directory, so after any [`load`](Self::load) it is
    /// absolute.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_root: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_max: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_result_max: Option<u32>,
    /// Maximum subagent nesting depth. See [`DEFAULT_SUBAGENT_DEPTH_MAX`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subagent_depth_max: Option<u32>,
    /// Maximum number of live subagents per launching agent. See
    /// [`DEFAULT_SUBAGENT_WIDTH_MAX`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subagent_width_max: Option<u32>,
    /// How many children a `run-new` session may hold at once -- launching,
    /// working, idle, or wedged -- 64 when unset. A launch past it fails at
    /// once.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub children_max: Option<u32>,
    /// How many model requests a session may have in flight at once, every
    /// agent's together, 8 when unset. A request past it waits its turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_concurrency_max: Option<u32>,
    /// How many requests one child may have unsettled, 256 when unset. A
    /// request or submission past it fails at once.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requests_max: Option<u32>,
    /// How many tokens a `run-new` session's model calls may spend in all,
    /// every agent's together. Unset, nothing caps them; once spent, no agent
    /// makes another model call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_budget: Option<u64>,
    /// Default transient-retry budget for every remote provider, in seconds.
    /// A `[providers.<name>]` row of its own overrides this. See
    /// [`DEFAULT_RETRY_BUDGET_SECS`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_budget_secs: Option<u64>,
    /// Default `tools/call` deadline for every MCP server, in seconds. A
    /// server's own `call-timeout-secs` overrides this. See
    /// [`DEFAULT_MCP_CALL_TIMEOUT_SECS`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mcp_call_timeout_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "NetworkConfig::is_default")]
    pub network: NetworkConfig,
    #[serde(default, skip_serializing_if = "EventsConfig::is_default")]
    pub events: EventsConfig,
    /// How a program `outrig run <script.py>` runs on the host is treated. See [`HostConfig`].
    #[serde(default, skip_serializing_if = "HostConfig::is_default")]
    pub host: HostConfig,
    /// Global-only, like [`session_root`](Self::session_root): which host
    /// variables a repo config file may reference. A repo config that declares
    /// `[repo].env-allow` fails [`validate_as_repo`](Self::validate_as_repo),
    /// and [`merge`] never reads it from the repo side.
    #[serde(default, skip_serializing_if = "RepoPolicy::is_default")]
    pub repo: RepoPolicy,

    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub providers: BTreeMap<String, LlmProvider>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub models: BTreeMap<String, Model>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub agents: BTreeMap<String, Agent>,

    #[serde(default, skip_serializing_if = "Workspace::is_default")]
    pub workspace: Workspace,

    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub images: BTreeMap<String, ImageConfig>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub sidecars: BTreeMap<String, SidecarConfig>,

    /// Objects a session hosts for its agent, each in a process of its own on the host. See
    /// [`Binding`].
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub bindings: BTreeMap<String, Binding>,

    /// What the load set aside instead of failing on. See
    /// [`warnings`](Self::warnings).
    #[serde(skip)]
    warnings: Vec<ConfigWarning>,
}

impl Config {
    /// Parse `s` as a config file. A key no table has is dropped and recorded
    /// in [`warnings`](Self::warnings); any other problem fails the parse.
    /// `toml::from_str::<Config>` stays strict and fails on the key as well.
    pub fn load_from_str(s: &str) -> Result<Self> {
        lenient::parse(s, None, false)
    }

    /// What the load that produced this config set aside instead of failing
    /// on, in file order: each key no table has, and, from the global config
    /// alone, each value that did not deserialize. `[network]` and the
    /// `security` tables set nothing aside. [`merge`] keeps both sides',
    /// global first. Empty for a config built in code.
    pub fn warnings(&self) -> &[ConfigWarning] {
        &self.warnings
    }

    /// Read repo + (optional) global config files, merge with repo precedence,
    /// and validate the merged result against `repo_root`. The repo config
    /// file is read from `<repo_root>/.agents/outrig/config.toml`; a missing
    /// one loads as empty.
    pub fn load(repo_root: &Path, global_path: Option<&Path>) -> Result<Self> {
        let merged = Self::load_unvalidated(repo_root, global_path)?;
        merged.validate(Some(repo_root))?;
        Ok(merged)
    }

    /// Load config for `outrig run`, allowing `--model` to supply the selected
    /// agent's model even when no top-level `default-model` is configured.
    pub fn load_for_run(
        repo_root: &Path,
        global_path: Option<&Path>,
        agent_flag: Option<&str>,
        model_override: Option<&str>,
    ) -> Result<Self> {
        let merged = Self::load_unvalidated(repo_root, global_path)?;
        merged.validate_for_run(Some(repo_root), agent_flag, model_override)?;
        Ok(merged)
    }

    /// Load config for `outrig build`. Image building only needs the image
    /// sections, so this preserves image and general validation while skipping
    /// agent/model/provider cross-reference checks.
    pub fn load_for_build(repo_root: &Path, global_path: Option<&Path>) -> Result<Self> {
        let merged = Self::load_unvalidated(repo_root, global_path)?;
        merged.validate_for_build(Some(repo_root))?;
        Ok(merged)
    }

    /// [`load`](Self::load), with the repo config read from `repo_cfg` instead
    /// of from under `repo_root` -- `outrig --config <path>` for a file
    /// outside `.agents/outrig/`. Relative paths the file declares resolve
    /// beside it ([`ConfigSource::RepoFile`]); `repo_root` is still what the
    /// rest resolves against, the default `[workspace].host-path` and
    /// `model-path` included. Unlike `load`, a missing `repo_cfg` is an error:
    /// the caller named that file, and an empty config is not what it meant.
    ///
    /// A repo's own `<root>/.agents/outrig/config.toml` belongs to `load`
    /// instead: its relative paths are written against the root, and read
    /// through this they would resolve beside the file.
    pub fn load_file(
        repo_cfg: &Path,
        repo_root: &Path,
        global_path: Option<&Path>,
    ) -> Result<Self> {
        let merged = Self::load_file_unvalidated(repo_cfg, global_path)?;
        merged.validate(Some(repo_root))?;
        Ok(merged)
    }

    /// [`load_for_run`](Self::load_for_run), with the repo config read from
    /// `repo_cfg` as [`load_file`](Self::load_file) reads it.
    pub fn load_file_for_run(
        repo_cfg: &Path,
        repo_root: &Path,
        global_path: Option<&Path>,
        agent_flag: Option<&str>,
        model_override: Option<&str>,
    ) -> Result<Self> {
        let merged = Self::load_file_unvalidated(repo_cfg, global_path)?;
        merged.validate_for_run(Some(repo_root), agent_flag, model_override)?;
        Ok(merged)
    }

    /// [`load_for_build`](Self::load_for_build), with the repo config read
    /// from `repo_cfg` as [`load_file`](Self::load_file) reads it.
    pub fn load_file_for_build(
        repo_cfg: &Path,
        repo_root: &Path,
        global_path: Option<&Path>,
    ) -> Result<Self> {
        let merged = Self::load_file_unvalidated(repo_cfg, global_path)?;
        merged.validate_for_build(Some(repo_root))?;
        Ok(merged)
    }

    /// The repo config under `repo_root`. A missing one is not an error:
    /// `outrig run`/`outrig mcp` may run in a directory with no
    /// `.agents/outrig/config.toml`, falling back to the global config (and
    /// built-in defaults). Mirrors the global-file handling in
    /// [`load_global`](Self::load_global).
    fn read_repo(repo_root: &Path) -> Result<Self> {
        let repo_path = crate::repo::repo_config_path(repo_root);
        let repo_text = match fs::read_to_string(&repo_path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(e).path_ctx("read", &repo_path),
        };
        Self::repo_side(
            &repo_text,
            &ConfigSource::Repo {
                root: repo_root.to_path_buf(),
            },
        )
    }

    fn read_repo_file(repo_cfg: &Path) -> Result<Self> {
        let (path, text) = read_resolved(repo_cfg)?;
        let text = text.path_ctx("read", &path)?;
        Self::repo_side(&text, &ConfigSource::RepoFile { path })
    }

    /// Parse a repo-side file, hold it to the repo-only rules, and stamp it.
    fn repo_side(text: &str, src: &ConfigSource) -> Result<Self> {
        let repo_cfg = Self::parse_stamped(text, src)?;
        repo_cfg.validate_as_repo()?;
        Ok(repo_cfg)
    }

    /// Parse the file `src` names, setting aside what [`load_from_str`]
    /// does -- and, for the global config, a value that does not deserialize
    /// -- with each warning naming the file, then stamp it.
    ///
    /// [`load_from_str`]: Self::load_from_str
    fn parse_stamped(text: &str, src: &ConfigSource) -> Result<Self> {
        let global = matches!(src, ConfigSource::Global { .. });
        let mut cfg = lenient::parse(text, Some(&src.config_path()), global)?;
        cfg.stamp_source(src);
        Ok(cfg)
    }

    /// The global config at `path` alone: parsed, with `session-root` resolved
    /// against its directory, and every path entry stamped with its origin, but
    /// neither merged nor validated. A missing
    /// file loads as empty, as it does for [`load`](Self::load).
    ///
    /// Unvalidated because a global file is half of a config, not a whole one:
    /// its `default-model` may name a model the repo declares. This is the read
    /// for a caller that wants a machine-level key and has no repo -- `outrig
    /// ls` finding the session root from any directory.
    ///
    /// Read leniently, because every outrig on the machine reads this one
    /// file: a key no table has, and a value that does not deserialize, are
    /// dropped and recorded in [`warnings`](Self::warnings) rather than
    /// failing the load -- except in `[network]` and the `security` tables,
    /// where leaving a key out would give a container more than the file
    /// allows. A TOML syntax error still fails it.
    pub fn load_global(path: &Path) -> Result<Self> {
        let (path, text) = read_resolved(path)?;
        match text {
            Ok(text) => {
                let src = ConfigSource::Global { path };
                let mut cfg = Self::parse_stamped(&text, &src)?;
                cfg.resolve_global_roots(src.base_dir());
                Ok(cfg)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).path_ctx("read", &path),
        }
    }

    /// [`load`](Self::load) up to the validation: both files read, each held
    /// to its own rules, and merged, but the merged config not validated.
    ///
    /// For a caller that reports [`warnings`](Self::warnings) before
    /// validating. A key set aside can be why a rule then fails -- a misspelled
    /// `provider` leaves its model with none -- and the warning naming it is
    /// what makes that error make sense. Follow it with
    /// [`validate`](Self::validate), [`validate_for_run`](Self::validate_for_run),
    /// or [`validate_for_build`](Self::validate_for_build), as the matching
    /// `load` would.
    pub fn load_unvalidated(repo_root: &Path, global_path: Option<&Path>) -> Result<Self> {
        Self::merge_over_global(Self::read_repo(repo_root)?, global_path)
    }

    /// [`load_unvalidated`](Self::load_unvalidated), with the repo config read
    /// from `repo_cfg` as [`load_file`](Self::load_file) reads it. No repo
    /// root: nothing is resolved against one until the validation that
    /// follows.
    pub fn load_file_unvalidated(repo_cfg: &Path, global_path: Option<&Path>) -> Result<Self> {
        Self::merge_over_global(Self::read_repo_file(repo_cfg)?, global_path)
    }

    /// Merge `repo_cfg` over the global config at `global_path`, unvalidated.
    fn merge_over_global(repo_cfg: Self, global_path: Option<&Path>) -> Result<Self> {
        let global_cfg = match global_path {
            Some(g) => Self::load_global(g)?,
            None => Self::default(),
        };

        Ok(merge(global_cfg, repo_cfg))
    }

    /// Record `src` on every entry that carries a path, so the entry survives
    /// [`merge`] knowing which directory its relative paths mean. Must run
    /// before the merge: `extend` and the mount concatenation move whole
    /// entries, and nothing afterwards can tell the two files apart.
    ///
    /// Only images, mounts, bindings, and `[workspace].host-path` are stamped
    /// -- the last of those only when the file declared it, since the built-in
    /// `.` belongs to no file. Providers, models, and agents have no path
    /// fields, so a base directory would buy them nothing. A binding's paths
    /// resolve against the repository root whichever file declared it; its
    /// source says whether a repository declared it, which decides whether it
    /// needs the operator's approval.
    fn stamp_source(&mut self, src: &ConfigSource) {
        for image in self.images.values_mut() {
            image.set_config_source(src.clone());
        }
        for binding in self.bindings.values_mut() {
            binding.set_config_source(src.clone());
        }
        self.workspace.set_config_source(src.clone());
        // Every mount, wherever it lives -- one expression, so a future
        // mount-bearing block extends here and nowhere else.
        let mounts = self.workspace.mounts.iter_mut().chain(
            self.sidecars
                .values_mut()
                .flat_map(|sidecar| sidecar.mounts.iter_mut()),
        );
        for mount in mounts {
            mount.set_config_source(src.clone());
        }
    }

    /// Every variable this config names through `${VAR}`: each provider's
    /// `api-key`, and each image's `build-args` and MCP server `env`. These
    /// are OutRig's own secrets, so a binding that inherits the owner's
    /// environment inherits it without them.
    pub(crate) fn env_refs(&self) -> BTreeSet<String> {
        let keys = self.providers.values().map(|provider| {
            let (LlmProvider::OpenAi { api_key, .. } | LlmProvider::Anthropic { api_key, .. }) =
                provider;
            api_key.var_name().to_string()
        });
        let values = self.images.values().flat_map(|image| {
            image
                .build_args
                .values()
                .chain(image.mcp.values().flat_map(|server| server.env().values()))
        });
        let refs = values.filter_map(|value| match value {
            EnvValue::EnvRef(var) => Some(var.clone()),
            _ => None,
        });
        keys.chain(refs).collect()
    }

    /// Resolve `session-root` against `base`, the global file's directory, now
    /// rather than stamping it like the paths above. It is global-only, so that
    /// is the one base it can have, and no provenance is left to record.
    /// Resolving keeps the promise a loaded config has always made about this
    /// pub field -- that it is absolute -- which a value carried as written
    /// beside a private source would quietly break for every caller reading the
    /// field directly. So it is the exception to "stored as written". A repo
    /// side needs no such step: [`validate_as_repo`](Self::validate_as_repo)
    /// refuses it.
    fn resolve_global_roots(&mut self, base: &Path) {
        if let Some(path) = &mut self.session_root {
            *path = resolve_against(base, path);
        }
    }

    /// Validate every cross-reference rule documented in `doc/reference/config.md`.
    /// `repo_root: Some(_)` enables the on-disk existence checks for
    /// config-declared paths; `None` keeps the check pure-structural for unit
    /// tests.
    ///
    /// One rule needs both files, so it is checked here, on the merged value: a
    /// `${VAR}` in the `build-args` or MCP `env` of an image a repo config file
    /// declared must name a variable the global config's
    /// [`[repo].env-allow`](RepoPolicy::env_allow) lists. Images the global
    /// config declared, and images no file declared -- hand-built, or read by
    /// [`load_from_str`](Self::load_from_str) -- are not held to it.
    pub fn validate(&self, repo_root: Option<&Path>) -> Result<()> {
        validate::validate(self, repo_root)?;
        Ok(())
    }

    /// Validate the rules that apply to a *repo* config file specifically, on
    /// the unmerged value.
    ///
    /// [`validate`](Self::validate) runs on the merged config, which by
    /// construction has already taken its global-only keys from the global
    /// side and replaced any global provider a repo entry shadowed, and so
    /// cannot say which file declared what. These rules therefore run
    /// per-file, before [`merge`](fn@merge). Today there are four. A repo
    /// config may not set `session-root`, which chooses where this machine
    /// writes. It may choose `[network].mode`, but
    /// `default`, `allow`, and `deny` describe the machine's egress and stay
    /// with the operator. It may not declare `[repo].env-allow`, even empty: it
    /// says what a repo config may reference from the host environment, which a
    /// repo file must not answer for itself. And it may declare no provider:
    /// every style carries an `api-key`, a key names a host environment
    /// variable, and a repo that could pair one with its own `base-url` would
    /// decide where the operator's secret is sent.
    ///
    /// `Config::load` applies this to the repo file it reads. An embedder
    /// assembling a repo-side `Config` by hand should call it too -- `merge`
    /// is infallible: it drops a repo policy or root rather than reporting it,
    /// and it takes a repo provider as written.
    ///
    /// What a repo file may *reference* is not checked here: that needs the
    /// global config's `[repo]` list too, so [`validate`](Self::validate) checks
    /// it on the merged value. A repo-side `Config` assembled by hand carries no
    /// record of the file that declared its images, and is not held to the list.
    pub fn validate_as_repo(&self) -> Result<()> {
        validate::validate_as_repo(self)?;
        Ok(())
    }

    /// Flatten `name`'s alias graph to the ordered list of provider-shape model
    /// names it stands for.
    ///
    /// A name that is not an alias flattens to itself, so every caller can walk
    /// unconditionally. A name that is not in `[models.<name>]` at all flattens
    /// to nothing: reporting an undefined *root* belongs to the caller, which
    /// has better words for it than this function does, and both in-tree
    /// callers look the name up before walking it.
    ///
    /// The walk is depth-first in config order, splicing a nested alias's
    /// targets in at its position and keeping the first occurrence of a
    /// repeated name. Order is the config's, so the result is stable across
    /// runs -- which matters because it reaches the subagent tool schema, and
    /// an unstable schema churns the parent agent's context for nothing.
    ///
    /// Public because both crates walk this graph: `validate` checks it, and
    /// the CLI's resolver selects from it. Two implementations that had to
    /// agree on ordering and on cycle handling would be two chances to
    /// disagree.
    ///
    /// This is the whole `[models.<name>]` contract, not just the graph half,
    /// so `model_candidates(name).is_ok()` is a complete answer to "would a
    /// validated load accept this row". Six errors: [`ModelSourceMissing`] for
    /// a row that is neither shape (including a leaf reached through an alias),
    /// [`ModelSourceConflict`] naming every provider-shape field set alongside
    /// `alias`, [`ModelAliasEmpty`], an [`UnknownModelAliasTarget`], a
    /// [`ModelAliasCycle`], and [`ModelAliasTooDeep`] once a path reaches
    /// [`MODEL_ALIAS_DEPTH_MAX`] hops.
    ///
    /// Validation surfaces all six from the file that has them, and reaches the
    /// same verdict this does on the same config -- but the walk detects them
    /// itself regardless: it must not hang, overflow, or panic on a `Config`
    /// built by hand in a test or by a library embedder, neither of which goes
    /// through [`validate`](Self::validate).
    ///
    /// [`ModelSourceMissing`]: crate::config::ConfigValidationError::ModelSourceMissing
    /// [`ModelSourceConflict`]: crate::config::ConfigValidationError::ModelSourceConflict
    /// [`ModelAliasTooDeep`]: crate::config::ConfigValidationError::ModelAliasTooDeep
    ///
    /// [`ModelAliasEmpty`]: crate::config::ConfigValidationError::ModelAliasEmpty
    /// [`UnknownModelAliasTarget`]: crate::config::ConfigValidationError::UnknownModelAliasTarget
    /// [`ModelAliasCycle`]: crate::config::ConfigValidationError::ModelAliasCycle
    pub fn model_candidates<'a>(
        &'a self,
        name: &str,
    ) -> std::result::Result<Vec<&'a str>, ConfigValidationError> {
        let mut walk = AliasWalk::default();
        self.walk_model_alias(name, &mut walk)?;
        Ok(walk.flattened)
    }

    /// Check every `[models.<name>]` alias graph rule across the whole table.
    ///
    /// One pass rather than one [`model_candidates`](Self::model_candidates)
    /// call per row: `expanded` carries across roots, so a subtree shared by
    /// many names is walked once. Sound because every rule the walk enforces is
    /// a property of the subtree alone -- a node lands in `expanded` only after
    /// completing without error, so nothing reachable from it can still fail.
    /// Re-flattening per root instead is cubic on a shape as ordinary as `N`
    /// names pointing at one shared alias over `N` leaves.
    pub(crate) fn validate_model_alias_graph(
        &self,
    ) -> std::result::Result<(), ConfigValidationError> {
        let mut walk = AliasWalk::default();
        for name in self.models.keys() {
            // Per-root state; `expanded` is deliberately the one thing kept.
            walk.flattened.clear();
            walk.emitted.clear();
            walk.on_path.clear();
            self.walk_model_alias(name, &mut walk)?;
        }
        Ok(())
    }

    /// One node of [`model_candidates`](Self::model_candidates)' depth-first
    /// walk.
    ///
    /// Reads the shape fields directly rather than going through
    /// [`Model::source`], which panics on a row that sets both shapes or
    /// neither. This walk has to stay total on exactly that input, so it
    /// enforces the same shape contract `validate` does and returns an error
    /// where `source` would abort.
    ///
    /// Returns `Some(height)` for an alias row -- how much deeper than itself
    /// its subtree reaches, in alias hops -- and `None` for a leaf. The height
    /// is what makes the depth bound independent of walk order: a memoized node
    /// re-entered from a longer path is checked against `depth + height`, so
    /// `validate` (which shares `expanded` across roots) and a fresh
    /// [`model_candidates`](Self::model_candidates) reach the same verdict on
    /// the same config. Without it, a bottom-up name ordering expands every
    /// suffix before its parent, and a chain past the limit validates cleanly
    /// only to fail later at resolve time.
    fn walk_model_alias<'a>(
        &'a self,
        name: &str,
        walk: &mut AliasWalk<'a>,
    ) -> std::result::Result<Option<usize>, ConfigValidationError> {
        // Borrow the key rather than the caller's `name`, so everything the
        // walk accumulates lives as long as `self`. Only the root can be
        // absent -- every target is checked against the table before it is
        // recursed into -- and an absent root contributes nothing.
        let Some((key, model)) = self.models.get_key_value(name) else {
            return Ok(None);
        };
        let key: &'a str = key.as_str();

        let Some(targets) = model.alias.as_deref() else {
            // A leaf still has to name a provider; a row that is neither shape
            // is what `Model::source` panics on, and callers that filter on
            // this walk are entitled to have it rejected rather than emitted.
            if model.provider.is_none() {
                return Err(ConfigValidationError::ModelSourceMissing {
                    model: key.to_string(),
                });
            }
            if walk.emitted.insert(key) {
                walk.flattened.push(key);
            }
            return Ok(None);
        };

        // An alias carrying any provider-shape field. Validation rejects it
        // before the resolver ever walks, so this is the hand-built case --
        // and silently treating it as an alias would discard fields the author
        // meant. The list is the shared collector, so the message names every
        // offender rather than the first two.
        let conflicts = model.provider_shape_fields();
        if !conflicts.is_empty() {
            let mut fields = vec!["alias"];
            fields.extend(conflicts);
            return Err(ConfigValidationError::ModelSourceConflict {
                model: key.to_string(),
                fields,
            });
        }

        if targets.is_empty() {
            return Err(ConfigValidationError::ModelAliasEmpty {
                model: key.to_string(),
            });
        }

        let depth = walk.on_path.len();
        let too_deep = || ConfigValidationError::ModelAliasTooDeep {
            model: key.to_string(),
            max: MODEL_ALIAS_DEPTH_MAX,
        };
        if depth >= MODEL_ALIAS_DEPTH_MAX {
            return Err(too_deep());
        }

        // Already walked to completion through some other path, so its leaves
        // are in `flattened` already and in the right places. Its *height* is
        // still checked against this path: arriving somewhere shallow first
        // must not license a longer route to the same subtree.
        if let Some(&height) = walk.expanded.get(&key) {
            if depth + height >= MODEL_ALIAS_DEPTH_MAX {
                return Err(too_deep());
            }
            return Ok(Some(height));
        }

        let mut height = 0;
        walk.on_path.push(key);
        for target in targets {
            if !self.models.contains_key(target.as_str()) {
                return Err(ConfigValidationError::UnknownModelAliasTarget {
                    model: key.to_string(),
                    target: target.clone(),
                });
            }
            // A cycle is a name already *on the current path*, not one already
            // emitted: `alias = ["b", "b"]` is a repeat, which the `emitted`
            // set collapses, while `a -> b -> a` is a cycle. Using `emitted`
            // for both would call the first one an error.
            if let Some(start) = walk
                .on_path
                .iter()
                .position(|seen| *seen == target.as_str())
            {
                let mut cycle: Vec<&str> = walk.on_path[start..].to_vec();
                cycle.push(walk.on_path[start]);
                return Err(ConfigValidationError::ModelAliasCycle {
                    cycle: cycle.join(" -> "),
                });
            }
            // Only an alias target adds a hop; a leaf ends the path here.
            if let Some(child) = self.walk_model_alias(target, walk)? {
                height = height.max(child + 1);
            }
        }
        walk.on_path.pop();
        walk.expanded.insert(key, height);
        Ok(Some(height))
    }

    /// Validate `[workspace.mounts]` -- including any appended at runtime, e.g.
    /// from `--volume` -- without re-running LLM/image checks: container paths
    /// absolute, not `/`, unique (against each other and the primary workspace
    /// mount), and (when `repo_root` is set) host paths existing directories.
    pub fn validate_workspace_mounts(&self, repo_root: Option<&Path>) -> Result<()> {
        validate::validate_workspace_mounts(self, repo_root)?;
        Ok(())
    }

    /// [`validate`](Self::validate) as [`load_for_run`](Self::load_for_run)
    /// applies it: `--model` stands in for the model of the agent the run
    /// selects, and only that one.
    pub fn validate_for_run(
        &self,
        repo_root: Option<&Path>,
        agent_flag: Option<&str>,
        model_override: Option<&str>,
    ) -> Result<()> {
        let agent_model_override = model_override.and(agent_flag.or(self.default_agent.as_deref()));
        validate::validate_with_options(
            self,
            repo_root,
            validate::ValidationOptions {
                agent_model_override,
                validate_llm: true,
            },
        )?;
        Ok(())
    }

    /// [`validate`](Self::validate) as [`load_for_build`](Self::load_for_build)
    /// applies it: the image and general rules, without the
    /// agent/model/provider cross-references a build does not use.
    pub fn validate_for_build(&self, repo_root: Option<&Path>) -> Result<()> {
        validate::validate_with_options(
            self,
            repo_root,
            validate::ValidationOptions {
                agent_model_override: None,
                validate_llm: false,
            },
        )?;
        Ok(())
    }
}

/// Whether a provider requires the user's and the model's turns to alternate
/// in every request: `[providers.<name>].role-alternation`.
///
/// A conversation sent in part can put one role after itself. A turn brought
/// back without the rest of its round opens on the model's reply right after
/// an earlier round's closing reply, and a round cut short ends on tool
/// results that the next round's opening follows. Anthropic's API merges such
/// a pair and OpenAI's accepts it. A provider or gateway that refuses it -- a
/// Bedrock-backed Claude behind an OpenAI-compatible gateway is one -- is
/// `Strict`, and `outrig run-new` then sends it a conversation that alternates
/// by construction, leaving out the turns that would not, which each call's
/// manifest records. `outrig run` sends whole rounds, and reads the key without
/// acting on it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum RoleAlternation {
    /// The provider merges or accepts two messages of one role in a row. The
    /// default.
    #[default]
    Relaxed,
    /// The provider refuses them: every request must alternate.
    Strict,
}

/// Optional settings for an OpenAI-compatible provider.
///
/// Non-exhaustive, so a future connection setting is an addition rather than a
/// change to [`LlmProvider::openai`]'s signature -- which also means the fields
/// alone cannot construct this from outside the crate. Start from
/// [`new`](Self::new) and set what you need with `with_*`.
#[derive(Debug, Clone, Default, PartialEq)]
#[non_exhaustive]
pub struct OpenAiOptions {
    /// Per-request HTTP timeout in seconds. `None` uses the client default.
    pub request_timeout_secs: Option<u64>,
    /// Transient-retry budget in seconds. `None` uses the top-level setting,
    /// then [`DEFAULT_RETRY_BUDGET_SECS`].
    pub retry_budget_secs: Option<u64>,
    /// Whether the provider requires the user's and the model's turns to
    /// alternate. `None` is [`RoleAlternation::Relaxed`].
    pub role_alternation: Option<RoleAlternation>,
}

impl OpenAiOptions {
    /// Options that override nothing: every field below stays `None`.
    pub fn new() -> Self {
        Self::default()
    }

    /// Bound each HTTP request to this provider at `secs`, rather than the
    /// client default.
    pub fn with_request_timeout_secs(mut self, secs: u64) -> Self {
        self.request_timeout_secs = Some(secs);
        self
    }

    /// Keep retrying a transiently-failing call to this provider for at most
    /// `secs`, rather than for the top-level budget. `0` disables retries.
    pub fn with_retry_budget_secs(mut self, secs: u64) -> Self {
        self.retry_budget_secs = Some(secs);
        self
    }

    /// Say whether the provider requires the user's and the model's turns to
    /// alternate: see [`RoleAlternation`].
    pub fn with_role_alternation(mut self, alternation: RoleAlternation) -> Self {
        self.role_alternation = Some(alternation);
        self
    }
}

/// Optional settings for an Anthropic provider.
///
/// The twin of [`OpenAiOptions`], and deliberately a separate type rather than
/// one shared with it: either provider is free to grow a setting the other has
/// no meaning for, which a shared struct could only ignore in silence on one of
/// the two paths. Construct it the same way: [`new`](Self::new), then `with_*`.
#[derive(Debug, Clone, Default, PartialEq)]
#[non_exhaustive]
pub struct AnthropicOptions {
    /// Per-request HTTP timeout in seconds. `None` uses the client default.
    pub request_timeout_secs: Option<u64>,
    /// Transient-retry budget in seconds. `None` uses the top-level setting,
    /// then [`DEFAULT_RETRY_BUDGET_SECS`].
    pub retry_budget_secs: Option<u64>,
    /// Whether the provider requires the user's and the model's turns to
    /// alternate. `None` is [`RoleAlternation::Relaxed`].
    pub role_alternation: Option<RoleAlternation>,
}

impl AnthropicOptions {
    /// Options that override nothing: every field below stays `None`.
    pub fn new() -> Self {
        Self::default()
    }

    /// Bound each HTTP request to this provider at `secs`, rather than the
    /// client default.
    pub fn with_request_timeout_secs(mut self, secs: u64) -> Self {
        self.request_timeout_secs = Some(secs);
        self
    }

    /// Keep retrying a transiently-failing call to this provider for at most
    /// `secs`, rather than for the top-level budget. `0` disables retries.
    pub fn with_retry_budget_secs(mut self, secs: u64) -> Self {
        self.retry_budget_secs = Some(secs);
        self
    }

    /// Say whether the provider requires the user's and the model's turns to
    /// alternate: see [`RoleAlternation`].
    pub fn with_role_alternation(mut self, alternation: RoleAlternation) -> Self {
        self.role_alternation = Some(alternation);
        self
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "style",
    rename_all = "kebab-case",
    rename_all_fields = "kebab-case",
    deny_unknown_fields
)]
#[non_exhaustive]
pub enum LlmProvider {
    // The kebab-case rule auto-converts `OpenAi` to `open-ai`; the doc'd
    // tag is `openai`, so override per-variant.
    #[serde(rename = "openai")]
    #[non_exhaustive]
    OpenAi {
        base_url: String,
        api_key: ApiKeyRef,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        request_timeout_secs: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        retry_budget_secs: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        role_alternation: Option<RoleAlternation>,
    },
    /// Anthropic's native Messages API: `POST {base-url}/v1/messages` with
    /// `x-api-key` auth. Distinct from reaching Claude through an
    /// OpenAI-compatible bridge, which is a `style = "openai"` provider
    /// pointed at that bridge.
    #[non_exhaustive]
    Anthropic {
        base_url: String,
        api_key: ApiKeyRef,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        request_timeout_secs: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        retry_budget_secs: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        role_alternation: Option<RoleAlternation>,
    },
}

impl LlmProvider {
    /// The `style` tag this variant serializes as, for diagnostics and
    /// user-facing labels. Written next to the serde attributes above so the
    /// two cannot drift: a message naming a style the config file does not
    /// accept sends the reader looking for the wrong key.
    pub fn style(&self) -> &'static str {
        match self {
            Self::OpenAi { .. } => "openai",
            Self::Anthropic { .. } => "anthropic",
        }
    }

    /// An OpenAI-compatible provider at `base_url`.
    pub fn openai(base_url: impl Into<String>, api_key: ApiKeyRef, options: OpenAiOptions) -> Self {
        Self::OpenAi {
            base_url: base_url.into(),
            api_key,
            request_timeout_secs: options.request_timeout_secs,
            retry_budget_secs: options.retry_budget_secs,
            role_alternation: options.role_alternation,
        }
    }

    /// Anthropic's native Messages API at `base_url` -- the official endpoint
    /// is `https://api.anthropic.com`. A trailing `/v1`, `/messages`, or
    /// `/v1/messages` is normalized away by the client, so either form works.
    pub fn anthropic(
        base_url: impl Into<String>,
        api_key: ApiKeyRef,
        options: AnthropicOptions,
    ) -> Self {
        Self::Anthropic {
            base_url: base_url.into(),
            api_key,
            request_timeout_secs: options.request_timeout_secs,
            retry_budget_secs: options.retry_budget_secs,
            role_alternation: options.role_alternation,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
#[non_exhaustive]
pub struct Model {
    /// The `[providers.<name>]` entry serving this model. Mutually exclusive
    /// with [`alias`](field@Self::alias): a row names a provider *or* other models,
    /// never both. `Option` for that reason rather than because a model may
    /// legitimately have neither -- validation rejects a row with neither, and
    /// [`source`](Self::source) panics on one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// Other models this name stands for, in preference order. One name for one
    /// model (`alias = "opus-5"`) or one name for a set of provider-equivalent
    /// rows (`alias = ["opus-5-bedrock", "opus-5-anthropic"]`); both spellings
    /// deserialize here. Mutually exclusive with every provider-shape field
    /// above and below, including `max-tokens` and `context-window`.
    #[serde(
        default,
        deserialize_with = "deserialize_string_or_vec_string",
        skip_serializing_if = "Option::is_none"
    )]
    pub alias: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identifier: Option<String>,
    /// Output-token ceiling for turns run against this model, used when the
    /// agent does not set its own. Anthropic *requires* one per request and
    /// only supplies a default for the model identifiers it recognizes, so a
    /// Claude model naming anything else needs this (or the agent's) set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    /// The model's whole context window in tokens -- a request and its reply
    /// together -- as its provider publishes it. It configures nothing on the
    /// provider's side: it tells `outrig run-new` how much of the conversation
    /// one model call can carry, after holding back room for the reply. Unset,
    /// an assumed window is used and a warning says so; it is never inferred
    /// from [`identifier`](field@Self::identifier). `outrig run` ignores it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u32>,
}

impl Model {
    /// A model served by the `[providers.<provider>]` entry of that name.
    /// Every other field is optional and stays unset; assign the ones the
    /// provider needs. The counterpart of [`ModelSourceRef::Provider`].
    pub fn new(provider: impl Into<String>) -> Self {
        Self {
            provider: Some(provider.into()),
            ..Self::sourceless()
        }
    }

    /// A name standing for one or more other models, in preference order. The
    /// counterpart of [`ModelSourceRef::Alias`].
    ///
    /// Takes an iterator rather than offering a second single-target
    /// constructor: `Model::alias(["opus-5"])` is marginally noisier than the
    /// `alias = "opus-5"` it mirrors, which is cheaper than a second published
    /// method meaning the same thing.
    pub fn alias<I, S>(targets: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            alias: Some(targets.into_iter().map(Into::into).collect()),
            ..Self::sourceless()
        }
    }

    /// Every field at its serde default, which means *neither* source shape is
    /// set. Deliberately private, and deliberately not a `Default` impl, for
    /// the reason [`ImageConfig::sourceless`] gives: it is the state
    /// [`source`](Self::source) panics on, so it is a base for the two
    /// constructors above rather than a value worth handing out.
    fn sourceless() -> Self {
        Self {
            provider: None,
            alias: None,
            identifier: None,
            max_tokens: None,
            context_window: None,
        }
    }

    /// Every provider-shape field this row actually sets, in declaration order.
    ///
    /// Empty for a well-formed alias, which is what makes it the conflict list:
    /// anything here alongside `alias` is a contradiction, `max-tokens` and
    /// `context-window` included.
    pub(crate) fn provider_shape_fields(&self) -> Vec<&'static str> {
        [
            (self.provider.is_some(), "provider"),
            (self.identifier.is_some(), "identifier"),
            (self.max_tokens.is_some(), "max-tokens"),
            (self.context_window.is_some(), "context-window"),
        ]
        .into_iter()
        .filter(|(present, _)| *present)
        .map(|(_, field)| field)
        .collect()
    }

    /// Return the discriminated source variant. Panics if validation has not
    /// run (i.e. both or neither shape is set). Every real call path goes
    /// through [`Config::load`], which validates -- and unlike the rest of the
    /// model rules, the shape check is not gated on `validate-llm`, so
    /// `outrig build` establishes this invariant too.
    ///
    /// Callers that must stay total on a hand-built `Config` -- the alias walk
    /// and the resolver's selectability check -- read the fields directly
    /// instead.
    pub fn source(&self) -> ModelSourceRef<'_> {
        match (&self.provider, &self.alias) {
            (Some(provider), None) => ModelSourceRef::Provider { provider },
            (None, Some(targets)) => ModelSourceRef::Alias { targets },
            _ => panic!(
                "Model::source() called on an unvalidated config; \
                 call Config::validate() first"
            ),
        }
    }
}

/// Discriminated view of what a `[models.<name>]` row names -- a provider that
/// serves it, or other models it stands for. Returned by [`Model::source`].
///
/// Deliberately carries only the discriminant: every reader already holds the
/// `&Model` and reads the provider-shape fields (`identifier`, `max-tokens`,
/// `context-window`) off it directly, so restating them here would be surface
/// with no consumer. The per-variant `#[non_exhaustive]` keeps adding one
/// additive.
#[non_exhaustive]
pub enum ModelSourceRef<'a> {
    #[non_exhaustive]
    Provider { provider: &'a str },
    #[non_exhaustive]
    Alias { targets: &'a [String] },
}

/// Whether an agent's system prompt carries the repository's own instructions
/// for agents: `[agents.<name>].repo-instructions`.
///
/// `outrig run-new` reads the file inside the session's container, from the
/// workspace's container path, so the text is what the agent would see by
/// reading it. `outrig run` reads the key without acting on it, and a child
/// agent is never given the file: its parent writes its prompt.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum RepoInstructions {
    /// `AGENTS.md` at the workspace's root, else `.agents/AGENTS.md`, whichever
    /// is found first, follows the orientation and the agent's `preamble`. A
    /// workspace with neither adds nothing. The default.
    #[default]
    AgentsMd,
    /// The system prompt carries none of the repository's files.
    None,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
#[non_exhaustive]
pub struct Agent {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preamble: Option<String>,
    /// Whether the system prompt carries the repository's instructions for
    /// agents, after the preamble. Unset means [`RepoInstructions::AgentsMd`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo_instructions: Option<RepoInstructions>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_max: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_result_max: Option<u32>,
    /// Whether this agent may launch subagents. Unset means enabled -- the
    /// `outrig__` subagent tools are registered unless an agent opts out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subagents: Option<bool>,
    /// Per-agent override of the maximum subagent nesting depth. Falls back to
    /// the top-level `subagent-depth-max`, then [`DEFAULT_SUBAGENT_DEPTH_MAX`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subagent_depth_max: Option<u32>,
    /// Per-agent override of the number of live subagents it may launch. Falls
    /// back to the top-level `subagent-width-max`, then
    /// [`DEFAULT_SUBAGENT_WIDTH_MAX`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subagent_width_max: Option<u32>,
    /// Per-agent override of the top-level `children-max`, for a session this
    /// agent runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub children_max: Option<u32>,
    /// Per-agent override of the top-level `model-concurrency-max`, for a
    /// session this agent runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_concurrency_max: Option<u32>,
    /// Per-agent override of the top-level `requests-max`, for a session this
    /// agent runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requests_max: Option<u32>,
    /// Per-agent override of the top-level `token-budget`, for a session this
    /// agent runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_budget: Option<u64>,
}

impl Agent {
    /// Whether the parent-side `outrig__` subagent tools are registered for
    /// this agent. Defaults to enabled when the key is absent.
    pub fn subagents_enabled(&self) -> bool {
        self.subagents.unwrap_or(true)
    }
}

impl Config {
    /// The limits a session of `agent` runs under: for each, the agent's own
    /// `[agents.<name>]` value, else the top level's, else the default. With
    /// no agent, or one this config does not define, the top level's.
    pub(crate) fn limits(&self, agent: Option<&str>) -> Limits {
        let agent = agent.and_then(|name| self.agents.get(name));
        let own = |key: fn(&Agent) -> Option<u32>| agent.and_then(key);
        Limits {
            depth_max: own(|a| a.subagent_depth_max)
                .or(self.subagent_depth_max)
                .unwrap_or(DEFAULT_SUBAGENT_DEPTH_MAX),
            children_max: own(|a| a.children_max)
                .or(self.children_max)
                .unwrap_or(DEFAULT_CHILDREN_MAX),
            model_concurrency_max: own(|a| a.model_concurrency_max)
                .or(self.model_concurrency_max)
                .unwrap_or(DEFAULT_MODEL_CONCURRENCY_MAX),
            requests_max: own(|a| a.requests_max)
                .or(self.requests_max)
                .unwrap_or(DEFAULT_REQUESTS_MAX),
            token_budget: agent.and_then(|a| a.token_budget).or(self.token_budget),
        }
    }
}

/// `[workspace].host-path` when the config declares none. Read it through
/// [`Workspace::host_path`].
const DEFAULT_WORKSPACE_HOST_PATH: &str = ".";
/// `[workspace].container-path` when the config declares none. Read it through
/// [`Workspace::container_path`].
const DEFAULT_WORKSPACE_CONTAINER_PATH: &str = "/workspace";

/// `None` on a primary field means the config file did not declare the key,
/// which is what lets [`merge`] fill it from the global config instead of
/// overwriting it with a default.
///
/// The two primary fields are private because `host_path` is paired with the
/// [`ConfigSource`] it is resolved against: a value replaced without clearing
/// that pairing would be resolved against the directory of a file it never
/// came from, and the primary mount is read-write. Read them with
/// [`host_path`](Self::host_path) / [`container_path`](Self::container_path),
/// which apply the documented defaults, or with
/// [`declared_host_path`](Self::declared_host_path) /
/// [`declared_container_path`](Self::declared_container_path) to see
/// declaration state. Write them with the setters, which keep the pairing
/// honest.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
#[non_exhaustive]
pub struct Workspace {
    #[serde(skip_serializing_if = "Option::is_none")]
    host_path: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    container_path: Option<PathBuf>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub mounts: Vec<MountConfig>,
    /// Source of `host_path` alone, set at load time, never deserialized. The
    /// two primary fields are selected independently. See [`ConfigSource`].
    #[serde(skip)]
    source: Option<ConfigSource>,
}

impl Workspace {
    /// A workspace mapping `host_path` to `container_path`, with no extra
    /// mounts. Both are declared, so neither is inherited by [`merge`];
    /// [`Workspace::default`] declares nothing and inherits both.
    pub fn new(host_path: impl Into<PathBuf>, container_path: impl Into<PathBuf>) -> Self {
        Self {
            host_path: Some(host_path.into()),
            container_path: Some(container_path.into()),
            ..Self::default()
        }
    }

    /// The declared `host-path`, or `.`. Relative values are resolved by
    /// [`resolved_host_path`](Self::resolved_host_path).
    pub fn host_path(&self) -> &Path {
        self.host_path
            .as_deref()
            .unwrap_or(Path::new(DEFAULT_WORKSPACE_HOST_PATH))
    }

    /// The declared `container-path`, or `/workspace`.
    pub fn container_path(&self) -> &Path {
        self.container_path
            .as_deref()
            .unwrap_or(Path::new(DEFAULT_WORKSPACE_CONTAINER_PATH))
    }

    /// `host-path` exactly as declared, or `None` when the config was silent --
    /// the state [`merge`] acts on, as opposed to the value it resolves to.
    pub fn declared_host_path(&self) -> Option<&Path> {
        self.host_path.as_deref()
    }

    /// `container-path` exactly as declared, or `None` when the config was
    /// silent. See [`declared_host_path`](Self::declared_host_path).
    pub fn declared_container_path(&self) -> Option<&Path> {
        self.container_path.as_deref()
    }

    /// Declare `host-path`, dropping any recorded [`ConfigSource`]: the new
    /// value did not come from a config file, so it resolves against the
    /// `repo_root` passed to
    /// [`resolved_host_path`](Self::resolved_host_path) like any other
    /// hand-built path.
    pub fn set_host_path(&mut self, host_path: impl Into<PathBuf>) {
        self.host_path = Some(host_path.into());
        self.source = None;
    }

    /// Declare `container-path`. Carries no provenance -- a container path
    /// resolves against nothing. [`Config::validate`] requires it absolute and
    /// not `/`.
    pub fn set_container_path(&mut self, container_path: impl Into<PathBuf>) {
        self.container_path = Some(container_path.into());
    }

    /// Nothing here came from a config file, so serializing would emit an empty
    /// `[workspace]` table. Lets [`Config`] skip the block the way it skips
    /// every other one with nothing to say.
    fn is_default(&self) -> bool {
        self == &Self::default()
    }

    /// Only a *declared* `host_path` gets a source: the built-in `.` is not the
    /// global file's `.`, and stamping it would move an undeclared workspace
    /// off the repo root.
    fn set_config_source(&mut self, source: ConfigSource) {
        if self.host_path.is_some() {
            self.source = Some(source);
        }
    }

    /// Take each primary field the repo file left undeclared from `global`.
    /// `host_path` travels with its provenance, so an inherited value keeps
    /// resolving against the file that declared it.
    fn inherit_missing_primary_fields(&mut self, global: &Self) {
        if self.host_path.is_none() {
            self.host_path.clone_from(&global.host_path);
            self.source.clone_from(&global.source);
        }
        if self.container_path.is_none() {
            self.container_path.clone_from(&global.container_path);
        }
    }

    /// [`host_path`](Self::host_path) made absolute against the directory of
    /// the config file that declared it, or under the home directory when its
    /// first component is `~`. A hand-built or default workspace falls back to
    /// `repo_root`, preserving the library API's existing behavior.
    pub fn resolved_host_path(&self, repo_root: &Path) -> PathBuf {
        resolve_against(
            source_base_dir(self.source.as_ref(), repo_root),
            self.host_path(),
        )
    }

    /// [`resolved_host_path`](Self::resolved_host_path), with `.` and `..`
    /// resolved, where [`known_path`] can say.
    pub(crate) fn known_host_path(&self, repo_root: Option<&Path>) -> Option<PathBuf> {
        known_path(self.source.as_ref(), self.host_path(), repo_root)
    }

    /// The config file that declared `host-path`, or `None` when none did: the
    /// built-in `.`, or a value from [`Workspace::new`] or
    /// [`set_host_path`](Self::set_host_path).
    pub fn config_source(&self) -> Option<&ConfigSource> {
        self.source.as_ref()
    }

    /// The file to name in a diagnostic about `host-path`, or `None` when no
    /// file declared it: the built-in `.`, or a value from [`Workspace::new`]
    /// or [`set_host_path`](Self::set_host_path). Deliberately not defaulted
    /// to the repo config, for the reason [`ImageConfig::declared_in`] gives.
    pub(crate) fn declared_in(&self) -> Option<PathBuf> {
        self.source.as_ref().map(ConfigSource::config_path)
    }
}

/// One `[[workspace.mounts]]` or `[[sidecars.<sc>.mounts]]` entry.
///
/// The fields are private because `host_path` is paired with the
/// [`ConfigSource`] it is resolved against: a value replaced without clearing
/// that pairing would be resolved against the directory of a file it never came
/// from, and a mount may be read-write. Read them with
/// [`host_path`](Self::host_path) /
/// [`declared_container_path`](Self::declared_container_path) /
/// [`access`](Self::access); write them with the setters, which keep the
/// pairing honest. The other two carry no provenance and are private only so
/// that one syntax reads the whole struct.
///
/// An undeclared `container-path` is the resolved host path: the directory
/// appears in the container where it is on the host, which is what a session
/// with a binding requires of every mount.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
#[non_exhaustive]
pub struct MountConfig {
    host_path: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    container_path: Option<PathBuf>,
    #[serde(default)]
    access: MountAccess,
    /// Set at load time, never deserialized. See [`ConfigSource`].
    #[serde(skip)]
    #[schemars(skip)]
    source: Option<ConfigSource>,
}

impl MountConfig {
    /// An extra bind mount of `host_path` at `container_path`.
    pub fn new(
        host_path: impl Into<PathBuf>,
        container_path: impl Into<PathBuf>,
        access: MountAccess,
    ) -> Self {
        Self {
            host_path: host_path.into(),
            container_path: Some(container_path.into()),
            access,
            source: None,
        }
    }

    /// `host-path` exactly as declared. Relative values are resolved by
    /// [`resolved_host_path`](Self::resolved_host_path).
    pub fn host_path(&self) -> &Path {
        &self.host_path
    }

    /// `container-path` exactly as declared, or `None` when the config was
    /// silent and the mount appears at its host path.
    pub fn declared_container_path(&self) -> Option<&Path> {
        self.container_path.as_deref()
    }

    /// The in-container mount point: the declared `container-path`, or else
    /// [`resolved_host_path`](Self::resolved_host_path) with `.` and `..`
    /// resolved.
    pub fn resolved_container_path(&self, repo_root: &Path) -> PathBuf {
        match &self.container_path {
            Some(path) => path.clone(),
            None => lexical(&self.resolved_host_path(repo_root)),
        }
    }

    /// [`resolved_container_path`](Self::resolved_container_path) where it can
    /// be known: declared, or defaulted from a host path [`known_path`] can
    /// resolve.
    pub(crate) fn known_container_path(&self, repo_root: Option<&Path>) -> Option<PathBuf> {
        match &self.container_path {
            Some(path) => Some(path.clone()),
            None => self.known_host_path(repo_root),
        }
    }

    /// [`resolved_host_path`](Self::resolved_host_path), with `.` and `..`
    /// resolved, where [`known_path`] can say.
    pub(crate) fn known_host_path(&self, repo_root: Option<&Path>) -> Option<PathBuf> {
        known_path(self.source.as_ref(), self.host_path(), repo_root)
    }

    /// Whether the container may write through this mount.
    pub fn access(&self) -> MountAccess {
        self.access
    }

    /// Declare `host-path`, dropping any recorded [`ConfigSource`]: the new
    /// value did not come from a config file, so it resolves against the
    /// `repo_root` passed to
    /// [`resolved_host_path`](Self::resolved_host_path) like any other
    /// hand-built path. The same rule as
    /// [`Workspace::set_host_path`](Workspace::set_host_path), one level down.
    pub fn set_host_path(&mut self, host_path: impl Into<PathBuf>) {
        self.host_path = host_path.into();
        self.source = None;
    }

    /// Declare `container-path`. Carries no provenance -- a container path is
    /// absolute and resolves against nothing.
    pub fn set_container_path(&mut self, container_path: impl Into<PathBuf>) {
        self.container_path = Some(container_path.into());
    }

    /// Declare `access`.
    pub fn set_access(&mut self, access: MountAccess) {
        self.access = access;
    }

    /// The config file this mount was declared in, or `None` for a
    /// hand-built entry that never went through [`Config::load`].
    pub fn config_source(&self) -> Option<&ConfigSource> {
        self.source.as_ref()
    }

    pub(crate) fn set_config_source(&mut self, source: ConfigSource) {
        self.source = Some(source);
    }

    /// `host_path` made absolute, against the directory of the file that
    /// declared it, or under the home directory when its first component is
    /// `~`.
    /// `repo_root` is the fallback for an entry with no recorded source, which
    /// is every hand-built [`MountConfig`].
    ///
    /// Global and repo mount lists are *concatenated* by [`merge`], so one base
    /// directory provably cannot be right for every element of the result --
    /// this is per-entry for that reason.
    pub fn resolved_host_path(&self, repo_root: &Path) -> PathBuf {
        resolve_against(
            source_base_dir(self.source.as_ref(), repo_root),
            self.host_path(),
        )
    }

    /// The file to name in a diagnostic about this mount, or `None` for a
    /// hand-built entry. Deliberately not defaulted to the repo config, for the
    /// reason [`ImageConfig::declared_in`] gives: unlike a base directory, a
    /// filename in an error message is a *claim*, and naming a file that never
    /// mentioned this mount would be a fabrication.
    pub(crate) fn declared_in(&self) -> Option<PathBuf> {
        self.source.as_ref().map(ConfigSource::config_path)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum MountAccess {
    #[default]
    ReadOnly,
    ReadWrite,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum NetworkMode {
    #[default]
    Default,
    Audit,
    Filter,
}

impl std::str::FromStr for NetworkMode {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s {
            "default" => Ok(Self::Default),
            "audit" => Ok(Self::Audit),
            "filter" => Ok(Self::Filter),
            _ => Err("expected one of: default, audit, filter".to_string()),
        }
    }
}

impl std::fmt::Display for NetworkMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Default => f.write_str("default"),
            Self::Audit => f.write_str("audit"),
            Self::Filter => f.write_str("filter"),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum NetworkAction {
    Allow,
    #[default]
    Deny,
}

impl NetworkAction {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Deny => "deny",
        }
    }

    fn is_deny(action: &Self) -> bool {
        *action == Self::Deny
    }
}

impl std::fmt::Display for NetworkAction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
#[non_exhaustive]
pub struct NetworkEntry {
    pub host: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
}

impl NetworkEntry {
    pub fn new(host: impl Into<String>) -> Self {
        Self {
            host: host.into(),
            port: None,
        }
    }

    pub fn with_port(host: impl Into<String>, port: u16) -> Self {
        Self {
            host: host.into(),
            port: Some(port),
        }
    }

    pub(crate) fn validate(&self, path: &str) -> std::result::Result<(), String> {
        if self.port == Some(0) {
            return Err(format!("{path}.port must be between 1 and 65535"));
        }
        parse_network_host_pattern(&self.host)
            .map(|_| ())
            .map_err(|e| format!("{path}.host {e}"))
    }
}

impl<'de> Deserialize<'de> for NetworkEntry {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields, rename_all = "kebab-case")]
        struct Table {
            host: String,
            #[serde(default)]
            port: Option<u16>,
        }

        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Repr {
            String(String),
            Table(Table),
        }

        match Repr::deserialize(deserializer)? {
            Repr::String(s) => parse_network_entry_string(&s).map_err(serde::de::Error::custom),
            Repr::Table(t) => Ok(Self {
                host: t.host,
                port: t.port,
            }),
        }
    }
}

fn parse_network_entry_string(s: &str) -> std::result::Result<NetworkEntry, String> {
    if s.is_empty() {
        return Err("network entry must not be empty".to_string());
    }
    if let Some(rest) = s.strip_prefix('[') {
        let Some(end) = rest.find(']') else {
            return Err("bracketed IPv6 network entry is missing `]`".to_string());
        };
        let host = &rest[..end];
        let suffix = &rest[end + 1..];
        let port = if suffix.is_empty() {
            None
        } else {
            let Some(raw) = suffix.strip_prefix(':') else {
                return Err("bracketed network entry may only be followed by `:<port>`".to_string());
            };
            Some(parse_network_port(raw)?)
        };
        return Ok(NetworkEntry {
            host: host.to_string(),
            port,
        });
    }

    if s.matches(':').count() == 1 {
        let (host, raw_port) = s
            .rsplit_once(':')
            .expect("single colon implies split_once succeeds");
        if raw_port.is_empty() {
            return Err("network entry port must not be empty".to_string());
        }
        if raw_port.chars().all(|c| c.is_ascii_digit()) {
            return Ok(NetworkEntry {
                host: host.to_string(),
                port: Some(parse_network_port(raw_port)?),
            });
        }
        return Err("network entry port must be an integer".to_string());
    }

    Ok(NetworkEntry {
        host: s.to_string(),
        port: None,
    })
}

fn parse_network_port(raw: &str) -> std::result::Result<u16, String> {
    if raw.is_empty() {
        return Err("network entry port must not be empty".to_string());
    }
    let port = raw
        .parse::<u16>()
        .map_err(|_| "network entry port must be between 1 and 65535".to_string())?;
    if port == 0 {
        return Err("network entry port must be between 1 and 65535".to_string());
    }
    Ok(port)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
#[non_exhaustive]
pub struct NetworkPolicy {
    /// The verdict for a connection no entry matches, and for traffic the
    /// interceptor cannot evaluate at all: anything but TCP and DNS over
    /// UDP/53 is dropped in the kernel under `Deny` and passes unrecorded
    /// under `Allow`.
    #[serde(default, skip_serializing_if = "NetworkAction::is_deny")]
    pub default: NetworkAction,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allow: Vec<NetworkEntry>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deny: Vec<NetworkEntry>,
}

impl Default for NetworkPolicy {
    fn default() -> Self {
        Self {
            default: NetworkAction::Deny,
            allow: Vec::new(),
            deny: Vec::new(),
        }
    }
}

impl NetworkPolicy {
    pub fn builder() -> NetworkPolicyBuilder {
        NetworkPolicyBuilder::default()
    }

    pub(crate) fn allow_all() -> Self {
        Self {
            default: NetworkAction::Allow,
            allow: Vec::new(),
            deny: Vec::new(),
        }
    }

    pub(crate) fn has_entries(&self) -> bool {
        !self.allow.is_empty() || !self.deny.is_empty()
    }

    pub(crate) fn validate(&self, require_entries: bool) -> std::result::Result<(), String> {
        if require_entries && !self.has_entries() {
            return Err(
                "network filter mode requires at least one allow or deny entry".to_string(),
            );
        }
        for (idx, entry) in self.allow.iter().enumerate() {
            entry.validate(&format!("network.allow[{idx}]"))?;
        }
        for (idx, entry) in self.deny.iter().enumerate() {
            entry.validate(&format!("network.deny[{idx}]"))?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Default)]
pub struct NetworkPolicyBuilder {
    policy: NetworkPolicy,
}

impl NetworkPolicyBuilder {
    pub fn default_action(mut self, action: NetworkAction) -> Self {
        self.policy.default = action;
        self
    }

    pub fn allow_host(mut self, host: impl Into<String>) -> Self {
        self.policy.allow.push(NetworkEntry::new(host));
        self
    }

    pub fn allow_host_port(mut self, host: impl Into<String>, port: u16) -> Self {
        self.policy.allow.push(NetworkEntry::with_port(host, port));
        self
    }

    pub fn deny_host(mut self, host: impl Into<String>) -> Self {
        self.policy.deny.push(NetworkEntry::new(host));
        self
    }

    pub fn deny_host_port(mut self, host: impl Into<String>, port: u16) -> Self {
        self.policy.deny.push(NetworkEntry::with_port(host, port));
        self
    }

    pub fn build(self) -> Result<NetworkPolicy> {
        self.policy
            .validate(true)
            .map_err(OutrigError::Configuration)?;
        Ok(self.policy)
    }
}

/// The `[network]` block: the interception mode, and -- in the global config
/// only -- the filter policy.
///
/// Every field is optional and private, so `None` (or an empty list) *is* "the
/// config did not declare this key" rather than a value that happens to match
/// the built-in default. [`merge`](fn@merge) turns on exactly that
/// distinction: it reads [`declared_mode`](Self::declared_mode) from the repo
/// side and nothing else, so a repo can choose the mode and cannot reach the
/// operator's policy. Read the effective values with [`mode`](Self::mode) and
/// [`policy`](Self::policy).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
#[non_exhaustive]
pub struct NetworkConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    mode: Option<NetworkMode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    default: Option<NetworkAction>,
    #[serde(skip_serializing_if = "Option::is_none")]
    allow: Option<Vec<NetworkEntry>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    deny: Option<Vec<NetworkEntry>>,
}

impl NetworkConfig {
    /// The declared `mode`, or [`NetworkMode::Default`].
    pub fn mode(&self) -> NetworkMode {
        self.mode.unwrap_or_default()
    }

    /// `mode` exactly as declared, or `None` when the config was silent -- the
    /// state [`merge`](fn@merge) acts on, as opposed to the value it
    /// resolves to. A `[network]` table that declares no `mode` is `None`, so
    /// it inherits rather than resetting an inherited mode to `default`.
    pub fn declared_mode(&self) -> Option<NetworkMode> {
        self.mode
    }

    /// Declare `mode`.
    pub fn set_mode(&mut self, mode: NetworkMode) {
        self.mode = Some(mode);
    }

    /// The effective filter policy: whatever was declared, with an undeclared
    /// `default` reading as [`NetworkAction::Deny`].
    pub fn policy(&self) -> NetworkPolicy {
        NetworkPolicy {
            default: self.default.unwrap_or_default(),
            allow: self.allow.clone().unwrap_or_default(),
            deny: self.deny.clone().unwrap_or_default(),
        }
    }

    /// Declare all three policy keys at once. The counterpart to
    /// [`policy`](Self::policy), and the only way to give a `Config` a filter
    /// policy now that the fields are private.
    pub fn set_policy(&mut self, policy: NetworkPolicy) {
        self.default = Some(policy.default);
        self.allow = Some(policy.allow);
        self.deny = Some(policy.deny);
    }

    /// Whether any `allow` or `deny` entry exists -- the question
    /// `mode = "filter"` asks, since a filter with no entries cannot decide
    /// anything. Distinct from `declared_policy_key`, which counts a declared
    /// `default` and counts an empty `allow = []` too, because the question
    /// *there* is whether a config touched the operator's policy at all.
    pub fn has_policy_entries(&self) -> bool {
        [&self.allow, &self.deny]
            .into_iter()
            .any(|list| list.as_ref().is_some_and(|l| !l.is_empty()))
    }

    /// The first policy key this config declared, in schema order. `None` when
    /// it declared no policy at all -- the predicate behind the
    /// global-versus-repo trust rule, applied by
    /// [`Config::validate_as_repo`].
    pub(super) fn declared_policy_key(&self) -> Option<&'static str> {
        if self.default.is_some() {
            Some("default")
        } else if self.allow.is_some() {
            Some("allow")
        } else if self.deny.is_some() {
            Some("deny")
        } else {
            None
        }
    }

    /// Apply the keys a repo config is allowed to choose. `self` is the
    /// operator's global block, and the policy keys are never read from
    /// `repo`, so a repo value carrying its own policy cannot reach the
    /// merged result whatever built it.
    ///
    /// The direction is the opposite of
    /// [`Workspace::inherit_missing_primary_fields`]: the global side is the
    /// base rather than the fallback, because that asymmetry *is* the trust
    /// rule.
    fn apply_repo_overrides(&mut self, repo: &Self) {
        if let Some(mode) = repo.mode {
            self.mode = Some(mode);
        }
    }

    fn is_default(&self) -> bool {
        self == &Self::default()
    }
}

/// Whether `outrig run-new` records what its agent did, in
/// `<session_dir>/logs/events.jsonl`.
///
/// The mode governs the file, not the session's events: a program running a
/// session through [`crate::harness`] receives them through its own
/// subscriptions whatever this says, and records them only if it asks
/// ([`SessionBuilder::record_events`](crate::harness::SessionBuilder::record_events)).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum EventsMode {
    /// Record nothing, and write no file.
    #[default]
    Off,
    /// Record every event the agent produces.
    Record,
}

/// The `[events]` block: whether a session records its agent's events.
///
/// The field is private and `None` when the config did not declare it, as
/// [`NetworkConfig`]'s `mode` is: a global config that declares it decides,
/// and a repo's applies only when the global one is silent. Read the effective
/// value with [`mode`](Self::mode).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
#[non_exhaustive]
pub struct EventsConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    mode: Option<EventsMode>,
}

impl EventsConfig {
    /// The declared `mode`, or [`EventsMode::Off`].
    pub fn mode(&self) -> EventsMode {
        self.mode.unwrap_or_default()
    }

    /// Declare `mode`, as a config file's `[events] mode` does.
    pub fn set_mode(&mut self, mode: EventsMode) {
        self.mode = Some(mode);
    }

    /// The operator's declared `mode` stands; a repo's applies only when the
    /// global config declares none. The record is the user's own of their own
    /// session, so a cloned project can neither turn it on nor off.
    fn inherit_from_repo(&mut self, repo: &Self) {
        self.mode = self.mode.or(repo.mode);
    }

    fn is_default(&self) -> bool {
        self == &Self::default()
    }
}

/// The `[host]` block: how a program `outrig run <script.py>` runs on the host is treated.
///
/// `grace-secs` is how long the program has to end on its own after the first `SIGINT` or
/// `SIGTERM` forwarded to it. Past it, or at a second signal, the engine stops the program's
/// process group -- `SIGTERM`, five seconds, then `SIGKILL` -- and shuts down every session it
/// left open. Time the engine spends shutting down a session the program asked it to shut down
/// does not count: the grace bounds the program's own response, not the shutdown. `0` stops the
/// group at the first signal. A repo's value replaces the global one, as the top-level keys do.
///
/// The field is private and `None` when the config did not declare it; read the effective value
/// with [`grace_secs`](Self::grace_secs).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
#[non_exhaustive]
pub struct HostConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    grace_secs: Option<u64>,
}

impl HostConfig {
    /// The declared `grace-secs`, or [`DEFAULT_HOST_GRACE_SECS`].
    pub fn grace_secs(&self) -> u64 {
        self.grace_secs.unwrap_or(DEFAULT_HOST_GRACE_SECS)
    }

    /// Declare `grace-secs`, as a config file's `[host] grace-secs` does.
    pub fn set_grace_secs(&mut self, secs: u64) {
        self.grace_secs = Some(secs);
    }

    /// The declared value, for validation to name.
    pub(crate) fn declared_grace_secs(&self) -> Option<u64> {
        self.grace_secs
    }

    /// A repo's declaration wins, as the top-level keys' do; otherwise the global one stands.
    fn inherit_from_global(&mut self, global: &Self) {
        self.grace_secs = self.grace_secs.or(global.grace_secs);
    }

    fn is_default(&self) -> bool {
        self == &Self::default()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum NetworkHostPattern {
    Ip(IpAddr),
    Cidr {
        base: IpAddr,
        prefix: u8,
    },
    /// A glob over hostnames, so it needs a name to match against.
    HostGlob(String),
    /// A glob with no letter in it (`*`, `10.0.*`): it describes addresses
    /// rather than names, so the interceptor matches it against the
    /// destination address itself.
    AddressGlob(String),
}

impl NetworkHostPattern {
    /// Whether this pattern matches every address of the family it describes:
    /// a `*` glob, or a CIDR with no prefix. The one kind of address entry that
    /// could grant a connection to any name, since any name can resolve to an
    /// address of that family.
    pub(crate) fn covers_every_address(&self) -> bool {
        match self {
            Self::Cidr { prefix, .. } => *prefix == 0,
            Self::AddressGlob(pattern) => pattern.chars().all(|c| c == '*'),
            Self::Ip(_) | Self::HostGlob(_) => false,
        }
    }
}

pub(crate) fn parse_network_host_pattern(
    host: &str,
) -> std::result::Result<NetworkHostPattern, String> {
    if host.is_empty() {
        return Err("must not be empty".to_string());
    }
    if host.chars().any(char::is_whitespace) {
        return Err("must not contain whitespace".to_string());
    }
    if let Ok(ip) = host.parse::<IpAddr>() {
        return Ok(NetworkHostPattern::Ip(ip));
    }
    if let Some((raw_ip, raw_prefix)) = host.split_once('/') {
        let ip = raw_ip
            .parse::<IpAddr>()
            .map_err(|_| "has an invalid CIDR address".to_string())?;
        let prefix = raw_prefix
            .parse::<u8>()
            .map_err(|_| "has an invalid CIDR prefix".to_string())?;
        let max = if ip.is_ipv4() { 32 } else { 128 };
        if prefix > max {
            return Err(format!(
                "has CIDR prefix {prefix}, maximum for this address is {max}"
            ));
        }
        return Ok(NetworkHostPattern::Cidr { base: ip, prefix });
    }
    if host.contains('/') || host.contains(':') {
        return Err("must be a hostname glob, IP address, or CIDR".to_string());
    }
    if host.starts_with('.') || host.ends_with('.') || host.contains("..") {
        return Err("has an invalid hostname glob".to_string());
    }
    if !host
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.' || c == '*')
    {
        return Err("has an invalid hostname glob".to_string());
    }
    for label in host.split('.') {
        if label.is_empty() {
            return Err("has an invalid hostname glob".to_string());
        }
        if label != "*" && !label.contains('*') && (label.starts_with('-') || label.ends_with('-'))
        {
            return Err("has an invalid hostname glob".to_string());
        }
    }
    let glob = host.to_ascii_lowercase();
    if glob.chars().any(|c| c.is_ascii_alphabetic()) {
        Ok(NetworkHostPattern::HostGlob(glob))
    } else {
        Ok(NetworkHostPattern::AddressGlob(glob))
    }
}

/// The `[repo]` block, in the global config only: what a repo config file may
/// take from this machine. The one key today is `env-allow`, the host variables
/// a `${VAR}` reference in a repo config's `build-args` or MCP `env` may name;
/// none by default. A variable on the list is the repo's to route: the repo
/// picks which build-arg or server environment receives the value.
///
/// Only the operator's file may declare it. A repo file that declares
/// `env-allow`, even empty, fails [`Config::validate_as_repo`], and
/// [`merge`](fn@merge) reads it from the global side alone;
/// [`Config::validate`] holds every image a repo file declares to it. The field
/// is private, like [`NetworkConfig`]'s, so `None` is "not declared" rather than
/// an empty list: read what is in effect with [`env_allow`](Self::env_allow).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
#[non_exhaustive]
pub struct RepoPolicy {
    #[serde(
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_env_allow"
    )]
    env_allow: Option<Vec<String>>,
}

impl RepoPolicy {
    /// The variables a repo config may reference: what was declared, and
    /// nothing when the config declared none, which allows no reference.
    pub fn env_allow(&self) -> &[String] {
        self.env_allow.as_deref().unwrap_or_default()
    }

    /// Declare the variables a repo config may reference. An entry that is not
    /// a variable name (`^[A-Z_][A-Z0-9_]*$`) allows nothing, since no reference
    /// can match it.
    pub fn set_env_allow(&mut self, vars: impl IntoIterator<Item = impl Into<String>>) {
        self.env_allow = Some(vars.into_iter().map(Into::into).collect());
    }

    fn is_default(&self) -> bool {
        self == &Self::default()
    }
}

/// Read `env-allow`, holding each entry to the grammar of a `${VAR}` reference.
/// An entry no reference could match -- lower case, wrapped in `${...}`, a glob
/// -- would otherwise allow nothing without saying so. The global config reads
/// leniently, so there the error sets the whole key aside with a warning, which
/// leaves the list empty: every reference is refused, never one more allowed.
fn deserialize_env_allow<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Option<Vec<String>>, D::Error> {
    let names = Vec::<String>::deserialize(deserializer)?;
    match names.iter().find(|name| !env_ref::is_env_var_name(name)) {
        Some(name) => Err(serde::de::Error::custom(format!(
            "env-allow entry {name:?} is not a variable name (must match ^[A-Z_][A-Z0-9_]*$)"
        ))),
        None => Ok(Some(names)),
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
#[non_exhaustive]
pub struct ContainerSecurity {
    pub capability_profile: CapabilityProfile,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cap_drop: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cap_add: Vec<String>,
    /// Whether to apply `--security-opt=no-new-privileges`. Setting this to
    /// false restores setuid escalation inside the container, so any
    /// setuid-root binary in the image becomes usable again.
    pub no_new_privileges: bool,
    /// Host device nodes to pass through, one `--device=<path>` each.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub devices: Vec<String>,
    /// Paths to exclude from podman's default masking, one
    /// `--security-opt=unmask=<path>` each. A nested container runtime needs
    /// `/proc/*` here; see `doc/concepts/containers.md`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unmask: Vec<String>,
}

/// Hand-written rather than derived: `no_new_privileges` must default to
/// `true`, and `bool::default()` is `false`. The container-level
/// `#[serde(default)]` fills omitted keys from here, so an absent
/// `no-new-privileges` deserializes to the safe value.
impl Default for ContainerSecurity {
    fn default() -> Self {
        Self {
            capability_profile: CapabilityProfile::default(),
            cap_drop: Vec::new(),
            cap_add: Vec::new(),
            no_new_privileges: true,
            devices: Vec::new(),
            unmask: Vec::new(),
        }
    }
}

impl ContainerSecurity {
    fn is_default(&self) -> bool {
        self == &Self::default()
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum CapabilityProfile {
    #[default]
    Default,
    NoNetRaw,
    DropAll,
}

pub(crate) fn capability_name_without_prefix(name: &str) -> &str {
    name.strip_prefix("CAP_").unwrap_or(name)
}

/// The name `name` stands for, or `None` if it is not one. A second `CAP_` is
/// not a name: podman rejects `CAP_CAP_X`, but once one prefix is stripped on
/// the way to it the rest reads as `CAP_X`, which it takes for `X`.
pub(crate) fn normalize_capability_name(name: &str) -> Option<String> {
    let name = capability_name_without_prefix(name);
    if name.is_empty() || name.starts_with("CAP_") {
        return None;
    }
    if name
        .chars()
        .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
    {
        Some(name.to_string())
    } else {
        None
    }
}

/// One `[images.<name>]` entry: either a build source (`dockerfile` plus
/// `context`) or a pull source (`image_name`), never both.
///
/// No field is public. [`set_build_paths`](Self::set_build_paths) and
/// [`set_image_name`](Self::set_image_name) each replace the whole source
/// shape, so no sequence of calls leaves a config [`source`](Self::source)
/// rejects. Read the shape with [`image_name`](Self::image_name),
/// [`dockerfile`](Self::dockerfile) and [`context`](Self::context).
///
/// [`build_args`](Self::build_args), [`security`](Self::security) and
/// [`mcp`](Self::mcp) take no part in the shape, so each has a `_mut`
/// accessor for in-place edits.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
#[non_exhaustive]
pub struct ImageConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    image_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    dockerfile: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    context: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    build_args: BTreeMap<String, EnvValue>,
    #[serde(default, skip_serializing_if = "ContainerSecurity::is_default")]
    security: ContainerSecurity,
    /// Set at load time, never deserialized. See [`ConfigSource`].
    #[serde(skip)]
    #[schemars(skip)]
    source: Option<ConfigSource>,
    /// Whether a repo config file declared this entry; `false` for the global
    /// config's, and for one no file declared. Set at load time beside
    /// [`source`](Self::config_source) and never deserialized, but kept apart
    /// from it: [`set_build_paths`](Self::set_build_paths) and
    /// [`set_image_name`](Self::set_image_name) clear `source`, which only
    /// resolves relative paths, and say nothing about who wrote the `${VAR}`
    /// references in `build-args` and `mcp` that [`Config::validate`] holds to
    /// `[repo] env-allow`.
    #[serde(skip)]
    #[schemars(skip)]
    repo_declared: bool,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    mcp: BTreeMap<String, McpServerSpec>,
}

/// A named sidecar container declared under `[sidecars.<sc>]`. Sidecars host
/// MCP servers in their own container, lifecycle-coupled to the session's
/// primary container.
///
/// Declared once at the top level and referenced by name from any number of
/// image-configs, exactly like `[models.<n>]` or `[providers.<n>]`. A session
/// starts the sidecars its image-config's `[mcp]` entries name, and only
/// those -- declaring a block instantiates nothing on its own.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
#[non_exhaustive]
pub struct SidecarConfig {
    /// Image reference, resolved exactly like `--image`: a sibling
    /// `[images.<name>]` config name first, else a raw podman ref that must
    /// be present locally (`--pull=never` semantics).
    pub image: String,
    /// Positional arguments for the image's ENTRYPOINT, used only when this
    /// block is an entrypoint host (its one MCP entry carries no `command`).
    /// The same key exists on the MCP entry; setting both is a config error.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    #[serde(default)]
    pub workspace: SidecarWorkspaceAccess,
    /// Whether the sidecar runs against the primary container's filesystem
    /// view (`view = "primary"`) or its own image's (`"none"`, the default).
    /// `"primary"` requires an entrypoint-stdio host and is mutually
    /// exclusive with `workspace`.
    #[serde(default)]
    pub view: SidecarView,
    #[serde(default)]
    pub start: SidecarStart,
    #[serde(default)]
    pub on_failure: SidecarOnFailure,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mounts: Vec<MountConfig>,
    #[serde(default, skip_serializing_if = "ContainerSecurity::is_default")]
    pub security: ContainerSecurity,
}

impl SidecarConfig {
    /// A sidecar running `image`, the one field with no default. Everything
    /// else starts where the TOML defaults leave it: no workspace access, no
    /// extra mounts, automatic start, abort on failure.
    pub fn new(image: impl Into<String>) -> Self {
        Self {
            image: image.into(),
            args: Vec::new(),
            workspace: SidecarWorkspaceAccess::default(),
            view: SidecarView::default(),
            start: SidecarStart::default(),
            on_failure: SidecarOnFailure::default(),
            mounts: Vec::new(),
            security: ContainerSecurity::default(),
        }
    }
}

/// How much of the session workspace a sidecar sees. Default: nothing.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum SidecarWorkspaceAccess {
    #[default]
    None,
    Ro,
    Rw,
}

impl SidecarWorkspaceAccess {
    /// The bind-mount access this level implies; `None` means no mount.
    pub fn mount_access(self) -> Option<MountAccess> {
        match self {
            Self::None => None,
            Self::Ro => Some(MountAccess::ReadOnly),
            Self::Rw => Some(MountAccess::ReadWrite),
        }
    }
}

/// Whether a sidecar sees the primary container's filesystem view. Default:
/// its own image's filesystem (`None`). `Primary` runs the sidecar's payload
/// inside the primary's mount namespace via the `outrig-enter` launcher, so a
/// third-party MCP image sees the primary's rootfs and paths without the
/// primary image carrying that tool.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum SidecarView {
    #[default]
    None,
    Primary,
}

impl SidecarView {
    /// Whether this is the default (`None`) view -- used to elide the key from
    /// serialization so entries without it stay byte-identical.
    pub fn is_none(&self) -> bool {
        matches!(self, Self::None)
    }

    /// The kebab-case name this view is written as in TOML. Lives here rather
    /// than at the rendering site so a new view is a compile error in the one
    /// crate that defines it, instead of a silently-dropped key downstream.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Primary => "primary",
        }
    }
}

/// Whether a sidecar starts with the session or waits for an explicit
/// `/sidecar add` / library call.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum SidecarStart {
    #[default]
    Auto,
    Manual,
}

/// How a sidecar's start/bootstrap/connect failure is handled at session
/// start. Mid-session death is uniform (log, tools error, no restart).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum SidecarOnFailure {
    #[default]
    Abort,
    Warn,
}

/// Discriminated view of the container source -- build-from-Dockerfile or
/// use-existing-image. Returned by [`ImageConfig::source`].
#[non_exhaustive]
pub enum ImageSourceRef<'a> {
    #[non_exhaustive]
    Build {
        dockerfile: &'a Path,
        context: &'a Path,
        build_args: &'a BTreeMap<String, EnvValue>,
    },
    #[non_exhaustive]
    Image { image_name: &'a str },
}

impl ImageConfig {
    /// Build-source config: a `Dockerfile` plus the build context it resolves
    /// against. The counterpart of [`ImageSourceRef::Build`].
    pub fn from_dockerfile(dockerfile: impl Into<PathBuf>, context: impl Into<PathBuf>) -> Self {
        Self {
            dockerfile: Some(dockerfile.into()),
            context: Some(context.into()),
            ..Self::sourceless()
        }
    }

    /// Pull-source config naming an existing image. The counterpart of
    /// [`ImageSourceRef::Image`].
    pub fn from_image_name(image_name: impl Into<String>) -> Self {
        Self {
            image_name: Some(image_name.into()),
            ..Self::sourceless()
        }
    }

    /// Every field at its serde default, which means *neither* source shape is
    /// set. Deliberately private, and deliberately not a `Default` impl: a
    /// sourceless config is exactly the state [`source`](Self::source) panics
    /// on, so it is a base for the two constructors above rather than a value
    /// worth handing out.
    fn sourceless() -> Self {
        Self {
            image_name: None,
            dockerfile: None,
            context: None,
            build_args: BTreeMap::new(),
            security: ContainerSecurity::default(),
            source: None,
            repo_declared: false,
            mcp: BTreeMap::new(),
        }
    }

    /// The image reference of a pull-source config, or `None` on a build-source
    /// one.
    pub fn image_name(&self) -> Option<&str> {
        self.image_name.as_deref()
    }

    /// `dockerfile` exactly as declared, or `None` on a pull-source config.
    /// Relative values are resolved by
    /// [`resolved_build_paths`](Self::resolved_build_paths).
    pub fn dockerfile(&self) -> Option<&Path> {
        self.dockerfile.as_deref()
    }

    /// `context` exactly as declared, or `None` on a pull-source config. See
    /// [`dockerfile`](Self::dockerfile).
    pub fn context(&self) -> Option<&Path> {
        self.context.as_deref()
    }

    /// Make this a build-source config: declare both build paths, clear
    /// `image_name`, and drop any recorded [`ConfigSource`]. The new values did
    /// not come from a config file, so they resolve against the `repo_root`
    /// passed to [`resolved_build_paths`](Self::resolved_build_paths).
    ///
    /// The source shape is replaced whole, so a pull-source config comes out as
    /// a build source rather than as both. The pair is written together because
    /// one source backs both: a setter for one path alone would clear the
    /// source its untouched sibling still depends on. `build_args` is left as
    /// it was, and so is whether a repo config file declared the entry.
    pub fn set_build_paths(&mut self, dockerfile: impl Into<PathBuf>, context: impl Into<PathBuf>) {
        self.image_name = None;
        self.dockerfile = Some(dockerfile.into());
        self.context = Some(context.into());
        self.source = None;
    }

    /// Make this a pull-source config: name the image, clear both build paths,
    /// and drop any recorded [`ConfigSource`], which existed only to resolve
    /// those paths. The counterpart of [`set_build_paths`](Self::set_build_paths)
    /// and the in-place form of [`from_image_name`](Self::from_image_name).
    ///
    /// `build_args` is left as it was, so a build-source config that declared
    /// any still fails validation with
    /// [`ConfigValidationError::ImageNameWithBuildArgs`] until they are
    /// cleared. Whether a repo config file declared the entry is left as it was
    /// too.
    pub fn set_image_name(&mut self, image_name: impl Into<String>) {
        self.image_name = Some(image_name.into());
        self.dockerfile = None;
        self.context = None;
        self.source = None;
    }

    /// The `--build-arg` values a build-source config passes, by name. Empty on
    /// a pull-source config, where a non-empty map is a validation error.
    pub fn build_args(&self) -> &BTreeMap<String, EnvValue> {
        &self.build_args
    }

    /// Edit [`build_args`](Self::build_args) in place. The map has no bearing
    /// on which source shape this config has.
    pub fn build_args_mut(&mut self) -> &mut BTreeMap<String, EnvValue> {
        &mut self.build_args
    }

    /// The container security settings sessions on this image start with.
    pub fn security(&self) -> &ContainerSecurity {
        &self.security
    }

    /// Edit [`security`](Self::security) in place.
    pub fn security_mut(&mut self) -> &mut ContainerSecurity {
        &mut self.security
    }

    /// The MCP servers this image declares, by name.
    pub fn mcp(&self) -> &BTreeMap<String, McpServerSpec> {
        &self.mcp
    }

    /// Edit [`mcp`](Self::mcp) in place.
    pub fn mcp_mut(&mut self) -> &mut BTreeMap<String, McpServerSpec> {
        &mut self.mcp
    }

    /// The config file this image-config was declared in, or `None` for a
    /// hand-built entry that never went through [`Config::load`].
    ///
    /// Distinct from [`source`](Self::source), which discriminates the *shape*
    /// of the container source rather than naming a file.
    pub fn config_source(&self) -> Option<&ConfigSource> {
        self.source.as_ref()
    }

    /// Record the file this entry was read from. Anything but the global config
    /// is a repo config.
    pub(crate) fn set_config_source(&mut self, source: ConfigSource) {
        self.repo_declared = !matches!(source, ConfigSource::Global { .. });
        self.source = Some(source);
    }

    /// Directory that `dockerfile` and `context` resolve against. `repo_root`
    /// is the fallback for an entry with no recorded source, which keeps every
    /// hand-built [`ImageConfig`] resolving exactly as it did before.
    pub fn base_dir<'a>(&'a self, repo_root: &'a Path) -> &'a Path {
        source_base_dir(self.source.as_ref(), repo_root)
    }

    /// `dockerfile` and `context` made absolute against
    /// [`base_dir`](Self::base_dir), or under the home directory for a path
    /// whose first component is `~`. Like [`source`](Self::source), this is
    /// only callable once validation has established the build shape.
    pub fn resolved_build_paths(&self, repo_root: &Path) -> (PathBuf, PathBuf) {
        let base = self.base_dir(repo_root);
        let dockerfile = self.dockerfile.as_ref().expect("build path validated");
        let context = self.context.as_ref().expect("build path validated");
        (
            resolve_against(base, dockerfile),
            resolve_against(base, context),
        )
    }

    /// The file to name in a diagnostic about one of this entry's paths, or
    /// `None` for a hand-built entry. Deliberately not defaulted to the repo
    /// config: unlike a base directory, a filename in an error message is a
    /// *claim*, and naming a file that never mentioned this image would be a
    /// fabrication.
    pub(crate) fn declared_in(&self) -> Option<PathBuf> {
        self.source.as_ref().map(ConfigSource::config_path)
    }

    /// Return the discriminated source variant. Panics unless exactly one of
    /// `image_name` or `dockerfile` plus `context` is set, which
    /// [`Config::validate`] establishes and the setters preserve. A value
    /// deserialized without validation, such as an `[images.<name>]` table
    /// that sets neither, can still reach the panic.
    pub fn source(&self) -> ImageSourceRef<'_> {
        match (&self.image_name, &self.dockerfile, &self.context) {
            (Some(name), None, None) => ImageSourceRef::Image { image_name: name },
            (None, Some(df), Some(ctx)) => ImageSourceRef::Build {
                dockerfile: df,
                context: ctx,
                build_args: &self.build_args,
            },
            _ => panic!(
                "ImageConfig::source() called on an unvalidated config; \
                 call Config::validate() first"
            ),
        }
    }
}

/// `Deserialize` is written by hand below; these serde attributes drive
/// `Serialize` and the JSON schema.
#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
#[serde(untagged, deny_unknown_fields)]
#[non_exhaustive]
pub enum McpServerSpec {
    Short(Vec<String>),
    #[non_exhaustive]
    Full {
        /// Argv to exec. Optional so the entrypoint-stdio form (a placed
        /// entry with no command) parses; validation guarantees every exec
        /// path sees a non-empty command.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        command: Option<Vec<String>>,
        /// Always serialized, even empty. It always has been, and the
        /// `org.outrig.mcp` label it lands in is part of an image's cache
        /// key, so eliding it now would rebuild every image stamped since.
        #[serde(default)]
        env: BTreeMap<String, EnvValue>,
        /// The sidecar declared under `[sidecars.<sc>]` that hosts this
        /// server. Naming it here is what starts that container for the
        /// session. With `command` present: exec-stdio in it. Without:
        /// entrypoint-stdio, and the block becomes an entrypoint host.
        /// Mutually exclusive with `image`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sidecar: Option<String>,
        /// A dedicated anonymous sidecar for this one server. With `command`
        /// present: exec-stdio in that sidecar. Without: entrypoint-stdio
        /// (the image's ENTRYPOINT is the server). Mutually exclusive with
        /// `sidecar`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        image: Option<String>,
        /// Positional arguments for an entrypoint-stdio server, appended
        /// after the image ref on `podman create`. Entrypoint-stdio only:
        /// exec-stdio already carries a full argv in `command`. Elided when
        /// empty, so an entry without it serializes exactly as before.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        args: Vec<String>,
        /// Filesystem view for the inline anonymous entrypoint-stdio form
        /// (`image` set, no `command`): `"primary"` runs the image against the
        /// primary container's view. Elided when default, so an entry without
        /// it serializes exactly as before.
        #[serde(default, skip_serializing_if = "SidecarView::is_none")]
        view: SidecarView,
        /// This server's `tools/call` deadline in seconds, overriding the
        /// top-level `mcp-call-timeout-secs`. Last and elided when unset, so
        /// an entry without it serializes -- into a label too -- exactly as
        /// before. See [`DEFAULT_MCP_CALL_TIMEOUT_SECS`].
        #[serde(
            default,
            rename = "call-timeout-secs",
            skip_serializing_if = "Option::is_none"
        )]
        call_timeout_secs: Option<u64>,
    },
}

/// Dispatches on the value's own type -- an array is `Short`, a table `Full`
/// -- rather than through `#[serde(untagged)]`. Untagged tries each variant
/// and, when none fits, reports only that none did; a table read directly
/// names the key it rejects, ``unknown field `comand` ``, as every other
/// table in the config does.
impl<'de> Deserialize<'de> for McpServerSpec {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields, rename_all = "kebab-case")]
        struct Table {
            command: Option<Vec<String>>,
            #[serde(default)]
            env: BTreeMap<String, EnvValue>,
            sidecar: Option<String>,
            image: Option<String>,
            #[serde(default)]
            args: Vec<String>,
            #[serde(default)]
            view: SidecarView,
            call_timeout_secs: Option<u64>,
        }

        struct SpecVisitor;

        impl<'de> Visitor<'de> for SpecVisitor {
            type Value = McpServerSpec;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("an argv array or an MCP server table")
            }

            fn visit_seq<A>(self, seq: A) -> std::result::Result<Self::Value, A::Error>
            where
                A: SeqAccess<'de>,
            {
                Vec::deserialize(SeqAccessDeserializer::new(seq)).map(McpServerSpec::Short)
            }

            fn visit_map<A>(self, map: A) -> std::result::Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                let t = Table::deserialize(MapAccessDeserializer::new(map))?;
                Ok(McpServerSpec::Full {
                    command: t.command,
                    env: t.env,
                    sidecar: t.sidecar,
                    image: t.image,
                    args: t.args,
                    view: t.view,
                    call_timeout_secs: t.call_timeout_secs,
                })
            }
        }

        deserializer.deserialize_any(SpecVisitor)
    }
}

impl McpServerSpec {
    /// Exec-stdio server: `command` is spawned with `podman exec -i` inside
    /// whichever container hosts it (the primary by default; a sidecar once
    /// [`with_sidecar`](Self::with_sidecar) names one).
    pub fn exec(command: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self::full(Some(command.into_iter().map(Into::into).collect()), None)
    }

    /// Entrypoint-stdio server in a dedicated anonymous sidecar built from
    /// `image`: the image's own ENTRYPOINT is the server, so there is no
    /// command to exec. For the same transport in a sidecar declared under
    /// `[sidecars.<sc>]`, see
    /// [`entrypoint_in_sidecar`](Self::entrypoint_in_sidecar).
    pub fn entrypoint(image: impl Into<String>) -> Self {
        Self::full(None, Some(image.into()))
    }

    /// Entrypoint-stdio server hosted by the sidecar declared under
    /// `[sidecars.<sc>]`: that container's own ENTRYPOINT is the server, so
    /// there is no command to exec. The named-block counterpart of
    /// [`entrypoint`](Self::entrypoint), which gives the server a dedicated
    /// anonymous sidecar instead.
    pub fn entrypoint_in_sidecar(sidecar: impl Into<String>) -> Self {
        Self::full(None, None).with_sidecar(sidecar)
    }

    /// Environment for the server process, resolved at spawn time.
    pub fn with_env(self, env: BTreeMap<String, EnvValue>) -> Self {
        self.map_full(|spec| {
            if let Self::Full { env: slot, .. } = spec {
                *slot = env;
            }
        })
    }

    /// Host this server in the sidecar declared under `[sidecars.<sc>]`.
    /// Mutually exclusive with the anonymous [`entrypoint`](Self::entrypoint)
    /// form; validation rejects setting both. Pairs with
    /// [`exec`](Self::exec) to run a command in that container; the
    /// entrypoint-stdio shape is
    /// [`entrypoint_in_sidecar`](Self::entrypoint_in_sidecar) rather than a
    /// commandless spec built here.
    pub fn with_sidecar(self, sidecar: impl Into<String>) -> Self {
        self.map_full(|spec| {
            if let Self::Full { sidecar: slot, .. } = spec {
                *slot = Some(sidecar.into());
            }
        })
    }

    /// Positional arguments appended after the image ref for an
    /// entrypoint-stdio server. Ignored by the exec-stdio form, which carries
    /// a full argv already.
    pub fn with_args(self, args: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.map_full(|spec| {
            if let Self::Full { args: slot, .. } = spec {
                *slot = args.into_iter().map(Into::into).collect();
            }
        })
    }

    /// Filesystem view for the anonymous entrypoint-stdio form.
    pub fn with_view(self, view: SidecarView) -> Self {
        self.map_full(|spec| {
            if let Self::Full { view: slot, .. } = spec {
                *slot = view;
            }
        })
    }

    /// This server's own `tools/call` deadline in seconds, replacing whatever
    /// the session default would have given it.
    pub fn with_call_timeout_secs(self, secs: u64) -> Self {
        self.map_full(|spec| {
            if let Self::Full {
                call_timeout_secs: slot,
                ..
            } = spec
            {
                *slot = Some(secs);
            }
        })
    }

    /// The one `Full` literal in this impl: the two constructors and the
    /// `Short` promotion below all route through it, so a field added to the
    /// variant is filled in exactly one place.
    fn full(command: Option<Vec<String>>, image: Option<String>) -> Self {
        Self::Full {
            command,
            env: BTreeMap::new(),
            sidecar: None,
            image,
            args: Vec::new(),
            view: SidecarView::None,
            call_timeout_secs: None,
        }
    }

    /// Promote `Short` to the equivalent `Full` and apply `set`. The
    /// promotion is what keeps the `with_*` setters total: `Short` carries an
    /// argv and nothing else, so it is exactly `Full { command, .. }`. The
    /// match is spelled out rather than using a catch-all so that a third
    /// shape is a compile error here instead of a silently-ignored setter.
    fn map_full(self, set: impl FnOnce(&mut Self)) -> Self {
        let mut spec = match self {
            Self::Short(command) => Self::full(Some(command), None),
            full @ Self::Full { .. } => full,
        };
        set(&mut spec);
        spec
    }

    /// Returns the argv and the (still-unresolved) env map. Resolution of any
    /// `EnvValue::EnvRef` entries happens at the call site that's about to
    /// spawn the MCP server, so a missing host env var is reported as an
    /// MCP-startup failure rather than a config-load failure.
    pub fn normalize(&self) -> (Vec<String>, BTreeMap<String, EnvValue>) {
        (
            self.command().unwrap_or_default().to_vec(),
            self.env().clone(),
        )
    }

    /// The argv to exec, if this entry carries one. `None` is the
    /// entrypoint-stdio form, where the container's ENTRYPOINT is the server.
    pub fn command(&self) -> Option<&[String]> {
        match self {
            Self::Short(command) => Some(command),
            Self::Full { command, .. } => command.as_deref(),
        }
    }

    /// The declared (still-unresolved) environment. Always empty for `Short`.
    pub fn env(&self) -> &BTreeMap<String, EnvValue> {
        // `BTreeMap::new` is const, so the `Short` arm borrows a static empty
        // map rather than forcing the caller to handle an `Option`.
        static EMPTY: BTreeMap<String, EnvValue> = BTreeMap::new();
        match self {
            Self::Short(_) => &EMPTY,
            Self::Full { env, .. } => env,
        }
    }

    /// Named-sidecar placement, if any. Always `None` for `Short`.
    pub fn sidecar(&self) -> Option<&str> {
        match self {
            Self::Short(_) => None,
            Self::Full { sidecar, .. } => sidecar.as_deref(),
        }
    }

    /// Inline anonymous-sidecar image, if any. Always `None` for `Short`.
    pub fn image(&self) -> Option<&str> {
        match self {
            Self::Short(_) => None,
            Self::Full { image, .. } => image.as_deref(),
        }
    }

    /// Positional arguments for the entrypoint-stdio form. Always empty for
    /// `Short`, which is exec-stdio in the primary.
    pub fn args(&self) -> &[String] {
        match self {
            Self::Short(_) => &[],
            Self::Full { args, .. } => args,
        }
    }

    /// Filesystem view for the inline anonymous entrypoint-stdio form.
    /// Always `None` for `Short` and for `Full` without `view`.
    pub fn view(&self) -> SidecarView {
        match self {
            Self::Short(_) => SidecarView::None,
            Self::Full { view, .. } => *view,
        }
    }

    /// This server's own `tools/call` deadline in seconds, if it declares
    /// one. Always `None` for `Short`; the session default then applies.
    pub fn call_timeout_secs(&self) -> Option<u64> {
        match self {
            Self::Short(_) => None,
            Self::Full {
                call_timeout_secs, ..
            } => *call_timeout_secs,
        }
    }

    /// Whether the spec carries a command (`Short` always does).
    pub fn has_command(&self) -> bool {
        match self {
            Self::Short(_) => true,
            Self::Full { command, .. } => command.is_some(),
        }
    }

    /// Whether the entry names a container other than the primary -- an
    /// inline `image` or a named `sidecar`. The single definition of "carries
    /// a placement key", so a third placement would extend one predicate
    /// rather than every site that tests for them pairwise.
    pub fn is_placed(&self) -> bool {
        self.image().is_some() || self.sidecar().is_some()
    }

    /// Whether this entry is the entrypoint-stdio form: a placed entry
    /// (inline `image` or a named `sidecar`) with no `command`, meaning that
    /// container's ENTRYPOINT is the server. The single definition of the
    /// transport classification -- placement planning, MCP connection, and
    /// the library facade's rejection all dispatch on it.
    pub fn is_entrypoint_stdio(&self) -> bool {
        !self.has_command() && self.is_placed()
    }
}

/// Accepts a bare string *or* an array of them during deserialization,
/// normalizing to `Vec<String>`. `alias` uses it, where the two forms are the
/// two cases the feature exists for: one name for one model, and one name for
/// a set of equivalents.
///
/// It re-serializes as an array, so a round-trip rewrites `alias = "opus-5"`
/// as `alias = ["opus-5"]`. The parsed configs compare equal, which is what
/// the round-trip tests assert.
fn deserialize_string_or_vec_string<'de, D>(
    d: D,
) -> std::result::Result<Option<Vec<String>>, D::Error>
where
    D: Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum StringOrVec {
        Single(String),
        Multi(Vec<String>),
    }

    Option::<StringOrVec>::deserialize(d).map(|opt| {
        opt.map(|v| match v {
            StringOrVec::Single(s) => vec![s],
            StringOrVec::Multi(ss) => ss,
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each limit is the agent's own, else the top level's, else the default;
    /// the token budget has none.
    #[test]
    fn limits_read_the_agent_then_the_top_level_then_the_default() {
        let cfg = Config::load_from_str(
            "children-max = 10\nrequests-max = 20\ntoken-budget = 30\n\
             [agents.coding]\nchildren-max = 2\nsubagent-depth-max = 5\n",
        )
        .expect("parses");
        let expected = Limits {
            depth_max: 5,
            children_max: 2,
            model_concurrency_max: DEFAULT_MODEL_CONCURRENCY_MAX,
            requests_max: 20,
            token_budget: Some(30),
        };
        assert_eq!(cfg.limits(Some("coding")), expected);
        assert_eq!(
            cfg.limits(None),
            Limits {
                depth_max: DEFAULT_SUBAGENT_DEPTH_MAX,
                children_max: 10,
                ..expected
            }
        );
        assert_eq!(
            Config::default().limits(Some("absent")),
            Limits {
                depth_max: DEFAULT_SUBAGENT_DEPTH_MAX,
                children_max: DEFAULT_CHILDREN_MAX,
                model_concurrency_max: DEFAULT_MODEL_CONCURRENCY_MAX,
                requests_max: DEFAULT_REQUESTS_MAX,
                token_budget: None,
            }
        );
    }

    fn expand(path: &str) -> PathBuf {
        expand_tilde(Path::new(path), Some(Path::new("/home/you")))
    }

    /// Compared as strings: `PathBuf` equality ignores a trailing separator,
    /// which `home.join("")` would leave behind.
    #[test]
    fn a_bare_tilde_is_the_home_directory_itself() {
        assert_eq!(expand("~").as_os_str(), "/home/you");
        assert_eq!(expand("~/").as_os_str(), "/home/you");
    }

    #[test]
    fn a_leading_tilde_component_is_replaced_by_home() {
        assert_eq!(
            expand("~/.cache/example"),
            Path::new("/home/you/.cache/example")
        );
    }

    #[test]
    fn only_a_whole_leading_tilde_component_expands() {
        for path in ["~alice/src", "./~/src", "src/~", "/abs/~", "relative"] {
            assert_eq!(expand(path), Path::new(path), "{path:?}");
        }
    }

    #[test]
    fn without_an_absolute_home_a_tilde_is_an_ordinary_name() {
        for home in [None, Some(Path::new("")), Some(Path::new("relative/home"))] {
            assert_eq!(
                expand_tilde(Path::new("~/src"), home),
                Path::new("~/src"),
                "{home:?}",
            );
        }
    }

    /// `McpServerSpec` serializes through its derive and deserializes through
    /// a hand-written table struct, so every key has two spellings to agree.
    /// No field here is at its default, so none is elided on the way out.
    #[test]
    fn every_mcp_table_key_round_trips() {
        let spec = McpServerSpec::Full {
            command: Some(vec!["mcp-fs".to_string()]),
            env: BTreeMap::from([("TOKEN".to_string(), EnvValue::EnvRef("T".to_string()))]),
            sidecar: Some("tools".to_string()),
            image: Some("ghcr.io/example/fs:1".to_string()),
            args: vec!["/workspace".to_string()],
            view: SidecarView::Primary,
            call_timeout_secs: Some(30),
        };

        let json = serde_json::to_string(&spec).unwrap();
        assert_eq!(serde_json::from_str::<McpServerSpec>(&json).unwrap(), spec);
    }
}
