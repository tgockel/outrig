//! Image-embedded metadata.
//!
//! Runtime reads `/etc/outrig/image.toml` from the running container and only
//! consumes its `[mcp]` table. Other top-level tables are intentionally ignored
//! so images can carry forward-looking metadata without breaking older outrig
//! binaries.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::string::FromUtf8Error;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::config::{McpServerSpec, is_valid_mcp_server_name, mcp_command_is_empty};
use crate::container::Container;
use crate::error::{OutrigError, Result};
use crate::process;

use super::podman_exec_root;

pub const EMBEDDED_IMAGE_CONFIG_PATH: &str = "/etc/outrig/image.toml";

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct EmbeddedImageConfig {
    pub mcp: BTreeMap<String, McpServerSpec>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct StandaloneImageToml {
    pub image: StandaloneImageMetadata,
    pub build: StandaloneBuildConfig,
    pub mcp: BTreeMap<String, McpServerSpec>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StandaloneImageMetadata {
    pub image_ref: String,
    pub description: Option<String>,
    pub version: Option<String>,
    pub tags: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StandaloneBuildConfig {
    pub dockerfile: PathBuf,
    pub context: PathBuf,
}

impl Default for StandaloneBuildConfig {
    fn default() -> Self {
        Self {
            dockerfile: PathBuf::from("Dockerfile"),
            context: PathBuf::from("."),
        }
    }
}

#[derive(Debug, Error)]
pub enum EmbeddedImageConfigError {
    #[error("content is not valid UTF-8: {0}")]
    NonUtf8(#[from] FromUtf8Error),

    #[error("TOML parse failed: {0}")]
    Toml(#[from] toml::de::Error),

    #[error("invalid mcp server name {server:?} (must match ^[a-zA-Z][a-zA-Z0-9_-]*$)")]
    InvalidMcpServerName { server: String },

    #[error("mcp server {server:?} has empty command")]
    EmptyMcpCommand { server: String },
}

#[derive(Debug, Error)]
pub enum StandaloneImageTomlError {
    #[error("TOML parse failed: {0}")]
    Toml(#[from] toml::de::Error),

    #[error("image.ref is required")]
    MissingImageRef,

    #[error("build.{field} is required when [build] is present")]
    BuildFieldMissing { field: &'static str },

    #[error("mcp table must contain at least one server")]
    McpEmpty,

    #[error("invalid mcp server name {server:?} (must match ^[a-zA-Z][a-zA-Z0-9_-]*$)")]
    InvalidMcpServerName { server: String },

    #[error("mcp server {server:?} has empty command")]
    EmptyMcpCommand { server: String },
}

pub async fn read_embedded_image_config(container: &Container) -> Result<EmbeddedImageConfig> {
    let cmd = podman_exec_root(container.name())
        .arg("cat")
        .arg(EMBEDDED_IMAGE_CONFIG_PATH);
    let output =
        process::try_capture_logged(cmd.clone(), "podman", container.transcript().as_ref()).await?;

    if !output.status.success() {
        if is_missing_embedded_image_config(&output.stderr) {
            return Ok(EmbeddedImageConfig::default());
        }
        return Err(process::process_error_from_output(cmd, output));
    }

    parse_embedded_image_config(container.name(), output.stdout)
}

pub fn parse_embedded_image_config(container: &str, bytes: Vec<u8>) -> Result<EmbeddedImageConfig> {
    let text = String::from_utf8(bytes)
        .map_err(|source| embedded_image_config_parse_error(container, source.into()))?;
    let cfg = toml::from_str::<EmbeddedImageConfig>(&text)
        .map_err(|source| embedded_image_config_parse_error(container, source.into()))?;
    validate_embedded_image_config(container, &cfg)?;
    Ok(cfg)
}

pub fn parse_standalone_image_toml(
    toml: &str,
) -> std::result::Result<StandaloneImageToml, StandaloneImageTomlError> {
    let raw = toml::from_str::<StandaloneImageTomlRaw>(toml)?;
    StandaloneImageToml::try_from(raw)
}

pub async fn merged_mcp(
    container: &Container,
    config_mcp: &BTreeMap<String, McpServerSpec>,
) -> Result<BTreeMap<String, McpServerSpec>> {
    let embedded = read_embedded_image_config(container).await?;
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

fn validate_embedded_image_config(container: &str, cfg: &EmbeddedImageConfig) -> Result<()> {
    for (server, spec) in &cfg.mcp {
        if !is_valid_mcp_server_name(server) {
            return Err(embedded_image_config_parse_error(
                container,
                EmbeddedImageConfigError::InvalidMcpServerName {
                    server: server.clone(),
                },
            ));
        }
        if mcp_command_is_empty(spec) {
            return Err(embedded_image_config_parse_error(
                container,
                EmbeddedImageConfigError::EmptyMcpCommand {
                    server: server.clone(),
                },
            ));
        }
    }
    Ok(())
}

fn embedded_image_config_parse_error(
    container: &str,
    source: EmbeddedImageConfigError,
) -> OutrigError {
    OutrigError::EmbeddedImageConfigParse {
        container: container.to_string(),
        source: Box::new(source),
    }
}

fn is_missing_embedded_image_config(stderr: &[u8]) -> bool {
    let stderr = String::from_utf8_lossy(stderr);
    stderr.contains(EMBEDDED_IMAGE_CONFIG_PATH) && stderr.contains("No such file")
}

impl TryFrom<StandaloneImageTomlRaw> for StandaloneImageToml {
    type Error = StandaloneImageTomlError;

    fn try_from(raw: StandaloneImageTomlRaw) -> std::result::Result<Self, Self::Error> {
        let image = raw.image.unwrap_or_default();
        let image_ref = image
            .image_ref
            .filter(|image_ref| !image_ref.trim().is_empty())
            .ok_or(StandaloneImageTomlError::MissingImageRef)?;

        let build = match raw.build {
            Some(build) => StandaloneBuildConfig {
                dockerfile: build.dockerfile.ok_or(
                    StandaloneImageTomlError::BuildFieldMissing {
                        field: "dockerfile",
                    },
                )?,
                context: build
                    .context
                    .ok_or(StandaloneImageTomlError::BuildFieldMissing { field: "context" })?,
            },
            None => StandaloneBuildConfig::default(),
        };

        let mcp = raw.mcp.unwrap_or_default();
        if mcp.is_empty() {
            return Err(StandaloneImageTomlError::McpEmpty);
        }
        for (server, spec) in &mcp {
            if !is_valid_mcp_server_name(server) {
                return Err(StandaloneImageTomlError::InvalidMcpServerName {
                    server: server.clone(),
                });
            }
            if mcp_command_is_empty(spec) {
                return Err(StandaloneImageTomlError::EmptyMcpCommand {
                    server: server.clone(),
                });
            }
        }

        Ok(Self {
            image: StandaloneImageMetadata {
                image_ref,
                description: image.description,
                version: image.version,
                tags: image.tags.unwrap_or_default(),
            },
            build,
            mcp,
        })
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
struct StandaloneImageTomlRaw {
    image: Option<StandaloneImageMetadataRaw>,
    build: Option<StandaloneBuildConfigRaw>,
    mcp: Option<BTreeMap<String, McpServerSpec>>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
struct StandaloneImageMetadataRaw {
    #[serde(rename = "ref")]
    image_ref: Option<String>,
    description: Option<String>,
    version: Option<String>,
    tags: Option<Vec<String>>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
struct StandaloneBuildConfigRaw {
    dockerfile: Option<PathBuf>,
    context: Option<PathBuf>,
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
        let cfg = parse_embedded_image_config(
            "ctr",
            br#"
            [image]
            ref = "rust-dev"

            [build]
            dockerfile = "Dockerfile"
            context = "."

            [workspace]
            hint = "/workspace"

            [mcp]
            fs = ["mcp-server-filesystem", "/workspace"]
            build = { command = ["cargo-mcp"], env = { CARGO_HOME = "/workspace/.cargo" } }
            "#
            .to_vec(),
        )
        .expect("embedded config parses");

        let (cmd, env) = cfg.mcp["fs"].normalize();
        assert_eq!(
            cmd,
            vec![
                "mcp-server-filesystem".to_string(),
                "/workspace".to_string()
            ]
        );
        assert!(env.is_empty());
        let (cmd, env) = cfg.mcp["build"].normalize();
        assert_eq!(cmd, vec!["cargo-mcp".to_string()]);
        assert_eq!(
            env["CARGO_HOME"],
            EnvValue::Literal("/workspace/.cargo".to_string())
        );
    }

    #[test]
    fn invalid_server_name_is_embedded_parse_error() {
        let err = parse_embedded_image_config(
            "ctr",
            br#"
            [mcp]
            "bad.name" = ["bin"]
            "#
            .to_vec(),
        )
        .unwrap_err();

        assert!(matches!(err, OutrigError::EmbeddedImageConfigParse { .. }));
        assert!(err.to_string().contains("bad.name"));
    }

    #[test]
    fn empty_command_is_embedded_parse_error() {
        let err = parse_embedded_image_config(
            "ctr",
            br#"
            [mcp]
            fs = []
            "#
            .to_vec(),
        )
        .unwrap_err();

        assert!(matches!(err, OutrigError::EmbeddedImageConfigParse { .. }));
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
        assert!(is_missing_embedded_image_config(
            b"cat: /etc/outrig/image.toml: No such file or directory\n"
        ));
        assert!(is_missing_embedded_image_config(
            b"cat: can't open '/etc/outrig/image.toml': No such file or directory\n"
        ));
        assert!(!is_missing_embedded_image_config(
            b"Error: no container with name or ID \"outrig-missing\" found\n"
        ));
    }

    #[test]
    fn standalone_image_toml_accepts_explicit_build_and_metadata() {
        let cfg = parse_standalone_image_toml(
            r#"
            [image]
            ref = "rust-dev:0.1.0"
            description = "Rust tooling"
            version = "0.1.0"
            tags = ["rust", "build"]

            [build]
            dockerfile = "Containerfile"
            context = "image"

            [mcp]
            fs = { command = ["mcp-server-filesystem", "/workspace"] }
            "#,
        )
        .expect("standalone image.toml parses");

        assert_eq!(cfg.image.image_ref, "rust-dev:0.1.0");
        assert_eq!(cfg.image.description.as_deref(), Some("Rust tooling"));
        assert_eq!(cfg.image.version.as_deref(), Some("0.1.0"));
        assert_eq!(cfg.image.tags, vec!["rust", "build"]);
        assert_eq!(cfg.build.dockerfile, PathBuf::from("Containerfile"));
        assert_eq!(cfg.build.context, PathBuf::from("image"));
        let (cmd, env) = cfg.mcp["fs"].normalize();
        assert_eq!(
            cmd,
            vec![
                "mcp-server-filesystem".to_string(),
                "/workspace".to_string()
            ]
        );
        assert!(env.is_empty());
    }

    #[test]
    fn standalone_image_toml_defaults_build_to_sibling_dockerfile() {
        let cfg = parse_standalone_image_toml(
            r#"
            [image]
            ref = "rust-dev"

            [mcp]
            fs = ["mcp-server-filesystem", "/workspace"]
            "#,
        )
        .expect("standalone image.toml parses");

        assert_eq!(cfg.build.dockerfile, PathBuf::from("Dockerfile"));
        assert_eq!(cfg.build.context, PathBuf::from("."));
    }

    #[test]
    fn standalone_image_toml_requires_image_ref() {
        let err = parse_standalone_image_toml(
            r#"
            [image]
            description = "missing ref"

            [mcp]
            fs = ["mcp-server-filesystem", "/workspace"]
            "#,
        )
        .unwrap_err();

        assert!(matches!(err, StandaloneImageTomlError::MissingImageRef));
    }

    #[test]
    fn standalone_image_toml_rejects_partial_build() {
        let err = parse_standalone_image_toml(
            r#"
            [image]
            ref = "rust-dev"

            [build]
            dockerfile = "Dockerfile"

            [mcp]
            fs = ["mcp-server-filesystem", "/workspace"]
            "#,
        )
        .unwrap_err();

        assert!(matches!(
            err,
            StandaloneImageTomlError::BuildFieldMissing { field: "context" }
        ));
    }

    #[test]
    fn standalone_image_toml_rejects_empty_mcp() {
        let err = parse_standalone_image_toml(
            r#"
            [image]
            ref = "rust-dev"

            [mcp]
            "#,
        )
        .unwrap_err();

        assert!(matches!(err, StandaloneImageTomlError::McpEmpty));
    }

    #[test]
    fn standalone_image_toml_rejects_invalid_mcp_server_name() {
        let err = parse_standalone_image_toml(
            r#"
            [image]
            ref = "rust-dev"

            [mcp]
            "bad.name" = ["mcp"]
            "#,
        )
        .unwrap_err();

        assert!(matches!(
            err,
            StandaloneImageTomlError::InvalidMcpServerName { server }
                if server == "bad.name"
        ));
    }

    #[test]
    fn standalone_image_toml_rejects_empty_mcp_command() {
        let err = parse_standalone_image_toml(
            r#"
            [image]
            ref = "rust-dev"

            [mcp]
            fs = []
            "#,
        )
        .unwrap_err();

        assert!(matches!(
            err,
            StandaloneImageTomlError::EmptyMcpCommand { server } if server == "fs"
        ));
    }
}
