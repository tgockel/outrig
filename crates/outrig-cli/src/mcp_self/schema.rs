use schemars::JsonSchema;
use serde::Serialize;
use serde_json::Value;

use outrig::config::{ImageConfig, McpServerSpec};
use outrig::container::embedded::{
    LABEL_DESCRIPTION, LABEL_MCP, LABEL_SCHEMA, LABEL_TAGS, LABEL_VERSION,
};

#[derive(Debug, Clone, Serialize)]
pub struct ConfigSchemaResponse {
    pub image_config_schema: Value,
    pub mcp_server_spec: Value,
    pub paths: ConfigPaths,
    pub image_labels: ImageLabels,
}

#[derive(Debug, Clone, Serialize)]
pub struct ConfigPaths {
    pub repo_config: &'static str,
    pub image_dir: &'static str,
    pub dockerfile: &'static str,
    pub context: &'static str,
}

#[derive(Debug, Clone, Serialize)]
pub struct ImageLabels {
    pub mcp: &'static str,
    pub schema: &'static str,
    pub description: &'static str,
    pub version: &'static str,
    pub tags: &'static str,
}

pub fn get_config_schema() -> ConfigSchemaResponse {
    ConfigSchemaResponse {
        image_config_schema: schema_value::<ImageConfig>(),
        mcp_server_spec: schema_value::<McpServerSpec>(),
        paths: ConfigPaths {
            repo_config: ".agents/outrig/config.toml",
            image_dir: ".agents/outrig/images/<name>/",
            dockerfile: ".agents/outrig/images/<name>/Dockerfile",
            context: ".agents/outrig/images/<name>/",
        },
        image_labels: ImageLabels {
            mcp: LABEL_MCP,
            schema: LABEL_SCHEMA,
            description: LABEL_DESCRIPTION,
            version: LABEL_VERSION,
            tags: LABEL_TAGS,
        },
    }
}

fn schema_value<T: JsonSchema>() -> Value {
    serde_json::to_value(schemars::schema_for!(T)).expect("schema serializes")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exports_image_and_mcp_schemas() {
        let schema = get_config_schema();
        assert_eq!(schema.paths.repo_config, ".agents/outrig/config.toml");
        assert_eq!(schema.image_labels.mcp, "org.outrig.mcp");
        assert_eq!(schema.image_labels.schema, "org.outrig.schema");
        assert!(
            schema.image_config_schema.get("definitions").is_some()
                || schema.image_config_schema.get("$defs").is_some(),
            "image schema should carry definitions: {:?}",
            schema.image_config_schema,
        );
        assert!(
            schema.mcp_server_spec.get("schema").is_some()
                || schema.mcp_server_spec.get("$schema").is_some(),
            "mcp schema should be a root schema: {:?}",
            schema.mcp_server_spec,
        );
    }
}
