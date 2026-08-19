//! Cross-reference validation for a merged `Config`. Mirrors the rules in
//! `doc/reference/config.md`'s "Validation rules" section.
//!
//! Two entry points, because two kinds of rule live here. [`validate`] runs on
//! the *merged* config and checks that it hangs together. [`validate_as_repo`]
//! runs on a single *unmerged* repo file and checks what that file was allowed
//! to say -- a question the merged value cannot answer, since the merge has by
//! then taken each global-only key from the global side.
//!
//! `api-key` syntax is enforced at parse time by `super::api_key`; this module
//! only checks cross-references, MCP server-name shape, and disk-existence of
//! image `dockerfile` / `context` paths.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use regex::Regex;
use thiserror::Error;

use super::{
    Config, ImageConfig, ImageSourceRef, LlmProvider, McpServerSpec, MistralrsDeviceSpec, Model,
    ModelSourceRef, NetworkMode, REQUEST_TIMEOUT_SECS_CEILING, RETRY_BUDGET_SECS_CEILING,
    SUBAGENT_DEPTH_MAX_CEILING, SUBAGENT_WIDTH_MAX_CEILING, TOOL_CALL_MAX_LIMIT,
    TOOL_RESULT_MAX_CEILING_BYTES, TOOL_RESULT_MAX_FLOOR_BYTES, normalize_capability_name,
};

/// Renders the trailing `(declared in <file>)` note, or nothing when the entry
/// records no source. Kept out of the `#[error]` strings so an unrecorded
/// source prints no clause rather than a `None`.
fn declared_in_clause(declared_in: &Option<PathBuf>) -> String {
    declared_in
        .as_ref()
        .map_or_else(String::new, |p| format!(" (declared in {p:?})"))
}

#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ConfigValidationError {
    #[error("default-image {name:?} does not match any [images.<name>]")]
    UnknownDefaultImage { name: String },

    #[error("default-agent {name:?} does not match any [agents.<name>]")]
    UnknownDefaultAgent { name: String },

    #[error("default-model {name:?} does not match any [models.<name>]")]
    UnknownDefaultModel { name: String },

    #[error("agent {agent:?} has model={model:?} which does not match any [models.<name>]")]
    UnknownAgentModel { agent: String, model: String },

    #[error("agent {agent:?} omits 'model' and no top-level 'default-model' is set")]
    AgentMissingModel { agent: String },

    #[error(
        "agent {agent:?} has image={image:?} which does not match any \
         [images.<name>]"
    )]
    UnknownAgentImage { agent: String, image: String },

    #[error(
        "model {model:?} has provider={provider:?} which does not match any \
         [providers.<name>]"
    )]
    UnknownModelProvider { model: String, provider: String },

    #[error(
        "image {image:?} has invalid mcp server name {server:?} \
         (must match ^[a-zA-Z][a-zA-Z0-9_-]*$)"
    )]
    InvalidMcpServerName { image: String, server: String },

    #[error(
        "image {image:?} declares mcp server {server:?}, but that name is reserved \
         for OutRig's built-in tools; its tools would collide with `{server}__*`"
    )]
    ReservedMcpServerName { image: String, server: String },

    #[error("image {image:?} mcp server {server:?} has empty command")]
    EmptyMcpCommand { image: String, server: String },

    #[error(
        "image {image:?}: a build image's name becomes its container image \
         repository, so it must match ^[a-z0-9]+([._-]+[a-z0-9]+)*$ \
         (lowercase alphanumeric, separated by `.`, `_`, or `-`)"
    )]
    BuildImageNameInvalid { image: String },

    #[error("image {image:?}: neither `image-name` nor `dockerfile`+`context` is set")]
    ImageSourceMissing { image: String },

    #[error(
        "image {image:?}: conflicting fields {fields:?} -- set either `image-name` \
         or `dockerfile`+`context`, not both"
    )]
    ImageSourceConflict {
        image: String,
        fields: Vec<&'static str>,
    },

    #[error("image {image:?}: `image-name` must not be empty")]
    ImageNameEmpty { image: String },

    #[error("image {image:?}: `{missing}` is required when `{present}` is set")]
    ImageHalfBuilt {
        image: String,
        present: &'static str,
        missing: &'static str,
    },

    #[error("image {image:?}: `build-args` cannot be used with `image-name`")]
    ImageNameWithBuildArgs { image: String },

    #[error("model {model:?}: neither `provider` nor `alias` is set")]
    #[non_exhaustive]
    ModelSourceMissing { model: String },

    #[error(
        "model {model:?}: conflicting fields {fields:?} -- set either `alias` \
         or `provider`, not both"
    )]
    #[non_exhaustive]
    ModelSourceConflict {
        model: String,
        fields: Vec<&'static str>,
    },

    #[error("model {model:?}: `alias` must name at least one model")]
    #[non_exhaustive]
    ModelAliasEmpty { model: String },

    #[error(
        "model {model:?} has alias target {target:?} which does not match any \
         [models.<name>]"
    )]
    #[non_exhaustive]
    UnknownModelAliasTarget { model: String, target: String },

    #[error("model alias cycle: {cycle}")]
    #[non_exhaustive]
    ModelAliasCycle { cycle: String },

    #[error(
        "model {model:?}: `alias` chain is more than {max} hops deep; an alias \
         graph this deep is a mistake rather than a configuration"
    )]
    #[non_exhaustive]
    ModelAliasTooDeep { model: String, max: usize },

    #[error(
        "image {image:?} dockerfile path {path:?} does not exist{}",
        declared_in_clause(declared_in)
    )]
    #[non_exhaustive]
    DockerfileMissing {
        image: String,
        path: PathBuf,
        /// The config file that declared `path`. A bare relative path reads as
        /// a repo problem even when it came from the global config. `None` for
        /// a hand-built config that never went through `Config::load`.
        declared_in: Option<PathBuf>,
    },

    #[error(
        "image {image:?} context path {path:?} does not exist{}",
        declared_in_clause(declared_in)
    )]
    #[non_exhaustive]
    ContextMissing {
        image: String,
        path: PathBuf,
        /// The config file that declared `path`, or `None` when unrecorded.
        declared_in: Option<PathBuf>,
    },

    #[error("session-root {path:?} must be an absolute path")]
    SessionRootNotAbsolute { path: PathBuf },

    #[error("model-cache-root {path:?} must be an absolute path")]
    ModelCacheRootNotAbsolute { path: PathBuf },

    // The five below restate `MountRuleViolation` in workspace terms, so their
    // `declared_in` means exactly what the variant it is mapped from means.
    #[error(
        "workspace mount host-path {path:?} does not exist{}",
        declared_in_clause(declared_in)
    )]
    #[non_exhaustive]
    WorkspaceMountHostMissing {
        path: PathBuf,
        declared_in: Option<PathBuf>,
    },

    #[error(
        "workspace mount host-path {path:?} is not a directory{}",
        declared_in_clause(declared_in)
    )]
    #[non_exhaustive]
    WorkspaceMountHostNotDirectory {
        path: PathBuf,
        declared_in: Option<PathBuf>,
    },

    #[error(
        "workspace mount container-path {path:?} must be absolute{}",
        declared_in_clause(declared_in)
    )]
    #[non_exhaustive]
    WorkspaceMountContainerNotAbsolute {
        path: PathBuf,
        declared_in: Option<PathBuf>,
    },

    #[error(
        "workspace mount container-path must not be /{}",
        declared_in_clause(declared_in)
    )]
    #[non_exhaustive]
    WorkspaceMountContainerRoot { declared_in: Option<PathBuf> },

    #[error(
        "workspace mount container-path {path:?} is declared more than once{}",
        declared_in_clause(declared_in)
    )]
    #[non_exhaustive]
    WorkspaceMountContainerDuplicate {
        path: PathBuf,
        declared_in: Option<PathBuf>,
    },

    #[error("image {image:?}: `{field}` capability name must not be empty")]
    CapabilityNameEmpty { image: String, field: &'static str },

    #[error(
        "image {image:?}: `{field}` capability {capability:?} must match \
         ^[A-Z0-9_]+$ after optional CAP_ stripping"
    )]
    CapabilityNameInvalid {
        image: String,
        field: &'static str,
        capability: String,
    },

    #[error("image {image:?}: `{field}` capability {capability:?} is declared more than once")]
    CapabilityNameDuplicate {
        image: String,
        field: &'static str,
        capability: String,
    },

    #[error(
        "image {image:?}: capability {capability:?} is listed in both `cap-drop` \
         and `cap-add`"
    )]
    CapabilityDropAddConflict { image: String, capability: String },

    #[error("image {image:?}: `devices` entry must not be empty")]
    DevicePathEmpty { image: String },

    #[error("image {image:?}: `devices` entry {device:?} must be an absolute path")]
    DevicePathRelative { image: String, device: String },

    #[error("image {image:?}: `devices` entry {device:?} is declared more than once")]
    DevicePathDuplicate { image: String, device: String },

    #[error("image {image:?}: `unmask` entry must not be empty")]
    UnmaskPathEmpty { image: String },

    #[error("image {image:?}: `unmask` entry {path:?} must be an absolute path or `ALL`")]
    UnmaskPathRelative { image: String, path: String },

    #[error(
        "image {image:?}: `unmask` entry {path:?} must not contain `:`; podman splits an \
         unmask value on it, so declare one path per entry"
    )]
    UnmaskPathListSeparator { image: String, path: String },

    #[error("image {image:?}: `unmask` entry {path:?} is declared more than once")]
    UnmaskPathDuplicate { image: String, path: String },

    #[error(
        "image {image:?}: `unmask` entry {path:?} must be spelled `ALL`; podman only lifts \
         the read-only paths for that exact casing, so {path:?} would leave `/sys/fs/cgroup` \
         read-only while looking like a full unmask"
    )]
    UnmaskAllNotCanonical { image: String, path: String },

    #[error(
        "image {image:?}: `unmask` must be exactly [\"ALL\"] when it lists `ALL`; podman \
         applies the full unmask only when `ALL` comes first, and every other entry is \
         redundant beside it"
    )]
    UnmaskAllNotAlone { image: String },

    #[error(
        "image {image:?}: `unmask` entry {path:?} {detail}; podman logs a pattern syntax \
         error and leaves the path masked instead of failing the launch"
    )]
    UnmaskPathBadGlob {
        image: String,
        path: String,
        detail: &'static str,
    },

    #[error("{path} must be between 1 and {max}; got {value}")]
    ToolCallMaxOutOfRange { path: String, value: u32, max: u32 },

    #[error("{path} must be between 1 and {max} (1 disables subagents); got {value}")]
    SubagentDepthMaxOutOfRange { path: String, value: u32, max: u32 },

    #[error("{path} must be between 1 and {max}; got {value}")]
    SubagentWidthMaxOutOfRange { path: String, value: u32, max: u32 },

    #[error("{path} must be at least {min} bytes; got {value}")]
    ToolResultMaxTooSmall { path: String, value: u32, min: u32 },

    #[error("{path} must be at most {max} bytes; got {value}")]
    ToolResultMaxTooLarge { path: String, value: u32, max: u32 },

    #[error("{path} must be at most {max} seconds (0 disables retries); got {value}")]
    RetryBudgetSecsTooLarge { path: String, value: u64, max: u64 },

    #[error("{path} must be between 1 and {max} seconds; got {value}")]
    RequestTimeoutSecsTooLarge { path: String, value: u64, max: u64 },

    #[error(
        "{path} must be between 1 and {max} seconds; got 0, which is an immediate \
         timeout rather than a disabled one -- every request would fail before \
         it could be answered"
    )]
    RequestTimeoutSecsZero { path: String, max: u64 },

    #[error("{message}")]
    NetworkPolicyInvalid { message: String },

    #[error(
        "repo config may set [network].mode only; [network].{key} belongs in \
         global config"
    )]
    RepoNetworkPolicy { key: &'static str },

    #[error(
        "model {model:?} (provider style=mistralrs) must set exactly one of \
         model-id or model-path; got neither"
    )]
    MistralrsMissingModelSource { model: String },

    #[error(
        "model {model:?} (provider style=mistralrs) must set exactly one of \
         model-id or model-path; got both"
    )]
    MistralrsBothModelSources { model: String },

    #[error(
        "model {model:?} (provider style=mistralrs) sets {field:?} which only \
         applies when model-id is set"
    )]
    MistralrsExtraFieldRequiresModelId { model: String, field: &'static str },

    #[error(
        "model {model:?} (provider style=mistralrs) sets model-id={model_id:?} \
         but no model-file; pick a specific GGUF filename inside the repo"
    )]
    MistralrsModelIdMissingFile { model: String, model_id: String },

    #[error("model {model:?} (provider style=mistralrs) model-path {path:?} does not exist")]
    MistralrsModelPathMissing { model: String, path: PathBuf },

    #[error(
        "model {model:?} (provider style=mistralrs) has invalid device {device:?}; \
         expected one of: cpu, cuda, cuda:N, metal"
    )]
    MistralrsDeviceInvalid { model: String, device: String },

    #[error(
        "model {model:?} (provider style=mistralrs) must not set {field:?} -- \
         that field belongs to remote providers (style=openai, style=anthropic)"
    )]
    MistralrsModelHasRemoteField { model: String, field: &'static str },

    #[error(
        "model {model:?} (provider style={style}) must set 'identifier' (the \
         string sent to the provider API)"
    )]
    #[non_exhaustive]
    RemoteModelMissingIdentifier { model: String, style: &'static str },

    #[error(
        "model {model:?} (provider style={style}) must not set {field:?} -- \
         that field belongs to mistralrs-style providers"
    )]
    #[non_exhaustive]
    RemoteModelHasMistralrsField {
        model: String,
        style: &'static str,
        field: &'static str,
    },

    #[error(
        "invalid sidecar name {sidecar:?} \
         (must match ^[A-Za-z0-9][A-Za-z0-9_-]*$ -- it embeds in container names)"
    )]
    SidecarNameInvalid { sidecar: String },

    #[error("sidecar {sidecar:?}: `image` must not be empty")]
    SidecarImageEmpty { sidecar: String },

    #[error("sidecar {sidecar:?} mount {violation}")]
    SidecarMount {
        sidecar: String,
        violation: MountRuleViolation,
    },

    #[error(
        "image {image:?} mcp server {server:?} sets both `sidecar` and `image`; \
         they are mutually exclusive"
    )]
    McpPlacementConflict { image: String, server: String },

    #[error(
        "image {image:?} mcp server {server:?} has sidecar={sidecar:?} which does not \
         match any [sidecars.<name>]"
    )]
    McpUnknownSidecar {
        image: String,
        server: String,
        sidecar: String,
    },

    #[error("image {image:?} mcp server {server:?}: `image` must not be empty")]
    McpInlineImageEmpty { image: String, server: String },

    #[error(
        "image {image:?} mcp server {server:?} sets both `command` and `args`; \
         `args` is for entrypoint-stdio servers, and `command` is already a \
         full argv"
    )]
    McpArgsWithCommand { image: String, server: String },

    #[error(
        "image {image:?} mcp server {server:?} sets `args` without `image` or \
         `sidecar`; a server in the primary container is exec-stdio and takes \
         its arguments in `command`"
    )]
    McpArgsWithoutPlacement { image: String, server: String },

    #[error(
        "image {image:?} mcp server {server:?} sets `args` and so does its \
         sidecar {sidecar:?}; declare the arguments in one place"
    )]
    McpArgsDeclaredTwice {
        image: String,
        server: String,
        sidecar: String,
    },

    #[error(
        "sidecar {sidecar:?} sets `args` but no [images.<name>.mcp] entry hosts \
         an entrypoint-stdio server in it; `args` is the argv of the image's \
         ENTRYPOINT, which runs only for an entry that omits `command`"
    )]
    SidecarArgsWithoutEntrypoint { sidecar: String },

    #[error(
        "image {image:?} sidecar {sidecar:?} hosts entrypoint-stdio server \
         {server:?} alongside {other:?}; the container process is the server, \
         so an entrypoint host serves exactly one"
    )]
    SidecarEntrypointNotAlone {
        image: String,
        sidecar: String,
        server: String,
        other: String,
    },

    #[error(
        "image {image:?} sidecar {sidecar:?} hosts entrypoint-stdio server \
         {server:?} but is start = \"manual\"; container lifetime is the \
         server's, so an entrypoint host must start with the session"
    )]
    SidecarEntrypointNotAuto {
        image: String,
        sidecar: String,
        server: String,
    },

    #[error(
        "sidecar {sidecar:?} sets view = \"primary\" and workspace access; the \
         primary's view already contains the workspace at its real path, so the \
         two are mutually exclusive"
    )]
    SidecarViewWorkspaceConflict { sidecar: String },

    #[error(
        "sidecar {sidecar:?} sets view = \"primary\" with capability-profile = \
         \"drop-all\"; joining the primary's namespaces needs CAP_SYS_ADMIN and \
         CAP_SYS_PTRACE, which drop-all removes"
    )]
    SidecarViewDropsCaps { sidecar: String },

    #[error(
        "image {image:?} sidecar {sidecar:?} sets view = \"primary\" but hosts \
         exec-stdio server {server:?}; the view runs the launcher as the \
         container ENTRYPOINT, so it is entrypoint-stdio only"
    )]
    SidecarViewRequiresEntrypoint {
        image: String,
        sidecar: String,
        server: String,
    },

    #[error(
        "image {image:?} mcp server {server:?} sets view = \"primary\" without \
         the inline entrypoint-stdio form (an `image` and no `command`); for a \
         named sidecar, set `view` on its [sidecars.<name>] block instead"
    )]
    McpViewNotInlineEntrypoint { image: String, server: String },

    #[error(
        "image {image:?}: sidecar name {name:?} collides with mcp server {name:?}, \
         which declares an anonymous sidecar via `image`; anonymous sidecars occupy \
         their server's name"
    )]
    SidecarNameCollision { image: String, name: String },
}

#[derive(Debug, Clone, Copy)]
pub(super) struct ValidationOptions<'a> {
    pub agent_model_override: Option<&'a str>,
    /// Whether to check the `[providers]` / `[models]` / `[agents]` blocks.
    /// `outrig build` sets this false: it resolves images and never opens an
    /// HTTP connection, so a config whose LLM half is broken still builds.
    ///
    /// The provider range checks -- `retry-budget-secs`,
    /// `request-timeout-secs` -- ride along inside this gate, so `outrig build`
    /// accepts values `outrig run` rejects. That is deliberate: the bounds
    /// exist to keep an interactive turn from wedging, and a build has no turn
    /// to wedge.
    pub validate_llm: bool,
}

impl Default for ValidationOptions<'_> {
    fn default() -> Self {
        Self {
            agent_model_override: None,
            validate_llm: true,
        }
    }
}

pub(super) fn validate(
    cfg: &Config,
    repo_root: Option<&Path>,
) -> Result<(), ConfigValidationError> {
    validate_with_options(cfg, repo_root, ValidationOptions::default())
}

pub(super) fn validate_with_options(
    cfg: &Config,
    repo_root: Option<&Path>,
    options: ValidationOptions<'_>,
) -> Result<(), ConfigValidationError> {
    validate_workspace_mounts(cfg, repo_root)?;

    if let Some(name) = &cfg.default_image
        && !cfg.images.contains_key(name)
    {
        return Err(ConfigValidationError::UnknownDefaultImage { name: name.clone() });
    }

    if options.validate_llm {
        if let Some(name) = &cfg.default_agent
            && !cfg.agents.contains_key(name)
        {
            return Err(ConfigValidationError::UnknownDefaultAgent { name: name.clone() });
        }
        if let Some(name) = &cfg.default_model
            && !cfg.models.contains_key(name)
        {
            return Err(ConfigValidationError::UnknownDefaultModel { name: name.clone() });
        }

        for (agent_name, agent) in &cfg.agents {
            match &agent.model {
                Some(m) => {
                    if !cfg.models.contains_key(m) {
                        return Err(ConfigValidationError::UnknownAgentModel {
                            agent: agent_name.clone(),
                            model: m.clone(),
                        });
                    }
                }
                None => {
                    // default-model existence already checked above; here we just
                    // need it to be set at all.
                    let has_run_model_override =
                        options.agent_model_override == Some(agent_name.as_str());
                    if cfg.default_model.is_none() && !has_run_model_override {
                        return Err(ConfigValidationError::AgentMissingModel {
                            agent: agent_name.clone(),
                        });
                    }
                }
            }
            if let Some(c) = &agent.image
                && !cfg.images.contains_key(c)
            {
                return Err(ConfigValidationError::UnknownAgentImage {
                    agent: agent_name.clone(),
                    image: c.clone(),
                });
            }
        }
    }

    // Sidecar blocks stand on their own, so they validate before anything
    // references them -- a malformed block is reported as such rather than as
    // a broken reference from whichever image-config happened to sort first.
    for (sidecar_name, sidecar) in &cfg.sidecars {
        validate_sidecar(cfg, repo_root, sidecar_name, sidecar)?;
        validate_sidecar_args_reachable(cfg, sidecar_name, sidecar)?;
    }

    for (image_name, image) in &cfg.images {
        validate_image_source(image_name, image, repo_root)?;
        // A build image's name becomes its container image repository, so it
        // must be a valid repository component. Image-name configs use the
        // `image-name` field as the tag, so their block key is just a label.
        // `validate_image_source` ran above, so `source()` won't panic here.
        if matches!(image.source(), ImageSourceRef::Build { .. })
            && !is_valid_build_image_name(image_name)
        {
            return Err(ConfigValidationError::BuildImageNameInvalid {
                image: image_name.clone(),
            });
        }
        validate_image_security(image_name, image)?;

        for (server_name, spec) in &image.mcp {
            if !is_valid_mcp_server_name(server_name) {
                return Err(ConfigValidationError::InvalidMcpServerName {
                    image: image_name.clone(),
                    server: server_name.clone(),
                });
            }
            if server_name == crate::RESERVED_SERVER {
                return Err(ConfigValidationError::ReservedMcpServerName {
                    image: image_name.clone(),
                    server: server_name.clone(),
                });
            }
            validate_mcp_placement(cfg, image_name, server_name, spec)?;
            if mcp_command_is_empty(spec) {
                return Err(ConfigValidationError::EmptyMcpCommand {
                    image: image_name.clone(),
                    server: server_name.clone(),
                });
            }
        }

        // Only the sidecars this image-config actually names: a declared block
        // it never references is inert here, and never starts. Every name has
        // resolved above, so the lookup cannot miss.
        for (_, sidecar_name) in image
            .mcp
            .iter()
            .filter_map(|(name, spec)| spec.sidecar().map(|sc| (name, sc)))
        {
            let sidecar = &cfg.sidecars[sidecar_name];
            validate_sidecar_hosting(image_name, image, sidecar_name, sidecar)?;
        }
    }

    if let Some(path) = &cfg.session_root
        && !path.is_absolute()
    {
        return Err(ConfigValidationError::SessionRootNotAbsolute { path: path.clone() });
    }

    if let Some(path) = &cfg.model_cache_root
        && !path.is_absolute()
    {
        return Err(ConfigValidationError::ModelCacheRootNotAbsolute { path: path.clone() });
    }

    if let Some(value) = cfg.tool_call_max {
        validate_tool_call_max("top-level tool-call-max", value)?;
    }
    if let Some(value) = cfg.tool_result_max {
        validate_tool_result_max("top-level tool-result-max", value)?;
    }
    if let Some(value) = cfg.subagent_depth_max {
        validate_subagent_depth_max("top-level subagent-depth-max", value)?;
    }
    if let Some(value) = cfg.subagent_width_max {
        validate_subagent_width_max("top-level subagent-width-max", value)?;
    }
    if let Some(value) = cfg.retry_budget_secs {
        validate_retry_budget_secs("top-level retry-budget-secs", value)?;
    }
    validate_network_policy(cfg)?;

    // Deliberately outside the `validate_llm` gate below, which `outrig build`
    // turns off, for two different reasons.
    //
    // `validate_model_source` is a shape rule rather than a cross-reference
    // one: it establishes the invariant `Model::source` panics on, and until
    // `provider` became optional serde's own "missing field" enforced half of
    // it on every path, `outrig build` included. Gating it would quietly let a
    // build accept `[models.x]` with nothing in it. The images loop above
    // treats `validate_image_source` exactly this way.
    //
    // `model_candidates` *is* a cross-reference check, but one that resolves
    // entirely within `[models]` -- an alias target is another row in this same
    // table, not a provider, an endpoint, or a credential. So it says nothing
    // about whether this build can reach an LLM, which is the only thing the
    // gate exists to skip. A build therefore still accepts a typo'd `provider`
    // while rejecting a typo'd alias target, which looks inconsistent and is
    // not: one is reachability, the other is internal consistency.
    for (model_name, model) in &cfg.models {
        validate_model_source(model_name, model)?;
    }
    // One pass for the whole table rather than a flatten per row: a subtree
    // shared by many names is walked once, which keeps an ordinary "many names,
    // one shared alias" config from costing cubic work at every load.
    cfg.validate_model_alias_graph()?;

    if options.validate_llm {
        for (provider_name, provider) in &cfg.providers {
            // Deliberately without a `_` arm, matching the model loop below: a
            // new provider style has to stop here and say whether it retries
            // and whether it speaks HTTP at all.
            let (retry_budget_secs, request_timeout_secs) = match provider {
                LlmProvider::OpenAi {
                    retry_budget_secs,
                    request_timeout_secs,
                    ..
                }
                | LlmProvider::Anthropic {
                    retry_budget_secs,
                    request_timeout_secs,
                    ..
                } => (*retry_budget_secs, *request_timeout_secs),
                // In-process: no HTTP layer, so nothing to retry and no
                // request to time out.
                LlmProvider::Mistralrs => (None, None),
            };
            if let Some(value) = retry_budget_secs {
                validate_retry_budget_secs(
                    &format!("providers.{provider_name}.retry-budget-secs"),
                    value,
                )?;
            }
            if let Some(value) = request_timeout_secs {
                validate_request_timeout_secs(
                    &format!("providers.{provider_name}.request-timeout-secs"),
                    value,
                )?;
            }
        }

        for (agent_name, agent) in &cfg.agents {
            if let Some(value) = agent.tool_call_max {
                validate_tool_call_max(&format!("agents.{agent_name}.tool-call-max"), value)?;
            }
            if let Some(value) = agent.tool_result_max {
                validate_tool_result_max(&format!("agents.{agent_name}.tool-result-max"), value)?;
            }
            if let Some(value) = agent.subagent_depth_max {
                validate_subagent_depth_max(
                    &format!("agents.{agent_name}.subagent-depth-max"),
                    value,
                )?;
            }
            if let Some(value) = agent.subagent_width_max {
                validate_subagent_width_max(
                    &format!("agents.{agent_name}.subagent-width-max"),
                    value,
                )?;
            }
        }

        for (model_name, model) in &cfg.models {
            // An alias has no provider of its own; its targets are rows in this
            // same loop and are checked on their own account. The shape pass
            // above ran ungated, so `source()` cannot panic here.
            let ModelSourceRef::Provider {
                provider: provider_name,
            } = model.source()
            else {
                continue;
            };
            let provider = cfg.providers.get(provider_name).ok_or_else(|| {
                ConfigValidationError::UnknownModelProvider {
                    model: model_name.clone(),
                    provider: provider_name.to_string(),
                }
            })?;
            // Deliberately without a `_` arm: this crate can match the enum
            // exhaustively even though it is `#[non_exhaustive]`, so a new
            // provider style has to stop here and say which field rules it
            // follows rather than inheriting someone else's by default.
            match provider {
                LlmProvider::OpenAi { .. } | LlmProvider::Anthropic { .. } => {
                    validate_remote_model(provider.style(), model_name, model)?
                }
                LlmProvider::Mistralrs => validate_mistralrs_model(model_name, model, repo_root)?,
            }
        }
    }

    Ok(())
}

/// The rules that apply to a repo config file, checked on the unmerged value.
///
/// Today there is one: `[network]`'s policy keys describe the machine's egress
/// and belong to the operator, so a repo config may declare `mode` and nothing
/// else. See [`Config::validate_as_repo`](super::Config::validate_as_repo).
pub(super) fn validate_as_repo(cfg: &Config) -> Result<(), ConfigValidationError> {
    match cfg.network.declared_policy_key() {
        Some(key) => Err(ConfigValidationError::RepoNetworkPolicy { key }),
        None => Ok(()),
    }
}

fn validate_network_policy(cfg: &Config) -> Result<(), ConfigValidationError> {
    cfg.network
        .policy()
        .validate(cfg.network.mode() == NetworkMode::Filter)
        .map_err(|message| ConfigValidationError::NetworkPolicyInvalid { message })
}

fn validate_image_security(
    image_name: &str,
    image: &ImageConfig,
) -> Result<(), ConfigValidationError> {
    validate_security(image_name, &image.security)
}

/// Validate a `[security]` block. `scope` names the owning block in errors --
/// the image-config name, or `<image>.sidecars.<sc>` for a sidecar block.
fn validate_security(
    scope: &str,
    security: &super::ContainerSecurity,
) -> Result<(), ConfigValidationError> {
    let drops = validate_capability_list(scope, "cap-drop", &security.cap_drop)?;
    let adds = validate_capability_list(scope, "cap-add", &security.cap_add)?;

    if let Some(capability) = drops.intersection(&adds).next() {
        return Err(ConfigValidationError::CapabilityDropAddConflict {
            image: scope.to_string(),
            capability: capability.clone(),
        });
    }

    validate_device_list(scope, &security.devices)?;
    validate_unmask_list(scope, &security.unmask)
}

/// Shape-check the `devices` list. Whether the node exists is deliberately not
/// checked: validation runs on machines that are not the launch host, and
/// podman's own error covers a missing node clearly enough.
fn validate_device_list(scope: &str, devices: &[String]) -> Result<(), ConfigValidationError> {
    let mut seen = BTreeSet::new();

    for device in devices {
        if device.trim().is_empty() {
            return Err(ConfigValidationError::DevicePathEmpty {
                image: scope.to_string(),
            });
        }
        if !Path::new(device).is_absolute() {
            return Err(ConfigValidationError::DevicePathRelative {
                image: scope.to_string(),
                device: device.clone(),
            });
        }
        if !seen.insert(device.clone()) {
            return Err(ConfigValidationError::DevicePathDuplicate {
                image: scope.to_string(),
                device: device.clone(),
            });
        }
    }

    Ok(())
}

/// Shape-check the `unmask` list. Entries reach podman verbatim, which is what
/// keeps `ALL` -- podman's "mask nothing" token rather than a path -- from
/// being something a caller who asked for `/proc/*` gets by accident. The two
/// rules that look pedantic are the ones measured against podman 5.7, where
/// both near-misses are silent:
///
/// - `ALL` only clears the *read-only* paths (`/sys/fs/cgroup` becomes
///   writable) when it is spelled in exact uppercase and comes first. Lowercase
///   `all` still clears the masked paths, so the container looks right while
///   cgroup stays read-only, and `["/proc/*", "ALL"]` does the same. Rather than
///   silently reordering a caller's list, `ALL` has to stand alone -- every
///   other entry is redundant beside it anyway.
/// - A malformed glob is a no-op, not an error: podman logs `syntax error in
///   pattern` and creates the container with the path still masked, so the
///   failure surfaces later as an unrelated-looking one.
fn validate_unmask_list(scope: &str, unmask: &[String]) -> Result<(), ConfigValidationError> {
    let mut seen = BTreeSet::new();

    for path in unmask {
        if path.trim().is_empty() {
            return Err(ConfigValidationError::UnmaskPathEmpty {
                image: scope.to_string(),
            });
        }
        // Checked before the absolute-path rule: a colon-joined list is made of
        // absolute paths, so the rule below would wave it through into an
        // argument podman then splits back into several.
        if path.contains(':') {
            return Err(ConfigValidationError::UnmaskPathListSeparator {
                image: scope.to_string(),
                path: path.clone(),
            });
        }
        if path.eq_ignore_ascii_case(UNMASK_ALL) {
            if path != UNMASK_ALL {
                return Err(ConfigValidationError::UnmaskAllNotCanonical {
                    image: scope.to_string(),
                    path: path.clone(),
                });
            }
            if unmask.len() > 1 {
                return Err(ConfigValidationError::UnmaskAllNotAlone {
                    image: scope.to_string(),
                });
            }
        } else {
            if !Path::new(path).is_absolute() {
                return Err(ConfigValidationError::UnmaskPathRelative {
                    image: scope.to_string(),
                    path: path.clone(),
                });
            }
            if let Err(detail) = check_glob_syntax(path) {
                return Err(ConfigValidationError::UnmaskPathBadGlob {
                    image: scope.to_string(),
                    path: path.clone(),
                    detail,
                });
            }
        }
        if !seen.insert(path.clone()) {
            return Err(ConfigValidationError::UnmaskPathDuplicate {
                image: scope.to_string(),
                path: path.clone(),
            });
        }
    }

    Ok(())
}

/// Podman's "mask nothing" token. Load-bearing in exactly this casing.
const UNMASK_ALL: &str = "ALL";

/// Reject the patterns Go's `filepath.Match` reports `ErrBadPattern` for, since
/// podman matches unmask entries with it and treats that error as a log line
/// rather than a failure. Deliberately a shape check against Go's documented
/// grammar rather than a port of `Match` itself: a pattern this accepts and Go
/// rejects is no worse than today, while every malformed form seen in practice
/// -- an unterminated class, a dangling escape -- is caught here where the
/// config is read instead of going quiet at launch.
fn check_glob_syntax(pattern: &str) -> Result<(), &'static str> {
    let mut chars = pattern.chars();

    while let Some(c) = chars.next() {
        match c {
            // Go's Match escapes the next character; a trailing `\` has none.
            '\\' => {
                chars.next().ok_or("ends with a dangling `\\` escape")?;
            }
            '[' => {
                // A class may open with a negation, then needs at least one
                // character before the `]` that closes it -- `[]` and `[^]`
                // are both `ErrBadPattern` in Go, not empty classes.
                let mut len = 0usize;
                let mut closed = false;
                let mut first = true;
                while let Some(c) = chars.next() {
                    if first {
                        first = false;
                        if c == '^' || c == '!' {
                            continue;
                        }
                    }
                    match c {
                        '\\' => {
                            if chars.next().is_none() {
                                return Err("ends with a dangling `\\` escape");
                            }
                            len += 1;
                        }
                        ']' if len > 0 => {
                            closed = true;
                            break;
                        }
                        _ => len += 1,
                    }
                }
                if !closed {
                    return Err("has an unterminated `[` character class");
                }
            }
            _ => {}
        }
    }

    Ok(())
}

/// Placement rules for one `[images.<name>.mcp]` entry: `sidecar`/`image`
/// mutual exclusion, a `sidecar` must name a declared `[sidecars.<sc>]` block,
/// an anonymous sidecar must not collide with one (it occupies its server's
/// name), and `args` belongs only to the entrypoint-stdio form. A placed entry
/// without a `command` *is* that form -- the container's ENTRYPOINT is the
/// server -- whether the container is an inline `image` or a named block.
fn validate_mcp_placement(
    cfg: &Config,
    image_name: &str,
    server_name: &str,
    spec: &McpServerSpec,
) -> Result<(), ConfigValidationError> {
    if spec.sidecar().is_some() && spec.image().is_some() {
        return Err(ConfigValidationError::McpPlacementConflict {
            image: image_name.to_string(),
            server: server_name.to_string(),
        });
    }
    if !spec.args().is_empty() {
        if spec.has_command() {
            return Err(ConfigValidationError::McpArgsWithCommand {
                image: image_name.to_string(),
                server: server_name.to_string(),
            });
        }
        if !spec.is_placed() {
            return Err(ConfigValidationError::McpArgsWithoutPlacement {
                image: image_name.to_string(),
                server: server_name.to_string(),
            });
        }
    }
    if let Some(sidecar) = spec.sidecar() {
        let Some(block) = cfg.sidecars.get(sidecar) else {
            return Err(ConfigValidationError::McpUnknownSidecar {
                image: image_name.to_string(),
                server: server_name.to_string(),
                sidecar: sidecar.to_string(),
            });
        };
        if !spec.args().is_empty() && !block.args.is_empty() {
            return Err(ConfigValidationError::McpArgsDeclaredTwice {
                image: image_name.to_string(),
                server: server_name.to_string(),
                sidecar: sidecar.to_string(),
            });
        }
    }
    if let Some(inline_image) = spec.image() {
        if inline_image.trim().is_empty() {
            return Err(ConfigValidationError::McpInlineImageEmpty {
                image: image_name.to_string(),
                server: server_name.to_string(),
            });
        }
        if cfg.sidecars.contains_key(server_name) {
            return Err(ConfigValidationError::SidecarNameCollision {
                image: image_name.to_string(),
                name: server_name.to_string(),
            });
        }
    }
    // An entry's own `view` configures the inline anonymous sidecar, so it is
    // meaningful only in the inline entrypoint-stdio form (an `image`, no
    // `command`). On a named-sidecar entry the block carries `view`; on a
    // commanded or unplaced entry there is nothing to view.
    if spec.view() != super::SidecarView::None && (spec.image().is_none() || spec.has_command()) {
        return Err(ConfigValidationError::McpViewNotInlineEntrypoint {
            image: image_name.to_string(),
            server: server_name.to_string(),
        });
    }
    Ok(())
}

/// The `[images.<name>.mcp]` entries hosted by `sidecar_name`, in name order.
fn servers_hosted_in<'a>(
    image: &'a ImageConfig,
    sidecar_name: &'a str,
) -> impl Iterator<Item = (&'a String, &'a McpServerSpec)> {
    image
        .mcp
        .iter()
        .filter(move |(_, spec)| spec.sidecar() == Some(sidecar_name))
}

/// Cross-check one image-config's use of a sidecar it references. An entry
/// that omits `command` makes the block an *entrypoint host* for this session:
/// the container process is the server, which bounds what else the block can
/// do -- it serves exactly one server, and it cannot be `start = "manual"`,
/// because `/sidecar add` starts a container and then execs into it.
///
/// Per image-config, not per block: a shared `[sidecars.<sc>]` may legitimately
/// be an entrypoint host for one image-config and an exec-stdio host for
/// another, since which it is follows from the referencing `[mcp]` entry.
fn validate_sidecar_hosting(
    image_name: &str,
    image: &ImageConfig,
    sidecar_name: &str,
    sidecar: &super::SidecarConfig,
) -> Result<(), ConfigValidationError> {
    let hosted: Vec<(&str, bool)> = servers_hosted_in(image, sidecar_name)
        .map(|(name, spec)| (name.as_str(), spec.is_entrypoint_stdio()))
        .collect();
    check_entrypoint_hosting(image_name, sidecar_name, sidecar.view, &hosted)?;

    // `start` has no library counterpart -- a `SidecarSpec` starts when the
    // caller adds it -- so this rule stays here rather than in the shared
    // check above.
    if sidecar.start == super::SidecarStart::Manual
        && let Some((server, _)) = hosted.iter().find(|(_, entrypoint)| *entrypoint)
    {
        return Err(ConfigValidationError::SidecarEntrypointNotAuto {
            image: image_name.to_string(),
            sidecar: sidecar_name.to_string(),
            server: (*server).to_string(),
        });
    }
    Ok(())
}

/// The placement rules that follow from *which* servers a sidecar hosts:
/// `view = "primary"` runs the `outrig-enter` launcher as the container
/// ENTRYPOINT, so every server must be entrypoint-stdio; and because the
/// container process is the server, an entrypoint host serves exactly one.
///
/// `hosted` is `(server name, is entrypoint-stdio)` in name order. Taking that
/// rather than an `ImageConfig` is what lets the library facade's hand-built
/// [`SidecarSpec`] run the identical checks -- the config path passes its
/// image-config name as `scope`, the library path its podman image ref.
///
/// [`SidecarSpec`]: crate::SidecarSpec
pub(crate) fn check_entrypoint_hosting(
    scope: &str,
    sidecar_name: &str,
    view: super::SidecarView,
    hosted: &[(&str, bool)],
) -> Result<(), ConfigValidationError> {
    if view == super::SidecarView::Primary
        && let Some((exec_server, _)) = hosted.iter().find(|(_, entrypoint)| !*entrypoint)
    {
        return Err(ConfigValidationError::SidecarViewRequiresEntrypoint {
            image: scope.to_string(),
            sidecar: sidecar_name.to_string(),
            server: (*exec_server).to_string(),
        });
    }

    let Some((server, _)) = hosted.iter().find(|(_, entrypoint)| *entrypoint) else {
        return Ok(());
    };
    if let Some((other, _)) = hosted.iter().find(|(name, _)| name != server) {
        return Err(ConfigValidationError::SidecarEntrypointNotAlone {
            image: scope.to_string(),
            sidecar: sidecar_name.to_string(),
            server: (*server).to_string(),
            other: (*other).to_string(),
        });
    }
    Ok(())
}

/// The `view = "primary"` exclusions that hold however a sidecar was declared.
/// Shared with the library facade so a hand-built [`SidecarSpec`] is rejected
/// by this code, with this text, rather than by a parallel copy of it.
///
/// [`SidecarSpec`]: crate::SidecarSpec
pub(crate) fn check_view_exclusions(
    sidecar_name: &str,
    view: super::SidecarView,
    workspace: super::SidecarWorkspaceAccess,
    capability_profile: super::CapabilityProfile,
) -> Result<(), ConfigValidationError> {
    if view != super::SidecarView::Primary {
        return Ok(());
    }
    // The primary's view already holds the workspace at its real path;
    // re-binding it over that is contradictory.
    if workspace != super::SidecarWorkspaceAccess::None {
        return Err(ConfigValidationError::SidecarViewWorkspaceConflict {
            sidecar: sidecar_name.to_string(),
        });
    }
    // The view needs the mount capabilities; drop-all removes them, so the
    // config is contradictory rather than silently re-added.
    if capability_profile == super::CapabilityProfile::DropAll {
        return Err(ConfigValidationError::SidecarViewDropsCaps {
            sidecar: sidecar_name.to_string(),
        });
    }
    Ok(())
}

/// A sidecar name embeds in container names, so it has to match
/// `^[A-Za-z0-9][A-Za-z0-9_-]*$` however it was declared.
pub(crate) fn check_sidecar_name(sidecar_name: &str) -> Result<(), ConfigValidationError> {
    if sidecar_name_re().is_match(sidecar_name) {
        return Ok(());
    }
    Err(ConfigValidationError::SidecarNameInvalid {
        sidecar: sidecar_name.to_string(),
    })
}

/// A sidecar's image ref must be non-empty. Deliberately not resolved here:
/// like `--image`, an unmatched name falls through to raw-podman-ref semantics
/// and fails at start time if the ref is absent locally.
pub(crate) fn check_sidecar_image(
    sidecar_name: &str,
    image: &str,
) -> Result<(), ConfigValidationError> {
    if !image.trim().is_empty() {
        return Ok(());
    }
    Err(ConfigValidationError::SidecarImageEmpty {
        sidecar: sidecar_name.to_string(),
    })
}

/// A block's `args` is its ENTRYPOINT's argv, so it needs *some* image-config
/// to host an entrypoint-stdio server in it. Checked across the whole config
/// rather than per image-config: a shared block used exec-stdio by one
/// image-config and entrypoint-stdio by another has perfectly live `args`,
/// inert only on the exec side.
fn validate_sidecar_args_reachable(
    cfg: &Config,
    sidecar_name: &str,
    sidecar: &super::SidecarConfig,
) -> Result<(), ConfigValidationError> {
    if sidecar.args.is_empty() {
        return Ok(());
    }
    let reachable = cfg.images.values().any(|image| {
        servers_hosted_in(image, sidecar_name).any(|(_, spec)| spec.is_entrypoint_stdio())
    });
    if reachable {
        return Ok(());
    }
    Err(ConfigValidationError::SidecarArgsWithoutEntrypoint {
        sidecar: sidecar_name.to_string(),
    })
}

/// Validate one `[sidecars.<sc>]` block on its own terms: name shape,
/// non-empty image ref, security caps, and its mount list. The sidecar's
/// `image` value is deliberately *not* cross-checked against `[images.<name>]`
/// blocks -- like `--image`, an unmatched name falls through to raw-podman-ref
/// semantics and fails at start time if the ref is absent locally.
fn validate_sidecar(
    cfg: &Config,
    repo_root: Option<&Path>,
    sidecar_name: &str,
    sidecar: &super::SidecarConfig,
) -> Result<(), ConfigValidationError> {
    check_sidecar_name(sidecar_name)?;
    check_sidecar_image(sidecar_name, &sidecar.image)?;

    let scope = format!("sidecars.{sidecar_name}");
    validate_security(&scope, &sidecar.security)?;

    check_view_exclusions(
        sidecar_name,
        sidecar.view,
        sidecar.workspace,
        sidecar.security.capability_profile,
    )?;

    // The workspace mount (when enabled) reuses the session's container path,
    // so extra mounts must not collide with it.
    let mut reserved = BTreeSet::new();
    if sidecar.workspace != super::SidecarWorkspaceAccess::None {
        reserved.insert(cfg.workspace.container_path().to_path_buf());
    }
    check_mount_list(&sidecar.mounts, reserved, repo_root).map_err(|violation| {
        ConfigValidationError::SidecarMount {
            sidecar: sidecar_name.to_string(),
            violation,
        }
    })
}

fn validate_capability_list(
    image_name: &str,
    field: &'static str,
    capabilities: &[String],
) -> Result<BTreeSet<String>, ConfigValidationError> {
    let mut seen = BTreeSet::new();

    for capability in capabilities {
        let Some(normalized) = normalize_capability_name(capability) else {
            if super::capability_name_without_prefix(capability).is_empty() {
                return Err(ConfigValidationError::CapabilityNameEmpty {
                    image: image_name.to_string(),
                    field,
                });
            }
            return Err(ConfigValidationError::CapabilityNameInvalid {
                image: image_name.to_string(),
                field,
                capability: capability.clone(),
            });
        };

        if !seen.insert(normalized.clone()) {
            return Err(ConfigValidationError::CapabilityNameDuplicate {
                image: image_name.to_string(),
                field,
                capability: normalized,
            });
        }
    }

    Ok(seen)
}

pub(super) fn validate_workspace_mounts(
    cfg: &Config,
    repo_root: Option<&Path>,
) -> Result<(), ConfigValidationError> {
    let mut reserved = BTreeSet::new();
    reserved.insert(cfg.workspace.container_path().to_path_buf());

    check_mount_list(&cfg.workspace.mounts, reserved, repo_root).map_err(
        |violation| match violation {
            MountRuleViolation::ContainerNotAbsolute { path, declared_in } => {
                ConfigValidationError::WorkspaceMountContainerNotAbsolute { path, declared_in }
            }
            MountRuleViolation::ContainerRoot { declared_in } => {
                ConfigValidationError::WorkspaceMountContainerRoot { declared_in }
            }
            MountRuleViolation::ContainerDuplicate { path, declared_in } => {
                ConfigValidationError::WorkspaceMountContainerDuplicate { path, declared_in }
            }
            MountRuleViolation::HostMissing { path, declared_in } => {
                ConfigValidationError::WorkspaceMountHostMissing { path, declared_in }
            }
            MountRuleViolation::HostNotDirectory { path, declared_in } => {
                ConfigValidationError::WorkspaceMountHostNotDirectory { path, declared_in }
            }
        },
    )
}

/// Scope-agnostic mount-list rule violation. The workspace caller maps it
/// onto its pre-existing per-rule variants; sidecar (and future) callers
/// wrap it whole and render via `Display`.
///
/// Every variant names the file that declared the offending entry, because
/// [`merge`] *concatenates* the global and repo mount lists: a bare path in the
/// message is ambiguous between two files even when the rule broken has nothing
/// to do with path resolution.
///
/// [`merge`]: super::merge
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum MountRuleViolation {
    #[error(
        "container-path {path:?} must be absolute{}",
        declared_in_clause(declared_in)
    )]
    #[non_exhaustive]
    ContainerNotAbsolute {
        path: PathBuf,
        /// The config file that declared this mount, or `None` for a
        /// hand-built entry -- see [`MountConfig::config_source`], the public
        /// accessor for the same provenance. Every variant below carries this
        /// field with the same meaning.
        ///
        /// [`MountConfig::config_source`]: super::MountConfig::config_source
        declared_in: Option<PathBuf>,
    },

    #[error("container-path must not be /{}", declared_in_clause(declared_in))]
    #[non_exhaustive]
    ContainerRoot {
        /// The one handle this rule offers: the message carries no path at all.
        declared_in: Option<PathBuf>,
    },

    #[error(
        "container-path {path:?} is declared more than once{}",
        declared_in_clause(declared_in)
    )]
    #[non_exhaustive]
    ContainerDuplicate {
        path: PathBuf,
        /// The file that declared the *rejected* entry -- the later of the two,
        /// which is the one to edit. The path it collides with may have come
        /// from the other file, or from the block's own reserved set; neither is
        /// tracked, so this deliberately does not claim to name both sides.
        declared_in: Option<PathBuf>,
    },

    #[error("host-path {path:?} does not exist{}", declared_in_clause(declared_in))]
    #[non_exhaustive]
    HostMissing {
        path: PathBuf,
        declared_in: Option<PathBuf>,
    },

    #[error(
        "host-path {path:?} is not a directory{}",
        declared_in_clause(declared_in)
    )]
    #[non_exhaustive]
    HostNotDirectory {
        path: PathBuf,
        declared_in: Option<PathBuf>,
    },
}

/// Shared rules for any bind-mount list: absolute container paths, never `/`,
/// no duplicates (including against `reserved` paths already claimed by the
/// owning block), and -- when a repo root is known -- host paths that exist
/// and are directories.
fn check_mount_list(
    mounts: &[super::MountConfig],
    mut reserved: BTreeSet<PathBuf>,
    repo_root: Option<&Path>,
) -> Result<(), MountRuleViolation> {
    for mount in mounts {
        // Per entry, not per list: the list may be a concatenation of two files.
        let declared_in = mount.declared_in();

        if !mount.container_path.is_absolute() {
            return Err(MountRuleViolation::ContainerNotAbsolute {
                path: mount.container_path.clone(),
                declared_in,
            });
        }
        if mount.container_path == Path::new("/") {
            return Err(MountRuleViolation::ContainerRoot { declared_in });
        }
        if !reserved.insert(mount.container_path.clone()) {
            return Err(MountRuleViolation::ContainerDuplicate {
                path: mount.container_path.clone(),
                declared_in,
            });
        }

        if let Some(root) = repo_root {
            let resolved = mount.resolved_host_path(root);
            if !resolved.exists() {
                return Err(MountRuleViolation::HostMissing {
                    path: mount.host_path.clone(),
                    declared_in,
                });
            }
            if !resolved.is_dir() {
                return Err(MountRuleViolation::HostNotDirectory {
                    path: mount.host_path.clone(),
                    declared_in,
                });
            }
        }
    }

    Ok(())
}

pub(crate) fn is_valid_mcp_server_name(server: &str) -> bool {
    mcp_server_name_re().is_match(server)
}

/// A build image's config name becomes the repository of its container image
/// tag (`<name>:<hash>`), so it must be a valid lowercase repository component.
pub(crate) fn is_valid_build_image_name(name: &str) -> bool {
    build_image_name_re().is_match(name)
}

pub(crate) fn mcp_command_is_empty(spec: &McpServerSpec) -> bool {
    match spec {
        McpServerSpec::Short(cmd) => cmd.is_empty(),
        McpServerSpec::Full {
            command: Some(cmd), ..
        } => cmd.is_empty(),
        // The no-command form is entrypoint-stdio when the entry names a
        // container -- inline `image` or a named `sidecar` -- whose ENTRYPOINT
        // is then the server. Naming neither leaves nothing to run.
        McpServerSpec::Full { command: None, .. } => !spec.is_placed(),
    }
}

fn validate_tool_call_max(path: &str, value: u32) -> Result<(), ConfigValidationError> {
    if !(1..=TOOL_CALL_MAX_LIMIT).contains(&value) {
        return Err(ConfigValidationError::ToolCallMaxOutOfRange {
            path: path.to_string(),
            value,
            max: TOOL_CALL_MAX_LIMIT,
        });
    }
    Ok(())
}

fn validate_subagent_depth_max(path: &str, value: u32) -> Result<(), ConfigValidationError> {
    if !(1..=SUBAGENT_DEPTH_MAX_CEILING).contains(&value) {
        return Err(ConfigValidationError::SubagentDepthMaxOutOfRange {
            path: path.to_string(),
            value,
            max: SUBAGENT_DEPTH_MAX_CEILING,
        });
    }
    Ok(())
}

fn validate_subagent_width_max(path: &str, value: u32) -> Result<(), ConfigValidationError> {
    if !(1..=SUBAGENT_WIDTH_MAX_CEILING).contains(&value) {
        return Err(ConfigValidationError::SubagentWidthMaxOutOfRange {
            path: path.to_string(),
            value,
            max: SUBAGENT_WIDTH_MAX_CEILING,
        });
    }
    Ok(())
}

/// One-sided, unlike its siblings: `0` is a meaningful value (retries off), so
/// there is no floor to enforce -- only a ceiling, because an absurd budget
/// wedges an interactive turn for as long as it names.
fn validate_retry_budget_secs(path: &str, value: u64) -> Result<(), ConfigValidationError> {
    if value > RETRY_BUDGET_SECS_CEILING {
        return Err(ConfigValidationError::RetryBudgetSecsTooLarge {
            path: path.to_string(),
            value,
            max: RETRY_BUDGET_SECS_CEILING,
        });
    }
    Ok(())
}

/// Two-sided, unlike the budget above: this bounds one attempt rather than
/// counting them, so `0` is not "no wait" but an immediate timeout -- reqwest
/// treats `Duration::ZERO` as a deadline already past, failing every request
/// before it can be answered. Both ends reject a config that parses and then
/// cannot work.
fn validate_request_timeout_secs(path: &str, value: u64) -> Result<(), ConfigValidationError> {
    if value == 0 {
        return Err(ConfigValidationError::RequestTimeoutSecsZero {
            path: path.to_string(),
            max: REQUEST_TIMEOUT_SECS_CEILING,
        });
    }
    if value > REQUEST_TIMEOUT_SECS_CEILING {
        return Err(ConfigValidationError::RequestTimeoutSecsTooLarge {
            path: path.to_string(),
            value,
            max: REQUEST_TIMEOUT_SECS_CEILING,
        });
    }
    Ok(())
}

fn validate_tool_result_max(path: &str, value: u32) -> Result<(), ConfigValidationError> {
    if value < TOOL_RESULT_MAX_FLOOR_BYTES {
        return Err(ConfigValidationError::ToolResultMaxTooSmall {
            path: path.to_string(),
            value,
            min: TOOL_RESULT_MAX_FLOOR_BYTES,
        });
    }
    if value > TOOL_RESULT_MAX_CEILING_BYTES {
        return Err(ConfigValidationError::ToolResultMaxTooLarge {
            path: path.to_string(),
            value,
            max: TOOL_RESULT_MAX_CEILING_BYTES,
        });
    }
    Ok(())
}

/// Validate the XOR constraint on model source fields: a row names either a
/// `provider` that serves it or an `alias` naming other models, never both and
/// never neither. The counterpart of [`validate_image_source`], and the check
/// that makes [`Model::source`] safe to call.
///
/// Every provider-shape field counts as a conflict, `max-tokens` included. An
/// alias carrying a ceiling for whichever candidate wins is coherent and may be
/// allowed later; "an alias has no provider-shape fields" is a rule with one
/// clause, while the same rule with an exception in it is two. Relaxing later
/// is additive; tightening later is not.
fn validate_model_source(model_name: &str, model: &Model) -> Result<(), ConfigValidationError> {
    if model.alias.is_some() {
        // The same collector the alias walk reports from, so a row rejected
        // here names exactly the fields it would name there.
        let mut conflicts: Vec<&'static str> = vec!["alias"];
        conflicts.extend(model.provider_shape_fields());
        if conflicts.len() > 1 {
            return Err(ConfigValidationError::ModelSourceConflict {
                model: model_name.to_string(),
                fields: conflicts,
            });
        }
        // Emptiness, dangling targets, and cycles are graph properties rather
        // than per-row ones, so `Config::model_candidates` owns them.
        return Ok(());
    }

    if model.provider.is_none() {
        return Err(ConfigValidationError::ModelSourceMissing {
            model: model_name.to_string(),
        });
    }
    Ok(())
}

/// The field rules every remote (HTTP) provider style shares: an `identifier`
/// is required, and every mistralrs weight field is rejected. `style` names the
/// style in diagnostics, so the message points at the row the user wrote rather
/// than at whichever remote provider happens to be listed first.
fn validate_remote_model(
    style: &'static str,
    model_name: &str,
    model: &Model,
) -> Result<(), ConfigValidationError> {
    if model.identifier.is_none() {
        return Err(ConfigValidationError::RemoteModelMissingIdentifier {
            model: model_name.to_string(),
            style,
        });
    }
    for (present, field) in model.mistralrs_weight_fields() {
        if present {
            return Err(ConfigValidationError::RemoteModelHasMistralrsField {
                model: model_name.to_string(),
                style,
                field,
            });
        }
    }
    Ok(())
}

fn validate_mistralrs_model(
    model_name: &str,
    model: &Model,
    repo_root: Option<&Path>,
) -> Result<(), ConfigValidationError> {
    if model.identifier.is_some() {
        return Err(ConfigValidationError::MistralrsModelHasRemoteField {
            model: model_name.to_string(),
            field: "identifier",
        });
    }
    match (model.model_id.is_some(), model.model_path.is_some()) {
        (false, false) => {
            return Err(ConfigValidationError::MistralrsMissingModelSource {
                model: model_name.to_string(),
            });
        }
        (true, true) => {
            return Err(ConfigValidationError::MistralrsBothModelSources {
                model: model_name.to_string(),
            });
        }
        _ => {}
    }

    if let Some(id) = model.model_id.as_deref()
        && model.model_file.as_ref().is_none_or(|v| v.is_empty())
    {
        return Err(ConfigValidationError::MistralrsModelIdMissingFile {
            model: model_name.to_string(),
            model_id: id.to_string(),
        });
    }

    if model.model_id.is_none() {
        let extras: [(bool, &'static str); 2] = [
            (
                model.model_file.as_ref().is_some_and(|v| !v.is_empty()),
                "model-file",
            ),
            (model.revision.is_some(), "revision"),
        ];
        for (present, field) in extras {
            if present {
                return Err(ConfigValidationError::MistralrsExtraFieldRequiresModelId {
                    model: model_name.to_string(),
                    field,
                });
            }
        }
    }

    if let Some(device) = model.device.as_deref()
        && device.parse::<MistralrsDeviceSpec>().is_err()
    {
        return Err(ConfigValidationError::MistralrsDeviceInvalid {
            model: model_name.to_string(),
            device: device.to_string(),
        });
    }

    if let Some(path) = model.model_path.as_deref()
        && let Some(root) = repo_root
    {
        // Still repo-root-relative: `models` was left out of the provenance
        // sweep, and this validated base disagrees with the unjoined path the
        // loader opens -- see `plan/next/model-path-runtime-unjoined.md`.
        let resolved = super::resolve_against(root, path);
        if !resolved.exists() {
            return Err(ConfigValidationError::MistralrsModelPathMissing {
                model: model_name.to_string(),
                path: path.to_path_buf(),
            });
        }
    }

    Ok(())
}

fn mcp_server_name_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"^[a-zA-Z][a-zA-Z0-9_-]*$").expect("mcp server-name regex compiles")
    })
}

/// Sidecar names embed in container names (`outrig-<sid>-<sc>`), so they may
/// start with a digit but are otherwise the server-name alphabet.
fn sidecar_name_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"^[A-Za-z0-9][A-Za-z0-9_-]*$").expect("sidecar-name regex compiles")
    })
}

fn build_image_name_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"^[a-z0-9]+([._-]+[a-z0-9]+)*$").expect("build image-name regex compiles")
    })
}

/// Validate the XOR constraint on image source fields: exactly one of
/// `image-name` or `dockerfile`+`context` must be set.
fn validate_image_source(
    image_name: &str,
    image: &ImageConfig,
    repo_root: Option<&Path>,
) -> Result<(), ConfigValidationError> {
    let has_image_name = image.image_name.is_some();
    let has_dockerfile = image.dockerfile.is_some();
    let has_context = image.context.is_some();

    if has_image_name {
        // image-name path: reject any build-path fields.
        let mut conflicts: Vec<&'static str> = vec!["image-name"];
        if has_dockerfile {
            conflicts.push("dockerfile");
        }
        if has_context {
            conflicts.push("context");
        }
        if conflicts.len() > 1 {
            return Err(ConfigValidationError::ImageSourceConflict {
                image: image_name.to_string(),
                fields: conflicts,
            });
        }
        if !image.build_args.is_empty() {
            return Err(ConfigValidationError::ImageNameWithBuildArgs {
                image: image_name.to_string(),
            });
        }
        let name = image.image_name.as_deref().unwrap();
        if name.is_empty() {
            return Err(ConfigValidationError::ImageNameEmpty {
                image: image_name.to_string(),
            });
        }
    } else {
        // Build path: require both dockerfile and context.
        match (has_dockerfile, has_context) {
            (false, false) => {
                return Err(ConfigValidationError::ImageSourceMissing {
                    image: image_name.to_string(),
                });
            }
            (true, false) => {
                return Err(ConfigValidationError::ImageHalfBuilt {
                    image: image_name.to_string(),
                    present: "dockerfile",
                    missing: "context",
                });
            }
            (false, true) => {
                return Err(ConfigValidationError::ImageHalfBuilt {
                    image: image_name.to_string(),
                    present: "context",
                    missing: "dockerfile",
                });
            }
            (true, true) => {}
        }

        // On-disk existence checks for the build path. Both paths resolve
        // against the directory of the file that declared them, so a global
        // image-config is checked where it actually lives; `path` stays the
        // raw config value and `declared_in` says which file to go edit.
        if let Some(root) = repo_root {
            let (df_path, ctx_path) = image.resolved_build_paths(root);
            if !df_path.exists() {
                return Err(ConfigValidationError::DockerfileMissing {
                    image: image_name.to_string(),
                    path: image.dockerfile.clone().unwrap(),
                    declared_in: image.declared_in(),
                });
            }
            if !ctx_path.exists() {
                return Err(ConfigValidationError::ContextMissing {
                    image: image_name.to_string(),
                    path: image.context.clone().unwrap(),
                    declared_in: image.declared_in(),
                });
            }
        }
    }

    Ok(())
}
