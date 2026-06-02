use schemars::JsonSchema;
use serde::Serialize;
use serde_json::Value;

use outrig::config::{ImageConfig, McpServerSpec};

#[derive(Debug, Clone, Serialize)]
pub struct ConfigSchemaResponse {
    pub image_config_schema: Value,
    pub mcp_server_spec: Value,
    pub paths: ConfigPaths,
}

#[derive(Debug, Clone, Serialize)]
pub struct ConfigPaths {
    pub repo_config: &'static str,
    pub image_dir: &'static str,
    pub dockerfile: &'static str,
    pub context: &'static str,
    pub image_config: &'static str,
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
            image_config: "/etc/outrig/image.toml",
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
        assert_eq!(schema.paths.image_config, "/etc/outrig/image.toml");
        assert!(
            schema.image_config_schema.get("definitions").is_some(),
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
