//! Config schema, parsing, merge, and validation.

pub mod api_key;
mod env_ref;
pub mod env_value;
#[cfg(feature = "internal")]
pub mod init;
pub mod merge;
pub mod validate;

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize};

pub use api_key::ApiKeyRef;
pub use env_value::EnvValue;
pub use merge::merge;
pub use validate::ConfigValidationError;

use crate::error::{OutrigError, Result};

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

pub const DEFAULT_TOOL_CALL_CAP: u32 = 50;
pub const MAX_TOOL_CALL_CAP: u32 = 2000;
pub const DEFAULT_TOOL_RESULT_CAP_BYTES: u32 = 256 * 1024;
pub const MIN_TOOL_RESULT_CAP_BYTES: u32 = 1024;
pub const MAX_TOOL_RESULT_CAP_BYTES: u32 = 16 * 1024 * 1024;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct Config {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_container: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_agent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_root: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_cache_root: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_cap: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_result_cap: Option<u32>,
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
    pub containers: BTreeMap<String, ContainerConfig>,
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
        let repo_path = crate::repo::repo_config_path(repo_root);
        let repo_text = fs::read_to_string(&repo_path)?;
        let repo_cfg = Self::load_from_str(&repo_text)?;

        let global_cfg = match global_path {
            Some(g) => match fs::read_to_string(g) {
                Ok(text) => Self::load_from_str(&text)?,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Self::default(),
                Err(e) => return Err(e.into()),
            },
            None => Self::default(),
        };

        let merged = merge(global_cfg, repo_cfg);
        merged.validate(Some(repo_root))?;
        Ok(merged)
    }

    /// Validate every cross-reference rule documented in `doc/reference/config.md`.
    /// `repo_root: Some(_)` enables `dockerfile`/`context` on-disk existence checks;
    /// `None` keeps the check pure-structural for unit tests.
    pub fn validate(&self, repo_root: Option<&Path>) -> Result<()> {
        validate::validate(self, repo_root)?;
        Ok(())
    }
}

fn declares_top_level_network(text: &str) -> Result<bool> {
    let value = text.parse::<toml_edit::DocumentMut>().map_err(|source| {
        crate::error::OutrigError::Configuration(format!("parsing config for [network]: {source}"))
    })?;
    Ok(value.as_table().contains_key("network"))
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "style",
    rename_all = "kebab-case",
    rename_all_fields = "kebab-case",
    deny_unknown_fields
)]
pub enum LlmProvider {
    // The kebab-case rule auto-converts `OpenAi` to `open-ai`; the doc'd
    // tag is `openai`, so override per-variant.
    #[serde(rename = "openai")]
    OpenAi {
        base_url: String,
        api_key: ApiKeyRef,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        request_timeout_secs: Option<u64>,
    },
    Mistralrs,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
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
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct Agent {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preamble: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_cap: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_result_cap: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct MountConfig {
    pub host_path: PathBuf,
    pub container_path: PathBuf,
    #[serde(default)]
    pub access: MountAccess,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MountAccess {
    #[default]
    ReadOnly,
    ReadWrite,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum NetworkMode {
    #[default]
    Default,
    Audit,
}

impl std::str::FromStr for NetworkMode {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s {
            "default" => Ok(Self::Default),
            "audit" => Ok(Self::Audit),
            _ => Err("expected one of: default, audit".to_string()),
        }
    }
}

impl std::fmt::Display for NetworkMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Default => f.write_str("default"),
            Self::Audit => f.write_str("audit"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
pub struct NetworkConfig {
    pub mode: NetworkMode,
    #[serde(skip)]
    #[schemars(skip)]
    declared: bool,
}

impl Default for NetworkConfig {
    fn default() -> Self {
        Self {
            mode: NetworkMode::Default,
            declared: false,
        }
    }
}

impl PartialEq for NetworkConfig {
    fn eq(&self, other: &Self) -> bool {
        self.mode == other.mode
    }
}

impl Eq for NetworkConfig {}

impl NetworkConfig {
    pub(crate) fn is_declared(&self) -> bool {
        self.declared
    }
}

impl NetworkConfig {
    fn is_default(&self) -> bool {
        self == &Self::default()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
pub struct ContainerSecurity {
    pub capability_profile: CapabilityProfile,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cap_drop: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cap_add: Vec<String>,
}

impl ContainerSecurity {
    fn is_default(&self) -> bool {
        self == &Self::default()
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
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
pub struct ContainerConfig {
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
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub mcp: BTreeMap<String, McpServerSpec>,
}

/// Discriminated view of the container source -- build-from-Dockerfile or
/// use-existing-image. Returned by [`ContainerConfig::source`].
pub enum ContainerSourceRef<'a> {
    Build {
        dockerfile: &'a Path,
        context: &'a Path,
        build_args: &'a BTreeMap<String, EnvValue>,
    },
    Image {
        image_name: &'a str,
    },
}

impl ContainerConfig {
    /// Return the discriminated source variant. Panics if validation has not
    /// run (i.e. both or neither shape is set). Every real call path goes
    /// through `Config::load` which validates first.
    pub fn source(&self) -> ContainerSourceRef<'_> {
        match (&self.image_name, &self.dockerfile, &self.context) {
            (Some(name), None, None) => ContainerSourceRef::Image { image_name: name },
            (None, Some(df), Some(ctx)) => ContainerSourceRef::Build {
                dockerfile: df,
                context: ctx,
                build_args: &self.build_args,
            },
            _ => panic!(
                "ContainerConfig::source() called on an unvalidated config; \
                 call Config::validate() first"
            ),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum McpServerSpec {
    Short(Vec<String>),
    Full {
        command: Vec<String>,
        #[serde(default)]
        env: BTreeMap<String, EnvValue>,
    },
}

impl McpServerSpec {
    /// Returns the argv and the (still-unresolved) env map. Resolution of any
    /// `EnvValue::EnvRef` entries happens at the call site that's about to
    /// spawn the MCP server, so a missing host env var is reported as an
    /// MCP-startup failure rather than a config-load failure.
    pub fn normalize(&self) -> (Vec<String>, BTreeMap<String, EnvValue>) {
        match self {
            Self::Short(command) => (command.clone(), BTreeMap::new()),
            Self::Full { command, env } => (command.clone(), env.clone()),
        }
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
