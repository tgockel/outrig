//! Tool names and argument shapes for outrig's self-description surface.
//!
//! Shared by the two front-ends over it: [`super::server`], which serves them
//! to an external authoring client over rmcp, and [`crate::self_tool`], which
//! offers them to an in-session agent as `outrig__*` built-ins. One definition
//! each, so the two cannot drift in what a tool is called or what it accepts.

use schemars::JsonSchema;
use serde::Deserialize;

pub(crate) const LIST_DOCS: &str = "list_docs";
pub(crate) const GET_DOC: &str = "get_doc";
pub(crate) const GET_CONFIG_SCHEMA: &str = "get_config_schema";
pub(crate) const LIST_BASE_IMAGES: &str = "list_base_images";
pub(crate) const LIST_MCP_SERVER_SUGGESTIONS: &str = "list_mcp_server_suggestions";
pub(crate) const VALIDATE_DOCKERFILE: &str = "validate_dockerfile";
pub(crate) const VALIDATE_CONFIG: &str = "validate_config";
pub(crate) const VALIDATE_IMAGE_TOML: &str = "validate_image_toml";

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct EmptyArgs {}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct GetDocArgs {
    pub(crate) page: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ValidateDockerfileArgs {
    pub(crate) dockerfile: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ValidateConfigArgs {
    pub(crate) toml: String,
    /// Which config file this fragment is written for. Defaults to `repo`.
    #[serde(default)]
    pub(crate) scope: ConfigScope,
}

/// Which of outrig's two config files a fragment is written for.
///
/// The files share a schema but not their rules: the keys that describe the
/// operator's own machine -- `session-root`, `model-cache-root`, the
/// `[network]` policy keys, `[repo].env-allow`, and a provider carrying an
/// `api-key` -- belong to the global config, and a repo config that sets one
/// is refused at load.
// Defaulting to `Repo` is the point of the argument. That is the file this
// surface exists to help write -- `.agents/outrig/config.toml`, the one path
// `get_config_schema` names -- and the two ways to be wrong are not
// symmetric. A global fragment checked as a repo one is refused by an error
// that names the key and this argument, which an agent can act on; a repo
// fragment checked as a global one would be called valid and then refused by
// `outrig run`, after the user installed it.
#[derive(Debug, Clone, Copy, Default, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum ConfigScope {
    /// The repo config, `.agents/outrig/config.toml`.
    #[default]
    Repo,
    /// The global config, `~/.outrig/config.toml`.
    Global,
}

/// `validate_image_toml` takes a whole `image.toml`, which is neither config
/// file and has no scope to pick. It shared [`ValidateConfigArgs`] while the
/// two shapes were identical; a `scope` it would have to ignore is what
/// separates them.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ValidateImageTomlArgs {
    pub(crate) toml: String,
}
