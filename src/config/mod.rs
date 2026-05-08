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

use crate::error::Result;

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
        Ok(toml::from_str(s)?)
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
}

impl Default for Workspace {
    fn default() -> Self {
        Self {
            host_path: PathBuf::from("."),
            container_path: PathBuf::from("/workspace"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct ContainerConfig {
    pub dockerfile: PathBuf,
    pub context: PathBuf,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub build_args: BTreeMap<String, EnvValue>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub mcp: BTreeMap<String, McpServerSpec>,
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
