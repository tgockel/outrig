//! Image-embedded container metadata.
//!
//! v0 reads `/etc/outrig/container.toml` from the running container and only
//! consumes its `[mcp]` table. Other top-level tables are intentionally ignored
//! so images can carry forward-looking metadata without breaking older outrig
//! binaries.

use std::collections::BTreeMap;
use std::string::FromUtf8Error;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::config::McpServerSpec;
use crate::config::validate::{is_valid_mcp_server_name, mcp_command_is_empty};
use crate::container::Container;
use crate::error::{OutrigError, Result};
use crate::process;

use super::podman_exec_root;

pub const EMBEDDED_CONTAINER_CONFIG_PATH: &str = "/etc/outrig/container.toml";

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct EmbeddedContainerConfig {
    pub mcp: BTreeMap<String, McpServerSpec>,
}

#[derive(Debug, Error)]
pub enum EmbeddedContainerConfigError {
    #[error("content is not valid UTF-8: {0}")]
    NonUtf8(#[from] FromUtf8Error),

    #[error("TOML parse failed: {0}")]
    Toml(#[from] toml::de::Error),

    #[error("mcp server name {server:?} must match ^[a-zA-Z][a-zA-Z0-9_-]*$")]
    InvalidMcpServerName { server: String },

    #[error("mcp server {server:?} has empty command")]
    EmptyMcpCommand { server: String },
}

pub async fn read_embedded_container(container: &Container) -> Result<EmbeddedContainerConfig> {
    let cmd = podman_exec_root(container.name())
        .arg("cat")
        .arg(EMBEDDED_CONTAINER_CONFIG_PATH);
    let output =
        process::try_capture_logged(cmd.clone(), "podman", container.transcript().as_ref()).await?;

    if !output.status.success() {
        if is_missing_embedded_config(&output.stderr) {
            return Ok(EmbeddedContainerConfig::default());
        }
        return Err(process::process_error_from_output(cmd, output));
    }

    parse_embedded_container(container.name(), output.stdout)
}

pub fn parse_embedded_container(
    container: &str,
    bytes: Vec<u8>,
) -> Result<EmbeddedContainerConfig> {
    let text = String::from_utf8(bytes)
        .map_err(|source| embedded_parse_error(container, source.into()))?;
    let cfg = toml::from_str::<EmbeddedContainerConfig>(&text)
        .map_err(|source| embedded_parse_error(container, source.into()))?;
    validate_embedded_container(container, &cfg)?;
    Ok(cfg)
}

pub async fn merged_mcp(
    container: &Container,
    config_mcp: &BTreeMap<String, McpServerSpec>,
) -> Result<BTreeMap<String, McpServerSpec>> {
    let embedded = read_embedded_container(container).await?;
    Ok(merge_mcp(embedded.mcp, config_mcp))
}

pub fn merge_mcp(
    mut image: BTreeMap<String, McpServerSpec>,
    config: &BTreeMap<String, McpServerSpec>,
) -> BTreeMap<String, McpServerSpec> {
    for (name, spec) in config {
        image.insert(name.clone(), spec.clone());
    }
    image
}

fn validate_embedded_container(container: &str, cfg: &EmbeddedContainerConfig) -> Result<()> {
    for (server, spec) in &cfg.mcp {
        if !is_valid_mcp_server_name(server) {
            return Err(embedded_parse_error(
                container,
                EmbeddedContainerConfigError::InvalidMcpServerName {
                    server: server.clone(),
                },
            ));
        }
        if mcp_command_is_empty(spec) {
            return Err(embedded_parse_error(
                container,
                EmbeddedContainerConfigError::EmptyMcpCommand {
                    server: server.clone(),
                },
            ));
        }
    }
    Ok(())
}

fn embedded_parse_error(container: &str, source: EmbeddedContainerConfigError) -> OutrigError {
    OutrigError::EmbeddedContainerParse {
        container: container.to_string(),
        source: Box::new(source),
    }
}

fn is_missing_embedded_config(stderr: &[u8]) -> bool {
    let stderr = String::from_utf8_lossy(stderr);
    stderr.contains(EMBEDDED_CONTAINER_CONFIG_PATH) && stderr.contains("No such file")
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::config::EnvValue;

    fn short(cmd: &[&str]) -> McpServerSpec {
        McpServerSpec::Short(cmd.iter().map(|s| s.to_string()).collect())
    }

    #[test]
    fn parse_reads_mcp_and_ignores_unknown_top_level_tables() {
        let cfg = parse_embedded_container(
            "ctr",
            br#"
            [workspace]
            hint = "/workspace"

            [mcp]
            fs = ["mcp-server-filesystem", "/workspace"]
            build = { command = ["cargo-mcp"], env = { CARGO_HOME = "/workspace/.cargo" } }
            "#
            .to_vec(),
        )
        .expect("embedded config parses");

        assert_eq!(
            cfg.mcp["fs"],
            short(&["mcp-server-filesystem", "/workspace"])
        );
        let (cmd, env) = cfg.mcp["build"].normalize();
        assert_eq!(cmd, vec!["cargo-mcp".to_string()]);
        assert_eq!(
            env["CARGO_HOME"],
            EnvValue::Literal("/workspace/.cargo".to_string())
        );
    }

    #[test]
    fn invalid_server_name_is_embedded_parse_error() {
        let err = parse_embedded_container(
            "ctr",
            br#"
            [mcp]
            "bad.name" = ["bin"]
            "#
            .to_vec(),
        )
        .unwrap_err();

        assert!(matches!(err, OutrigError::EmbeddedContainerParse { .. }));
        assert!(err.to_string().contains("bad.name"));
    }

    #[test]
    fn empty_command_is_embedded_parse_error() {
        let err = parse_embedded_container(
            "ctr",
            br#"
            [mcp]
            fs = []
            "#
            .to_vec(),
        )
        .unwrap_err();

        assert!(matches!(err, OutrigError::EmbeddedContainerParse { .. }));
        assert!(err.to_string().contains("empty command"));
    }

    #[test]
    fn merge_mcp_uses_config_as_whole_entry_override() {
        let mut image = BTreeMap::new();
        image.insert("fs".to_string(), short(&["image-fs"]));
        image.insert("shell".to_string(), short(&["image-shell"]));

        let mut config = BTreeMap::new();
        config.insert("fs".to_string(), short(&["config-fs"]));
        config.insert("build".to_string(), short(&["config-build"]));

        let merged = merge_mcp(image, &config);
        assert_eq!(merged["fs"], short(&["config-fs"]));
        assert_eq!(merged["shell"], short(&["image-shell"]));
        assert_eq!(merged["build"], short(&["config-build"]));
    }

    #[test]
    fn missing_file_detection_matches_gnu_and_busybox_cat() {
        assert!(is_missing_embedded_config(
            b"cat: /etc/outrig/container.toml: No such file or directory\n"
        ));
        assert!(is_missing_embedded_config(
            b"cat: can't open '/etc/outrig/container.toml': No such file or directory\n"
        ));
        assert!(!is_missing_embedded_config(
            b"Error: no container with name or ID \"outrig-missing\" found\n"
        ));
    }
}
