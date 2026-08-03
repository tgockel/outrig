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
}
