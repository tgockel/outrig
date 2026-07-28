//! Config schema, parsing, merge, and validation.

mod api_key;
mod env_ref;
mod env_value;
mod merge;
mod validate;

use std::collections::BTreeMap;
use std::fs;
use std::net::IpAddr;
use std::path::{Path, PathBuf};

use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize};

pub use api_key::{ApiKeyError, ApiKeyRef};
pub use env_value::{EnvValue, EnvValueError};
pub use merge::merge;
pub use validate::{ConfigValidationError, MountRuleViolation};
pub(crate) use validate::{
    check_entrypoint_hosting, check_sidecar_image, check_sidecar_name, check_view_exclusions,
    is_valid_mcp_server_name, mcp_command_is_empty,
};

use crate::error::{IoPathExt, OutrigError, Result};

/// True when the parse error is an "unknown field" complaint and its span
/// lands on a `[<dotted.path>]` header whose path has an unquoted `.`. That's
/// the shape that makes TOML treat a name like `opus-4.7` as nested keys
/// (`opus-4` table with field `7`) and is the cue to suggest quoting.
/// Restricting to unknown-field errors avoids hinting on legitimate dotted
/// headers like `[providers.openai]` whose values fail validation.
fn error_lands_on_unquoted_dotted_header(err: &toml::de::Error, input: &str) -> bool {
    if !err.message().contains("unknown field") {
        return false;
    }
    let Some(span) = err.span() else {
        return false;
    };
    let line_start = input[..span.start].rfind('\n').map_or(0, |i| i + 1);
    let line_end = input[span.start..]
        .find('\n')
        .map_or(input.len(), |i| span.start + i);
    let line = input[line_start..line_end].trim();
    let Some(rest) = line.strip_prefix('[') else {
        return false;
    };
    let Some(end) = rest.find(']') else {
        return false;
    };
    let mut in_quote = false;
    for c in rest[..end].chars() {
        match c {
            '"' => in_quote = !in_quote,
            '.' if !in_quote => return true,
            _ => {}
        }
    }
    false
}

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
    /// A standalone image project -- the directory holding an `image.toml`.
    Project { dir: PathBuf },
}

impl ConfigSource {
    /// Directory that this file's relative paths resolve against.
    pub fn base_dir(&self) -> &Path {
        match self {
            Self::Repo { root } => root,
            Self::Global { path } => path.parent().unwrap_or(Path::new("")),
            Self::Project { dir } => dir,
        }
    }

    /// The file that declared the entry, for diagnostics. A path reported
    /// without this reads as a repo problem even when it came from elsewhere.
    pub fn config_path(&self) -> PathBuf {
        match self {
            Self::Repo { root } => crate::repo::repo_config_path(root),
            Self::Global { path } => path.clone(),
            // The literal rather than a shared constant: the two sites that
            // actually read this file live in `outrig-cli`, which cannot see a
            // `pub(crate)` constant here, so a constant would centralize
            // nothing while looking like it did.
            Self::Project { dir } => dir.join("image.toml"),
        }
    }
}

/// Resolve `path` against `base`, leaving absolute paths alone. The one rule,
/// shared by every config-declared host path.
pub(crate) fn resolve_against(base: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_root: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_cache_root: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_max: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_result_max: Option<u32>,
    /// Maximum subagent nesting depth. See [`DEFAULT_SUBAGENT_DEPTH_MAX`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subagent_depth_max: Option<u32>,
    #[serde(default, skip_serializing_if = "NetworkConfig::is_default")]
    pub network: NetworkConfig,

    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub providers: BTreeMap<String, LlmProvider>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub models: BTreeMap<String, Model>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub agents: BTreeMap<String, Agent>,

    #[serde(default)]
    pub workspace: Workspace,

    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub images: BTreeMap<String, ImageConfig>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub sidecars: BTreeMap<String, SidecarConfig>,
}

impl Config {
    pub fn load_from_str(s: &str) -> Result<Self> {
        let mut cfg: Self = match toml::from_str(s) {
            Ok(c) => c,
            Err(e) if error_lands_on_unquoted_dotted_header(&e, s) => {
                return Err(OutrigError::ConfigDottedKey { source: e });
            }
            Err(e) => return Err(e.into()),
        };
        cfg.network.declared = declares_top_level_network(s)?;
        Ok(cfg)
    }

    /// Read repo + (optional) global config files, merge with repo precedence,
    /// and validate the merged result against `repo_root`. The repo config
    /// file is read from `<repo_root>/.agents/outrig/config.toml`.
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
        let agent_model_override =
            model_override.and(agent_flag.or(merged.default_agent.as_deref()));
        merged.validate_for_run(Some(repo_root), agent_model_override)?;
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

    fn load_unvalidated(repo_root: &Path, global_path: Option<&Path>) -> Result<Self> {
        let repo_path = crate::repo::repo_config_path(repo_root);
        // A missing repo config is not an error: `outrig run`/`outrig mcp` may
        // run in a directory with no `.agents/outrig/config.toml`, falling back
        // to the global config (and built-in defaults). Mirrors the global-file
        // handling below.
        let repo_text = match fs::read_to_string(&repo_path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(e).path_ctx("read", &repo_path),
        };
        let mut repo_cfg = Self::load_from_str(&repo_text)?;
        reject_repo_network_policy(&repo_text)?;
        repo_cfg.stamp_source(&ConfigSource::Repo {
            root: repo_root.to_path_buf(),
        });

        let global_cfg = match global_path {
            Some(g) => match fs::read_to_string(g) {
                Ok(text) => {
                    let mut cfg = Self::load_from_str(&text)?;
                    cfg.stamp_source(&ConfigSource::Global {
                        path: g.to_path_buf(),
                    });
                    cfg
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Self::default(),
                Err(e) => return Err(e).path_ctx("read", g),
            },
            None => Self::default(),
        };

        Ok(merge(global_cfg, repo_cfg))
    }

    /// Record `src` on every entry that carries a path, so the entry survives
    /// [`merge`] knowing which directory its relative paths mean. Must run
    /// before the merge: `extend` and the mount concatenation move whole
    /// entries, and nothing afterwards can tell the two files apart.
    ///
    /// Only images and mounts are stamped. Providers and agents have no path
    /// fields, so a base directory would buy them nothing. `models.<n>`
    /// does have one -- `model-path` -- and is left out on purpose: it is
    /// validated against a base the loader does not use, so giving it a
    /// *better* validation base would only widen the disagreement. Both halves
    /// get fixed together in `plan/next/model-path-runtime-unjoined.md`.
    fn stamp_source(&mut self, src: &ConfigSource) {
        for image in self.images.values_mut() {
            image.set_config_source(src.clone());
        }
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

    /// Validate every cross-reference rule documented in `doc/reference/config.md`.
    /// `repo_root: Some(_)` enables `dockerfile`/`context` on-disk existence checks;
    /// `None` keeps the check pure-structural for unit tests.
    pub fn validate(&self, repo_root: Option<&Path>) -> Result<()> {
        validate::validate(self, repo_root)?;
        Ok(())
    }

    /// Validate `[workspace.mounts]` -- including any appended at runtime, e.g.
    /// from `--volume` -- without re-running LLM/image checks: container paths
    /// absolute, not `/`, unique (against each other and the primary workspace
    /// mount), and (when `repo_root` is set) host paths existing directories.
    pub fn validate_workspace_mounts(&self, repo_root: Option<&Path>) -> Result<()> {
        validate::validate_workspace_mounts(self, repo_root)?;
        Ok(())
    }

    fn validate_for_run(
        &self,
        repo_root: Option<&Path>,
        agent_model_override: Option<&str>,
    ) -> Result<()> {
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

    fn validate_for_build(&self, repo_root: Option<&Path>) -> Result<()> {
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

fn declares_top_level_network(text: &str) -> Result<bool> {
    let value = text.parse::<toml_edit::DocumentMut>().map_err(|source| {
        crate::error::OutrigError::Configuration(format!("parsing config for [network]: {source}"))
    })?;
    Ok(value.as_table().contains_key("network"))
}

fn reject_repo_network_policy(text: &str) -> Result<()> {
    let value = text.parse::<toml_edit::DocumentMut>().map_err(|source| {
        crate::error::OutrigError::Configuration(format!("parsing config for [network]: {source}"))
    })?;
    let Some(network) = value.get("network").and_then(toml_edit::Item::as_table) else {
        return Ok(());
    };
    for key in ["default", "allow", "deny"] {
        if network.contains_key(key) {
            return Err(OutrigError::Configuration(format!(
                "repo config may set [network].mode only; [network].{key} belongs in global config"
            )));
        }
    }
    Ok(())
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
    },
    Mistralrs,
}

impl LlmProvider {
    /// An OpenAI-compatible provider at `base_url`. `request_timeout_secs`
    /// falls back to the client default when `None`.
    pub fn openai(
        base_url: impl Into<String>,
        api_key: ApiKeyRef,
        request_timeout_secs: Option<u64>,
    ) -> Self {
        Self::OpenAi {
            base_url: base_url.into(),
            api_key,
            request_timeout_secs,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
#[non_exhaustive]
pub struct Model {
    pub provider: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identifier: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_path: Option<PathBuf>,
    #[serde(
        default,
        deserialize_with = "deserialize_string_or_vec_string",
        skip_serializing_if = "Option::is_none"
    )]
    pub model_file: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_length: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device: Option<String>,
}

impl Model {
    /// A model served by the `[providers.<provider>]` entry of that name.
    /// Every other field is optional and stays unset; assign the ones the
    /// provider needs.
    pub fn new(provider: impl Into<String>) -> Self {
        Self {
            provider: provider.into(),
            identifier: None,
            model_id: None,
            model_path: None,
            model_file: None,
            revision: None,
            context_length: None,
            device: None,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum MistralrsDeviceSpec {
    #[default]
    Cpu,
    Cuda(usize),
    Metal,
}

impl MistralrsDeviceSpec {
    pub const EXPECTED: &'static str = "expected one of: cpu, cuda, cuda:N, metal";
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct MistralrsDeviceParseError;

impl std::fmt::Display for MistralrsDeviceParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(MistralrsDeviceSpec::EXPECTED)
    }
}

impl std::error::Error for MistralrsDeviceParseError {}

impl std::str::FromStr for MistralrsDeviceSpec {
    type Err = MistralrsDeviceParseError;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s {
            "cpu" => Ok(Self::Cpu),
            "cuda" => Ok(Self::Cuda(0)),
            "metal" => Ok(Self::Metal),
            _ => {
                let Some(ordinal) = s.strip_prefix("cuda:") else {
                    return Err(MistralrsDeviceParseError);
                };
                if ordinal.is_empty() || !ordinal.chars().all(|c| c.is_ascii_digit()) {
                    return Err(MistralrsDeviceParseError);
                }
                ordinal
                    .parse::<usize>()
                    .map(Self::Cuda)
                    .map_err(|_| MistralrsDeviceParseError)
            }
        }
    }
}

impl std::fmt::Display for MistralrsDeviceSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cpu => f.write_str("cpu"),
            Self::Cuda(0) => f.write_str("cuda"),
            Self::Cuda(ordinal) => write!(f, "cuda:{ordinal}"),
            Self::Metal => f.write_str("metal"),
        }
    }
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
}

impl Agent {
    /// Whether the parent-side `outrig__` subagent tools are registered for
    /// this agent. Defaults to enabled when the key is absent.
    pub fn subagents_enabled(&self) -> bool {
        self.subagents.unwrap_or(true)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
#[non_exhaustive]
pub struct Workspace {
    pub host_path: PathBuf,
    pub container_path: PathBuf,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mounts: Vec<MountConfig>,
}

impl Default for Workspace {
    fn default() -> Self {
        Self {
            host_path: PathBuf::from("."),
            container_path: PathBuf::from("/workspace"),
            mounts: Vec::new(),
        }
    }
}

impl Workspace {
    /// A workspace mapping `host_path` to `container_path`, with no extra
    /// mounts. [`Workspace::default`] is the `.` -> `/workspace` case.
    pub fn new(host_path: impl Into<PathBuf>, container_path: impl Into<PathBuf>) -> Self {
        Self {
            host_path: host_path.into(),
            container_path: container_path.into(),
            mounts: Vec::new(),
        }
    }

    /// `host_path` made absolute. Always resolves against `repo_root`: [`merge`]
    /// takes the repo's `[workspace]` block whole, so this key can only ever
    /// have come from the repo config (or its serde default).
    pub fn resolved_host_path(&self, repo_root: &Path) -> PathBuf {
        resolve_against(repo_root, &self.host_path)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
#[non_exhaustive]
pub struct MountConfig {
    pub host_path: PathBuf,
    pub container_path: PathBuf,
    #[serde(default)]
    pub access: MountAccess,
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
            container_path: container_path.into(),
            access,
            source: None,
        }
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
    /// declared it. `repo_root` is the fallback for an entry with no recorded
    /// source, which is every hand-built [`MountConfig`].
    ///
    /// Global and repo mount lists are *concatenated* by [`merge`], so one base
    /// directory provably cannot be right for every element of the result --
    /// this is per-entry for that reason.
    pub fn resolved_host_path(&self, repo_root: &Path) -> PathBuf {
        let base = self
            .source
            .as_ref()
            .map_or(repo_root, ConfigSource::base_dir);
        resolve_against(base, &self.host_path)
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

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
#[non_exhaustive]
pub struct NetworkConfig {
    pub mode: NetworkMode,
    #[serde(default, skip_serializing_if = "NetworkAction::is_deny")]
    pub default: NetworkAction,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allow: Vec<NetworkEntry>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deny: Vec<NetworkEntry>,
    #[serde(skip)]
    #[schemars(skip)]
    declared: bool,
}

impl Default for NetworkConfig {
    fn default() -> Self {
        Self {
            mode: NetworkMode::Default,
            default: NetworkAction::Deny,
            allow: Vec::new(),
            deny: Vec::new(),
            declared: false,
        }
    }
}

impl PartialEq for NetworkConfig {
    fn eq(&self, other: &Self) -> bool {
        self.mode == other.mode
            && self.default == other.default
            && self.allow == other.allow
            && self.deny == other.deny
    }
}

impl Eq for NetworkConfig {}

impl NetworkConfig {
    pub(crate) fn is_declared(&self) -> bool {
        self.declared
    }

    pub(crate) fn set_declared(&mut self, declared: bool) {
        self.declared = declared;
    }

    pub fn policy(&self) -> NetworkPolicy {
        NetworkPolicy {
            default: self.default,
            allow: self.allow.clone(),
            deny: self.deny.clone(),
        }
    }

    pub fn has_policy_entries(&self) -> bool {
        !self.allow.is_empty() || !self.deny.is_empty()
    }
}

impl NetworkConfig {
    fn is_default(&self) -> bool {
        self == &Self::default()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum NetworkHostPattern {
    Ip(IpAddr),
    Cidr { base: IpAddr, prefix: u8 },
    HostGlob(String),
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
    Ok(NetworkHostPattern::HostGlob(host.to_ascii_lowercase()))
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
    /// false restores setuid escalation inside the container, which a nested
    /// rootless container runtime needs so that `newuidmap` can map its
    /// subordinate UID range.
    pub no_new_privileges: bool,
    /// Host device nodes to pass through, one `--device=<path>` each.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub devices: Vec<String>,
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

pub(crate) fn normalize_capability_name(name: &str) -> Option<String> {
    let name = capability_name_without_prefix(name);
    if name.is_empty() {
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
#[non_exhaustive]
pub struct ImageConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dockerfile: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub build_args: BTreeMap<String, EnvValue>,
    #[serde(default, skip_serializing_if = "ContainerSecurity::is_default")]
    pub security: ContainerSecurity,
    /// Set at load time, never deserialized. See [`ConfigSource`].
    #[serde(skip)]
    #[schemars(skip)]
    source: Option<ConfigSource>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub mcp: BTreeMap<String, McpServerSpec>,
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
            mcp: BTreeMap::new(),
        }
    }

    /// The config file this image-config was declared in, or `None` for a
    /// hand-built entry that never went through [`Config::load`].
    ///
    /// Distinct from [`source`](Self::source), which discriminates the *shape*
    /// of the container source rather than naming a file.
    pub fn config_source(&self) -> Option<&ConfigSource> {
        self.source.as_ref()
    }

    pub(crate) fn set_config_source(&mut self, source: ConfigSource) {
        self.source = Some(source);
    }

    /// Directory that `dockerfile` and `context` resolve against. `repo_root`
    /// is the fallback for an entry with no recorded source, which keeps every
    /// hand-built [`ImageConfig`] resolving exactly as it did before.
    pub fn base_dir<'a>(&'a self, repo_root: &'a Path) -> &'a Path {
        self.source
            .as_ref()
            .map_or(repo_root, ConfigSource::base_dir)
    }

    /// `dockerfile` and `context` made absolute against
    /// [`base_dir`](Self::base_dir). Like [`source`](Self::source), this is
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

    /// Return the discriminated source variant. Panics if validation has not
    /// run (i.e. both or neither shape is set). Every real call path goes
    /// through `Config::load` which validates first.
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
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
        /// Always serialized (no skip) so a `Full` entry can't collapse into
        /// the `Short` shape on a round-trip.
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
    },
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
    /// command to exec.
    pub fn entrypoint(image: impl Into<String>) -> Self {
        Self::full(None, Some(image.into()))
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
    /// form; validation rejects setting both.
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

/// Accepts `model-file = "x.gguf"` *or* `model-file = ["a.gguf", "b.gguf"]`
/// during deserialization, normalizing to `Vec<String>`. The single-string
/// form keeps configs from before this field went multi (split-quantization
/// shards) parsing without a hand edit; the array form is what the init
/// flow writes today.
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
