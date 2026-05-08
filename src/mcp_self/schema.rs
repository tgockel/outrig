use schemars::JsonSchema;
use serde::Serialize;
use serde_json::Value;

use crate::config::{ContainerConfig, McpServerSpec};

#[derive(Debug, Clone, Serialize)]
pub struct ConfigSchemaResponse {
    pub container_config: Value,
    pub mcp_server_spec: Value,
    pub paths: ConfigPaths,
}

#[derive(Debug, Clone, Serialize)]
pub struct ConfigPaths {
    pub repo_config: &'static str,
    pub container_dir: &'static str,
    pub dockerfile: &'static str,
    pub context: &'static str,
    pub image_config: &'static str,
}

pub fn get_config_schema() -> ConfigSchemaResponse {
    ConfigSchemaResponse {
        container_config: schema_value::<ContainerConfig>(),
        mcp_server_spec: schema_value::<McpServerSpec>(),
        paths: ConfigPaths {
            repo_config: ".agents/outrig/config.toml",
            container_dir: ".agents/outrig/containers/<name>/",
            dockerfile: ".agents/outrig/containers/<name>/Dockerfile",
            context: ".agents/outrig/containers/<name>/",
            image_config: "/etc/outrig/container.toml",
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
    fn exports_container_and_mcp_schemas() {
        let schema = get_config_schema();
        assert_eq!(schema.paths.repo_config, ".agents/outrig/config.toml");
        assert!(
            schema.container_config.get("definitions").is_some(),
            "container schema should carry definitions: {:?}",
            schema.container_config,
        );
        assert!(
            schema.mcp_server_spec.get("schema").is_some()
                || schema.mcp_server_spec.get("$schema").is_some(),
            "mcp schema should be a root schema: {:?}",
            schema.mcp_server_spec,
        );
    }
}
