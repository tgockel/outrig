//! Image-embedded metadata.
//!
//! A standalone image advertises its MCP/config via OCI image labels (the
//! `LABEL_*` constants below). Runtime reads them with `podman image inspect`
//! -- no running container, no pull -- and consumes only `org.outrig.mcp`;
//! other labels are ignored so images can carry forward-looking metadata
//! without breaking older outrig binaries. Authoring is unchanged: humans
//! still write `image.toml` (TOML on disk), which `outrig image build`
//! validates and serializes into labels.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::Deserialize;
use thiserror::Error;

use crate::config::{McpServerSpec, is_valid_mcp_server_name, mcp_command_is_empty};
use crate::container::Container;
use crate::error::{OutrigError, Result};

/// `org.opencontainers.image.description` <- `[image].description`.
pub const LABEL_DESCRIPTION: &str = "org.opencontainers.image.description";
/// `org.opencontainers.image.version` <- `[image].version`.
pub const LABEL_VERSION: &str = "org.opencontainers.image.version";
/// `org.outrig.tags` <- `[image].tags`, serialized as a JSON array.
pub const LABEL_TAGS: &str = "org.outrig.tags";
/// `org.outrig.mcp` <- the `[mcp]` table, serialized as a JSON object. The
/// load-bearing label: the one runtime and build validation read back.
pub const LABEL_MCP: &str = "org.outrig.mcp";
/// `org.outrig.schema` <- the config schema version, stamped for forward
/// compatibility. Not read in this version.
pub const LABEL_SCHEMA: &str = "org.outrig.schema";
/// Current schema version stamped into [`LABEL_SCHEMA`].
pub const LABEL_SCHEMA_VERSION: &str = "1";

#[derive(Debug, Clone, PartialEq)]
pub struct StandaloneImageToml {
    pub image: StandaloneImageMetadata,
    pub build: StandaloneBuildConfig,
    pub mcp: BTreeMap<String, McpServerSpec>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct StandaloneImageLabels {
    pub description: Option<String>,
    pub version: Option<String>,
    pub tags: Vec<String>,
    pub mcp: Option<BTreeMap<String, McpServerSpec>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpDeclarationSource {
    ImageLabel,
    LaunchSpec,
    ConfigToml,
}

impl McpDeclarationSource {
    pub fn description(self) -> &'static str {
        match self {
            Self::ImageLabel => "image label org.outrig.mcp",
            Self::LaunchSpec => "launch spec",
            Self::ConfigToml => "config.toml",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct McpServerSpecWithSource {
    pub spec: McpServerSpec,
    pub source: McpDeclarationSource,
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
    #[error("org.outrig.mcp is not valid JSON: {0}")]
    Json(#[from] serde_json::Error),

    #[error("invalid mcp server name {server:?} (must match ^[a-zA-Z][a-zA-Z0-9_-]*$)")]
    InvalidMcpServerName { server: String },

    #[error(
        "mcp server {server:?} uses a name reserved for OutRig's built-in tools; \
         its tools would shadow `{server}__*`"
    )]
    ReservedMcpServerName { server: String },

    #[error("mcp server {server:?} has empty command")]
    EmptyMcpCommand { server: String },

    #[error(
        "mcp server {server:?} carries a placement key (sidecar/image); image labels \
         declare servers for the image that carries them -- placement is repo-config-only"
    )]
    PlacementInLabel { server: String },

    #[error(
        "mcp server {server:?} carries `args`; image labels declare exec-stdio servers, \
         whose arguments belong in `command` -- `args` is the argv of a sidecar \
         ENTRYPOINT and is repo-config-only"
    )]
    ArgsInLabel { server: String },
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

    #[error(
        "mcp server {server:?} uses a name reserved for OutRig's built-in tools; \
         its tools would shadow `{server}__*`"
    )]
    ReservedMcpServerName { server: String },

    #[error("mcp server {server:?} has empty command")]
    EmptyMcpCommand { server: String },

    #[error(
        "mcp server {server:?} carries a placement key (sidecar/image); image labels \
         declare servers for the image that carries them -- placement is repo-config-only"
    )]
    PlacementInLabel { server: String },

    #[error(
        "mcp server {server:?} carries `args`; image labels declare exec-stdio servers, \
         whose arguments belong in `command` -- `args` is the argv of a sidecar \
         ENTRYPOINT and is repo-config-only"
    )]
    ArgsInLabel { server: String },
}

pub fn parse_standalone_image_toml(
    toml: &str,
) -> std::result::Result<StandaloneImageToml, StandaloneImageTomlError> {
    let raw = toml::from_str::<StandaloneImageTomlRaw>(toml)?;
    StandaloneImageToml::try_from(raw)
}

/// Strict build-validation read: inspect the built image's labels and decode a
/// non-empty, valid `org.outrig.mcp`. Unlike the runtime merge path (lenient --
/// a missing label yields an empty config), a missing or empty label is a hard
/// error here. `outrig image build` uses it to prove the stamped config
/// round-trips off the built image.
pub async fn read_standalone_image_mcp(
    container: &Container,
) -> Result<BTreeMap<String, McpServerSpec>> {
    let labels =
        crate::image::read_image_labels(container.image_tag(), container.transcript().as_ref())
            .await?;
    standalone_mcp_from_labels(&labels)
}

/// Serialize an MCP table into the common OutRig OCI labels. Repo-local image
/// builds use this for cache-key input before build time; standalone builds
/// add author-facing metadata labels on top.
pub(crate) fn mcp_config_to_labels(
    mcp: &BTreeMap<String, McpServerSpec>,
) -> Result<BTreeMap<String, String>> {
    let mut labels = BTreeMap::new();
    labels.insert(LABEL_SCHEMA.to_string(), LABEL_SCHEMA_VERSION.to_string());
    labels.insert(LABEL_MCP.to_string(), to_json_label(LABEL_MCP, mcp)?);
    Ok(labels)
}

/// Merge an already-built image's inherited/Dockerfile MCP label with repo
/// config, then serialize the effective table back into labels. This preserves
/// the runtime merge semantics for build-from-Dockerfile images: image-owned
/// entries survive, and repo entries replace by server name. Placement-bearing
/// config entries are excluded -- they declare servers for *other* containers,
/// and labels are image-scoped.
pub(crate) fn merged_mcp_config_to_labels(
    image: &str,
    labels: &BTreeMap<String, String>,
    config_mcp: &BTreeMap<String, McpServerSpec>,
) -> Result<BTreeMap<String, String>> {
    let image_mcp = embedded_mcp_from_labels(image, labels)?;
    mcp_config_to_labels(&merge_mcp(image_mcp, &primary_scoped_mcp(config_mcp)))
}

/// The subset of a config MCP table that belongs to the image itself: entries
/// without a `sidecar`/`image` placement key. Used when serializing config
/// into an image's own `org.outrig.mcp` label (and its cache key), where
/// placement keys are rejected on read.
pub(crate) fn primary_scoped_mcp(
    config: &BTreeMap<String, McpServerSpec>,
) -> BTreeMap<String, McpServerSpec> {
    config
        .iter()
        .filter(|(_, spec)| !spec.is_placed())
        .map(|(name, spec)| (name.clone(), spec.clone()))
        .collect()
}

/// Serialize a validated standalone config into the OCI label map buildah
/// stamps at build time. `org.outrig.mcp` (the `[mcp]` table) and
/// `org.outrig.schema` are always emitted; description, version, and tags only
/// when the author set them.
pub fn standalone_config_to_labels(cfg: &StandaloneImageToml) -> Result<BTreeMap<String, String>> {
    let mut labels = mcp_config_to_labels(&cfg.mcp)?;
    if let Some(description) = &cfg.image.description {
        labels.insert(LABEL_DESCRIPTION.to_string(), description.clone());
    }
    if let Some(version) = &cfg.image.version {
        labels.insert(LABEL_VERSION.to_string(), version.clone());
    }
    if !cfg.image.tags.is_empty() {
        labels.insert(
            LABEL_TAGS.to_string(),
            to_json_label(LABEL_TAGS, &cfg.image.tags)?,
        );
    }
    Ok(labels)
}

/// Decode all user-facing standalone image labels for `outrig image inspect`.
/// Missing metadata labels are omitted, and a missing `org.outrig.mcp` is not
/// an error: inspect can still show whatever metadata the local image carries.
/// A present-but-malformed label is a hard error because it claims to be an
/// OutRig declaration.
pub fn parse_standalone_image_labels(
    image: &str,
    labels: &BTreeMap<String, String>,
) -> Result<StandaloneImageLabels> {
    let tags = match labels.get(LABEL_TAGS) {
        Some(raw) => serde_json::from_str::<Vec<String>>(raw).map_err(|source| {
            OutrigError::Configuration(format!(
                "image {image:?}: invalid {LABEL_TAGS} label: {source}"
            ))
        })?,
        None => Vec::new(),
    };
    let mcp = match labels.get(LABEL_MCP) {
        Some(raw) => {
            let mcp = parse_mcp_table(raw)
                .map_err(|source| embedded_image_config_parse_error(image, source))?;
            Some(mcp)
        }
        None => None,
    };
    Ok(StandaloneImageLabels {
        description: labels.get(LABEL_DESCRIPTION).cloned(),
        version: labels.get(LABEL_VERSION).cloned(),
        tags,
        mcp,
    })
}

fn to_json_label<T: serde::Serialize>(key: &str, value: &T) -> Result<String> {
    serde_json::to_string(value)
        .map_err(|source| OutrigError::Configuration(format!("serialize {key} label: {source}")))
}

/// Lenient runtime read of an image's `org.outrig.mcp` label by tag: a
/// missing label is an empty map, a malformed one is a hard error. Sidecar
/// planning uses this to materialize label-declared servers before any
/// sidecar container exists.
pub async fn read_embedded_mcp(
    tag: &crate::image::ImageTag,
    transcript: Option<&crate::process::Transcript>,
) -> Result<BTreeMap<String, McpServerSpec>> {
    let labels = crate::image::read_image_labels(tag, transcript).await?;
    embedded_mcp_from_labels(&tag.0, &labels)
}

/// The MCP table a running container will actually serve: the servers
/// declared by its image's `org.outrig.mcp` label, overlaid with `config_mcp`
/// by server name. Reads the label off the live container, so it reflects the
/// image that started rather than whatever the config now resolves to.
///
/// This is the read-side counterpart to the label writers above, and the entry
/// point a caller driving a `Container` directly wants -- the merge stages
/// beneath it are crate-private.
pub async fn merged_mcp(
    container: &Container,
    config_mcp: &BTreeMap<String, McpServerSpec>,
) -> Result<BTreeMap<String, McpServerSpec>> {
    let sourced =
        merged_mcp_with_source(container, config_mcp, McpDeclarationSource::ConfigToml).await?;
    Ok(strip_mcp_sources(&sourced))
}

pub(crate) async fn merged_mcp_with_source(
    container: &Container,
    config_mcp: &BTreeMap<String, McpServerSpec>,
    config_source: McpDeclarationSource,
) -> Result<BTreeMap<String, McpServerSpecWithSource>> {
    let labels =
        crate::image::read_image_labels(container.image_tag(), container.transcript().as_ref())
            .await?;
    let image_mcp = embedded_mcp_from_labels(&container.image_tag().0, &labels)?;
    Ok(merge_mcp_with_source(image_mcp, config_mcp, config_source))
}

pub(crate) fn mcp_with_source(
    mcp: &BTreeMap<String, McpServerSpec>,
    source: McpDeclarationSource,
) -> BTreeMap<String, McpServerSpecWithSource> {
    mcp.iter()
        .map(|(name, spec)| {
            (
                name.clone(),
                McpServerSpecWithSource {
                    spec: spec.clone(),
                    source,
                },
            )
        })
        .collect()
}

fn strip_mcp_sources(
    mcp: &BTreeMap<String, McpServerSpecWithSource>,
) -> BTreeMap<String, McpServerSpec> {
    mcp.iter()
        .map(|(name, sourced)| (name.clone(), sourced.spec.clone()))
        .collect()
}

pub(crate) fn merge_mcp(
    mut image: BTreeMap<String, McpServerSpec>,
    config: &BTreeMap<String, McpServerSpec>,
) -> BTreeMap<String, McpServerSpec> {
    for (name, spec) in config {
        image.insert(name.clone(), spec.clone());
    }
    image
}

pub(crate) fn merge_mcp_with_source(
    image: BTreeMap<String, McpServerSpec>,
    config: &BTreeMap<String, McpServerSpec>,
    config_source: McpDeclarationSource,
) -> BTreeMap<String, McpServerSpecWithSource> {
    let mut merged = mcp_with_source(&image, McpDeclarationSource::ImageLabel);
    for (name, spec) in config {
        merged.insert(
            name.clone(),
            McpServerSpecWithSource {
                spec: spec.clone(),
                source: config_source,
            },
        );
    }
    merged
}

/// Parse and validate the `org.outrig.mcp` label value (a JSON object mapping
/// server name -> spec): every name must match the server-name pattern and
/// carry a non-empty command. Shared by the lenient runtime read and the
/// strict build-validation read.
fn parse_mcp_table(
    raw: &str,
) -> std::result::Result<BTreeMap<String, McpServerSpec>, EmbeddedImageConfigError> {
    let mcp = serde_json::from_str::<BTreeMap<String, McpServerSpec>>(raw)?;
    for (server, spec) in &mcp {
        if !is_valid_mcp_server_name(server) {
            return Err(EmbeddedImageConfigError::InvalidMcpServerName {
                server: server.clone(),
            });
        }
        if server == crate::RESERVED_SERVER {
            return Err(EmbeddedImageConfigError::ReservedMcpServerName {
                server: server.clone(),
            });
        }
        if !spec.args().is_empty() {
            return Err(EmbeddedImageConfigError::ArgsInLabel {
                server: server.clone(),
            });
        }
        if mcp_command_is_empty(spec) {
            return Err(EmbeddedImageConfigError::EmptyMcpCommand {
                server: server.clone(),
            });
        }
        if spec.is_placed() {
            return Err(EmbeddedImageConfigError::PlacementInLabel {
                server: server.clone(),
            });
        }
    }
    Ok(mcp)
}

/// Lenient decode for the runtime read: a missing `org.outrig.mcp` label is an
/// empty config (fall back to repo config); a present-but-malformed label is a
/// hard error. `image` names the offending image in the error.
fn embedded_mcp_from_labels(
    image: &str,
    labels: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, McpServerSpec>> {
    match labels.get(LABEL_MCP) {
        None => Ok(BTreeMap::new()),
        Some(raw) => {
            let mcp = parse_mcp_table(raw)
                .map_err(|source| embedded_image_config_parse_error(image, source))?;
            Ok(mcp)
        }
    }
}

/// Strict decode for build validation: the built image must carry a non-empty,
/// valid `org.outrig.mcp` label.
fn standalone_mcp_from_labels(
    labels: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, McpServerSpec>> {
    let raw = labels.get(LABEL_MCP).ok_or_else(|| {
        OutrigError::Configuration(format!(
            "built image is missing the {LABEL_MCP} label \
             (a standalone image must stamp its config)"
        ))
    })?;
    let mcp = parse_mcp_table(raw).map_err(|source| {
        OutrigError::Configuration(format!("invalid {LABEL_MCP} label: {source}"))
    })?;
    if mcp.is_empty() {
        return Err(OutrigError::Configuration(format!(
            "{LABEL_MCP} label must contain at least one server"
        )));
    }
    Ok(mcp)
}

fn embedded_image_config_parse_error(image: &str, source: EmbeddedImageConfigError) -> OutrigError {
    OutrigError::EmbeddedImageConfigParse {
        image: image.to_string(),
        source: Box::new(source),
    }
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
            if server == crate::RESERVED_SERVER {
                return Err(StandaloneImageTomlError::ReservedMcpServerName {
                    server: server.clone(),
                });
            }
            if !spec.args().is_empty() {
                return Err(StandaloneImageTomlError::ArgsInLabel {
                    server: server.clone(),
                });
            }
            if mcp_command_is_empty(spec) {
                return Err(StandaloneImageTomlError::EmptyMcpCommand {
                    server: server.clone(),
                });
            }
            if spec.is_placed() {
                return Err(StandaloneImageTomlError::PlacementInLabel {
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

    fn full(cmd: &[&str], env: &[(&str, EnvValue)]) -> McpServerSpec {
        McpServerSpec::Full {
            command: Some(cmd.iter().map(|s| s.to_string()).collect()),
            env: env
                .iter()
                .map(|(k, v)| (k.to_string(), v.clone()))
                .collect(),
            view: crate::config::SidecarView::None,
            sidecar: None,
            image: None,
            args: Vec::new(),
        }
    }

    fn standalone(
        mcp: BTreeMap<String, McpServerSpec>,
        description: Option<&str>,
        version: Option<&str>,
        tags: &[&str],
    ) -> StandaloneImageToml {
        StandaloneImageToml {
            image: StandaloneImageMetadata {
                image_ref: "img".to_string(),
                description: description.map(str::to_string),
                version: version.map(str::to_string),
                tags: tags.iter().map(|s| s.to_string()).collect(),
            },
            build: StandaloneBuildConfig::default(),
            mcp,
        }
    }

    #[test]
    fn labels_round_trip_short_and_full_specs() {
        let mut mcp = BTreeMap::new();
        mcp.insert(
            "fs".to_string(),
            short(&["mcp-server-filesystem", "/workspace"]),
        );
        mcp.insert(
            "build".to_string(),
            full(
                &["cargo-mcp"],
                &[(
                    "CARGO_HOME",
                    EnvValue::Literal("/workspace/.cargo".to_string()),
                )],
            ),
        );
        let cfg = standalone(mcp.clone(), None, None, &[]);

        let labels = standalone_config_to_labels(&cfg).expect("serialize labels");
        let parsed = parse_mcp_table(&labels[LABEL_MCP]).expect("parse mcp label");

        assert_eq!(parsed, mcp);
        assert!(matches!(parsed["fs"], McpServerSpec::Short(_)));
        assert!(matches!(parsed["build"], McpServerSpec::Full { .. }));
    }

    #[test]
    fn full_spec_with_empty_env_round_trips_as_full() {
        // Guards the untagged ordering (Short before Full) plus the always-
        // serialized `env`: an empty-env Full must not collapse to Short on the
        // way back through the label.
        let mut mcp = BTreeMap::new();
        mcp.insert("build".to_string(), full(&["cargo-mcp"], &[]));
        let cfg = standalone(mcp, None, None, &[]);

        let labels = standalone_config_to_labels(&cfg).expect("serialize labels");
        let parsed = parse_mcp_table(&labels[LABEL_MCP]).expect("parse mcp label");

        assert!(matches!(parsed["build"], McpServerSpec::Full { .. }));
    }

    #[test]
    fn env_ref_round_trips_through_label() {
        let mut mcp = BTreeMap::new();
        mcp.insert(
            "db".to_string(),
            full(
                &["db-mcp"],
                &[("TOKEN", EnvValue::EnvRef("DB_TOKEN".to_string()))],
            ),
        );
        let cfg = standalone(mcp, None, None, &[]);

        let labels = standalone_config_to_labels(&cfg).expect("serialize labels");
        let parsed = parse_mcp_table(&labels[LABEL_MCP]).expect("parse mcp label");

        let (_, env) = parsed["db"].normalize();
        assert_eq!(env["TOKEN"], EnvValue::EnvRef("DB_TOKEN".to_string()));
    }

    #[test]
    fn metadata_labels_present_when_set() {
        let mut mcp = BTreeMap::new();
        mcp.insert(
            "fs".to_string(),
            short(&["mcp-server-filesystem", "/workspace"]),
        );
        let cfg = standalone(mcp, Some("Rust tooling"), Some("0.1.0"), &["rust", "build"]);

        let labels = standalone_config_to_labels(&cfg).expect("serialize labels");

        assert_eq!(labels[LABEL_SCHEMA], "1");
        assert_eq!(labels[LABEL_DESCRIPTION], "Rust tooling");
        assert_eq!(labels[LABEL_VERSION], "0.1.0");
        assert_eq!(labels[LABEL_TAGS], r#"["rust","build"]"#);
    }

    #[test]
    fn metadata_labels_absent_when_unset() {
        let mut mcp = BTreeMap::new();
        mcp.insert(
            "fs".to_string(),
            short(&["mcp-server-filesystem", "/workspace"]),
        );
        let cfg = standalone(mcp, None, None, &[]);

        let labels = standalone_config_to_labels(&cfg).expect("serialize labels");

        assert!(!labels.contains_key(LABEL_DESCRIPTION));
        assert!(!labels.contains_key(LABEL_VERSION));
        assert!(!labels.contains_key(LABEL_TAGS));
        assert!(labels.contains_key(LABEL_MCP));
        assert!(labels.contains_key(LABEL_SCHEMA));
    }

    #[test]
    fn mcp_config_labels_serialize_empty_table_for_repo_builds() {
        let labels = mcp_config_to_labels(&BTreeMap::new()).expect("labels");

        assert_eq!(labels[LABEL_SCHEMA], LABEL_SCHEMA_VERSION);
        assert_eq!(labels[LABEL_MCP], "{}");
    }

    #[test]
    fn merged_mcp_config_labels_preserve_inherited_entries() {
        let inherited = BTreeMap::from([(
            LABEL_MCP.to_string(),
            r#"{"fs":["mcp-server-filesystem","/workspace"],"shell":["old-shell"]}"#.to_string(),
        )]);
        let config = BTreeMap::from([
            ("git".to_string(), short(&["mcp-server-git"])),
            ("shell".to_string(), short(&["new-shell"])),
        ]);

        let labels =
            merged_mcp_config_to_labels("img", &inherited, &config).expect("merged labels");
        let parsed = parse_mcp_table(&labels[LABEL_MCP]).expect("parse merged mcp");

        assert_eq!(
            parsed["fs"],
            short(&["mcp-server-filesystem", "/workspace"])
        );
        assert_eq!(parsed["git"], short(&["mcp-server-git"]));
        assert_eq!(parsed["shell"], short(&["new-shell"]));
        assert_eq!(labels[LABEL_SCHEMA], LABEL_SCHEMA_VERSION);
    }

    #[test]
    fn merged_mcp_config_labels_reject_malformed_inherited_label() {
        let inherited = BTreeMap::from([(LABEL_MCP.to_string(), r#"{"fs": ["#.to_string())]);

        let err = merged_mcp_config_to_labels("img", &inherited, &BTreeMap::new()).unwrap_err();

        assert!(matches!(err, OutrigError::EmbeddedImageConfigParse { .. }));
    }

    #[test]
    fn standalone_image_labels_read_metadata_and_mcp() {
        let mut mcp = BTreeMap::new();
        mcp.insert(
            "fs".to_string(),
            short(&["mcp-server-filesystem", "/workspace"]),
        );
        let cfg = standalone(
            mcp.clone(),
            Some("Rust tooling"),
            Some("0.1.0"),
            &["rust", "build"],
        );
        let labels = standalone_config_to_labels(&cfg).expect("serialize labels");

        let parsed = parse_standalone_image_labels("img", &labels).expect("parse labels");

        assert_eq!(parsed.description.as_deref(), Some("Rust tooling"));
        assert_eq!(parsed.version.as_deref(), Some("0.1.0"));
        assert_eq!(parsed.tags, vec!["rust", "build"]);
        assert_eq!(parsed.mcp, Some(mcp));
    }

    #[test]
    fn standalone_image_labels_allow_missing_mcp() {
        let mut labels = BTreeMap::new();
        labels.insert(LABEL_DESCRIPTION.to_string(), "metadata only".to_string());
        labels.insert(LABEL_TAGS.to_string(), r#"["docs"]"#.to_string());

        let parsed = parse_standalone_image_labels("img", &labels).expect("parse labels");

        assert_eq!(parsed.description.as_deref(), Some("metadata only"));
        assert_eq!(parsed.tags, vec!["docs"]);
        assert_eq!(parsed.mcp, None);
    }

    #[test]
    fn standalone_image_labels_reject_malformed_tags() {
        let mut labels = BTreeMap::new();
        labels.insert(LABEL_TAGS.to_string(), "[".to_string());

        let err = parse_standalone_image_labels("img", &labels).unwrap_err();

        assert!(matches!(err, OutrigError::Configuration(_)));
        assert!(err.to_string().contains(LABEL_TAGS), "{err}");
    }

    #[test]
    fn standalone_image_labels_reject_malformed_mcp() {
        let mut labels = BTreeMap::new();
        labels.insert(LABEL_MCP.to_string(), r#"{"bad.name":["bin"]}"#.to_string());

        let err = parse_standalone_image_labels("img", &labels).unwrap_err();

        assert!(matches!(err, OutrigError::EmbeddedImageConfigParse { .. }));
        assert!(err.to_string().contains("bad.name"), "{err}");
    }

    #[test]
    fn parse_mcp_table_rejects_bad_json() {
        let err = parse_mcp_table(r#"{"fs": ["#).unwrap_err();
        assert!(matches!(err, EmbeddedImageConfigError::Json(_)));
    }

    #[test]
    fn parse_mcp_table_rejects_invalid_server_name() {
        let err = parse_mcp_table(r#"{"bad.name": ["bin"]}"#).unwrap_err();
        assert!(matches!(
            err,
            EmbeddedImageConfigError::InvalidMcpServerName { server } if server == "bad.name"
        ));
    }

    /// The repo-config path rejects `outrig`; an image label is exactly where
    /// a server with that name would otherwise arrive unnoticed and shadow the
    /// built-in `outrig__*` tools.
    #[test]
    fn label_mcp_server_named_outrig_is_rejected() {
        let err = parse_mcp_table(r#"{"outrig":{"command":["x"]}}"#)
            .expect_err("reserved name in a label");
        assert!(
            matches!(
                err,
                EmbeddedImageConfigError::ReservedMcpServerName { ref server }
                    if server == "outrig"
            ),
            "got: {err:?}"
        );
    }

    #[test]
    fn parse_mcp_table_rejects_empty_command() {
        let err = parse_mcp_table(r#"{"fs": []}"#).unwrap_err();
        assert!(matches!(
            err,
            EmbeddedImageConfigError::EmptyMcpCommand { server } if server == "fs"
        ));
    }

    #[test]
    fn parse_mcp_table_rejects_placement_keys() {
        // Image labels declare servers for the image that carries them;
        // placement (`sidecar` / `image`) is repo-config-only.
        let err =
            parse_mcp_table(r#"{"fs": {"command": ["bin"], "sidecar": "tools"}}"#).unwrap_err();
        assert!(matches!(
            err,
            EmbeddedImageConfigError::PlacementInLabel { ref server } if server == "fs"
        ));

        let err =
            parse_mcp_table(r#"{"fs": {"command": ["bin"], "image": "some-img"}}"#).unwrap_err();
        assert!(matches!(
            err,
            EmbeddedImageConfigError::PlacementInLabel { ref server } if server == "fs"
        ));
    }

    /// `args` is the argv of a sidecar's ENTRYPOINT, and a label cannot
    /// declare a sidecar -- so it is rejected outright rather than parsed and
    /// silently dropped. Checked before the empty-command rule so the error
    /// names the real problem.
    #[test]
    fn parse_mcp_table_rejects_args() {
        let err = parse_mcp_table(r#"{"fs": {"command": ["bin"], "args": ["/w"]}}"#).unwrap_err();
        assert!(matches!(
            err,
            EmbeddedImageConfigError::ArgsInLabel { ref server } if server == "fs"
        ));

        // Without a command there is nothing for the label to run either;
        // `args` still names the actual mistake.
        let err = parse_mcp_table(r#"{"fs": {"args": ["/w"]}}"#).unwrap_err();
        assert!(matches!(
            err,
            EmbeddedImageConfigError::ArgsInLabel { ref server } if server == "fs"
        ));
    }

    #[test]
    fn embedded_mcp_missing_label_is_empty() {
        let mcp =
            embedded_mcp_from_labels("img", &BTreeMap::new()).expect("missing label is lenient");
        assert!(mcp.is_empty());
    }

    #[test]
    fn embedded_mcp_malformed_label_is_hard_error() {
        let mut labels = BTreeMap::new();
        labels.insert(
            LABEL_MCP.to_string(),
            r#"{"bad.name": ["bin"]}"#.to_string(),
        );
        let err = embedded_mcp_from_labels("img", &labels).unwrap_err();
        assert!(matches!(err, OutrigError::EmbeddedImageConfigParse { .. }));
        assert!(err.to_string().contains("bad.name"));
    }

    #[test]
    fn standalone_mcp_missing_label_is_hard_error() {
        let err = standalone_mcp_from_labels(&BTreeMap::new()).unwrap_err();
        assert!(matches!(err, OutrigError::Configuration(_)));
        assert!(err.to_string().contains(LABEL_MCP));
    }

    #[test]
    fn standalone_mcp_empty_table_is_hard_error() {
        let mut labels = BTreeMap::new();
        labels.insert(LABEL_MCP.to_string(), "{}".to_string());
        let err = standalone_mcp_from_labels(&labels).unwrap_err();
        assert!(matches!(err, OutrigError::Configuration(_)));
        assert!(err.to_string().contains("at least one server"));
    }

    #[test]
    fn standalone_mcp_valid_label_parses() {
        let mut labels = BTreeMap::new();
        labels.insert(
            LABEL_MCP.to_string(),
            r#"{"fs":["mcp-server-filesystem","/workspace"]}"#.to_string(),
        );
        let mcp = standalone_mcp_from_labels(&labels).expect("valid label parses");
        assert_eq!(
            mcp.keys().map(String::as_str).collect::<Vec<_>>(),
            vec!["fs"]
        );
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
    fn merge_mcp_with_source_tracks_origin_and_overrides() {
        let mut image = BTreeMap::new();
        image.insert("fs".to_string(), short(&["image-fs"]));
        image.insert("shell".to_string(), short(&["image-shell"]));

        let mut config = BTreeMap::new();
        config.insert("fs".to_string(), short(&["config-fs"]));
        config.insert("build".to_string(), short(&["config-build"]));

        let merged = merge_mcp_with_source(image, &config, McpDeclarationSource::LaunchSpec);

        assert_eq!(merged["fs"].spec, short(&["config-fs"]));
        assert_eq!(merged["fs"].source, McpDeclarationSource::LaunchSpec);
        assert_eq!(merged["shell"].spec, short(&["image-shell"]));
        assert_eq!(merged["shell"].source, McpDeclarationSource::ImageLabel);
        assert_eq!(merged["build"].spec, short(&["config-build"]));
        assert_eq!(merged["build"].source, McpDeclarationSource::LaunchSpec);
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

    #[test]
    fn standalone_image_toml_rejects_placement_keys() {
        let err = parse_standalone_image_toml(
            r#"
            [image]
            ref = "rust-dev"

            [mcp]
            fs = { command = ["mcp"], sidecar = "tools" }
            "#,
        )
        .unwrap_err();

        assert!(matches!(
            err,
            StandaloneImageTomlError::PlacementInLabel { server } if server == "fs"
        ));
    }
}
