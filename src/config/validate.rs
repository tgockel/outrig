//! Cross-reference validation for a merged `Config`. Mirrors the rules in
//! `doc/reference/config.md`'s "Validation rules" section.
//!
//! `api-key` syntax is enforced at parse time by `super::api_key`; this module
//! only checks cross-references, MCP server-name shape, and disk-existence of
//! container `dockerfile` / `context` paths.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use regex::Regex;
use thiserror::Error;

use super::{
    Config, ContainerConfig, LlmProvider, MAX_TOOL_CALL_CAP, MAX_TOOL_RESULT_CAP_BYTES,
    MIN_TOOL_RESULT_CAP_BYTES, McpServerSpec, Model,
};

#[derive(Debug, Error)]
pub enum ConfigValidationError {
    #[error("default-container {name:?} does not match any [containers.<name>]")]
    UnknownDefaultContainer { name: String },

    #[error("default-agent {name:?} does not match any [agents.<name>]")]
    UnknownDefaultAgent { name: String },

    #[error("default-model {name:?} does not match any [models.<name>]")]
    UnknownDefaultModel { name: String },

    #[error("agent {agent:?} has model={model:?} which does not match any [models.<name>]")]
    UnknownAgentModel { agent: String, model: String },

    #[error("agent {agent:?} omits 'model' and no top-level 'default-model' is set")]
    AgentMissingModel { agent: String },

    #[error(
        "agent {agent:?} has container={container:?} which does not match any \
         [containers.<name>]"
    )]
    UnknownAgentContainer { agent: String, container: String },

    #[error(
        "model {model:?} has provider={provider:?} which does not match any \
         [providers.<name>]"
    )]
    UnknownModelProvider { model: String, provider: String },

    #[error(
        "container {container:?} has invalid mcp server name {server:?} \
         (must match ^[a-zA-Z][a-zA-Z0-9_-]*$)"
    )]
    InvalidMcpServerName { container: String, server: String },

    #[error("container {container:?} mcp server {server:?} has empty command")]
    EmptyMcpCommand { container: String, server: String },

    #[error("container {container:?}: neither `image-name` nor `dockerfile`+`context` is set")]
    ContainerSourceMissing { container: String },

    #[error(
        "container {container:?}: conflicting fields {fields:?} -- set either `image-name` \
         or `dockerfile`+`context`, not both"
    )]
    ContainerSourceConflict {
        container: String,
        fields: Vec<&'static str>,
    },

    #[error("container {container:?}: `image-name` must not be empty")]
    ContainerImageNameEmpty { container: String },

    #[error("container {container:?}: `{missing}` is required when `{present}` is set")]
    ContainerHalfBuilt {
        container: String,
        present: &'static str,
        missing: &'static str,
    },

    #[error("container {container:?}: `build-args` cannot be used with `image-name`")]
    ContainerImageNameWithBuildArgs { container: String },

    #[error("container {container:?} dockerfile path {path:?} does not exist")]
    DockerfileMissing { container: String, path: PathBuf },

    #[error("container {container:?} context path {path:?} does not exist")]
    ContextMissing { container: String, path: PathBuf },

    #[error("session-root {path:?} must be an absolute path")]
    SessionRootNotAbsolute { path: PathBuf },

    #[error("model-cache-root {path:?} must be an absolute path")]
    ModelCacheRootNotAbsolute { path: PathBuf },

    #[error("workspace mount host-path {path:?} does not exist")]
    WorkspaceMountHostMissing { path: PathBuf },

    #[error("workspace mount host-path {path:?} is not a directory")]
    WorkspaceMountHostNotDirectory { path: PathBuf },

    #[error("workspace mount container-path {path:?} must be absolute")]
    WorkspaceMountContainerNotAbsolute { path: PathBuf },

    #[error("workspace mount container-path must not be /")]
    WorkspaceMountContainerRoot,

    #[error("workspace mount container-path {path:?} is declared more than once")]
    WorkspaceMountContainerDuplicate { path: PathBuf },

    #[error("{path} must be between 1 and {max}; got {value}")]
    ToolCallCapOutOfRange { path: String, value: u32, max: u32 },

    #[error("{path} must be at least {min} bytes; got {value}")]
    ToolResultCapTooSmall { path: String, value: u32, min: u32 },

    #[error("{path} must be at most {max} bytes; got {value}")]
    ToolResultCapTooLarge { path: String, value: u32, max: u32 },

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
        "model {model:?} (provider style=mistralrs) must not set {field:?} -- \
         that field belongs to openai-style providers"
    )]
    MistralrsModelHasOpenAiField { model: String, field: &'static str },

    #[error(
        "model {model:?} (provider style=openai) must set 'identifier' (the \
         string sent to the provider API)"
    )]
    OpenAiModelMissingIdentifier { model: String },

    #[error(
        "model {model:?} (provider style=openai) must not set {field:?} -- \
         that field belongs to mistralrs-style providers"
    )]
    OpenAiModelHasMistralrsField { model: String, field: &'static str },
}

pub(super) fn validate(
    cfg: &Config,
    repo_root: Option<&Path>,
) -> Result<(), ConfigValidationError> {
    validate_workspace_mounts(cfg, repo_root)?;

    if let Some(name) = &cfg.default_container
        && !cfg.containers.contains_key(name)
    {
        return Err(ConfigValidationError::UnknownDefaultContainer { name: name.clone() });
    }
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
                if cfg.default_model.is_none() {
                    return Err(ConfigValidationError::AgentMissingModel {
                        agent: agent_name.clone(),
                    });
                }
            }
        }
        if let Some(c) = &agent.container
            && !cfg.containers.contains_key(c)
        {
            return Err(ConfigValidationError::UnknownAgentContainer {
                agent: agent_name.clone(),
                container: c.clone(),
            });
        }
    }

    for (container_name, container) in &cfg.containers {
        validate_container_source(container_name, container, repo_root)?;

        for (server_name, spec) in &container.mcp {
            if !is_valid_mcp_server_name(server_name) {
                return Err(ConfigValidationError::InvalidMcpServerName {
                    container: container_name.clone(),
                    server: server_name.clone(),
                });
            }
            if mcp_command_is_empty(spec) {
                return Err(ConfigValidationError::EmptyMcpCommand {
                    container: container_name.clone(),
                    server: server_name.clone(),
                });
            }
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

    if let Some(value) = cfg.tool_call_cap {
        validate_tool_call_cap("top-level tool-call-cap", value)?;
    }
    if let Some(value) = cfg.tool_result_cap {
        validate_tool_result_cap("top-level tool-result-cap", value)?;
    }

    for (agent_name, agent) in &cfg.agents {
        if let Some(value) = agent.tool_call_cap {
            validate_tool_call_cap(&format!("agents.{agent_name}.tool-call-cap"), value)?;
        }
        if let Some(value) = agent.tool_result_cap {
            validate_tool_result_cap(&format!("agents.{agent_name}.tool-result-cap"), value)?;
        }
    }

    for (model_name, model) in &cfg.models {
        let provider = cfg.providers.get(&model.provider).ok_or_else(|| {
            ConfigValidationError::UnknownModelProvider {
                model: model_name.clone(),
                provider: model.provider.clone(),
            }
        })?;
        match provider {
            LlmProvider::OpenAi { .. } => validate_openai_model(model_name, model)?,
            LlmProvider::Mistralrs => validate_mistralrs_model(model_name, model, repo_root)?,
        }
    }

    Ok(())
}

fn validate_workspace_mounts(
    cfg: &Config,
    repo_root: Option<&Path>,
) -> Result<(), ConfigValidationError> {
    let mut container_paths = BTreeSet::new();
    container_paths.insert(cfg.workspace.container_path.clone());

    for mount in &cfg.workspace.mounts {
        if !mount.container_path.is_absolute() {
            return Err(ConfigValidationError::WorkspaceMountContainerNotAbsolute {
                path: mount.container_path.clone(),
            });
        }
        if mount.container_path == Path::new("/") {
            return Err(ConfigValidationError::WorkspaceMountContainerRoot);
        }
        if !container_paths.insert(mount.container_path.clone()) {
            return Err(ConfigValidationError::WorkspaceMountContainerDuplicate {
                path: mount.container_path.clone(),
            });
        }

        if let Some(root) = repo_root {
            let resolved = if mount.host_path.is_absolute() {
                mount.host_path.clone()
            } else {
                root.join(&mount.host_path)
            };
            if !resolved.exists() {
                return Err(ConfigValidationError::WorkspaceMountHostMissing {
                    path: mount.host_path.clone(),
                });
            }
            if !resolved.is_dir() {
                return Err(ConfigValidationError::WorkspaceMountHostNotDirectory {
                    path: mount.host_path.clone(),
                });
            }
        }
    }

    Ok(())
}

pub(crate) fn is_valid_mcp_server_name(server: &str) -> bool {
    mcp_server_name_re().is_match(server)
}

pub(crate) fn mcp_command_is_empty(spec: &McpServerSpec) -> bool {
    match spec {
        McpServerSpec::Short(cmd) => cmd.is_empty(),
        McpServerSpec::Full { command, .. } => command.is_empty(),
    }
}

fn validate_tool_call_cap(path: &str, value: u32) -> Result<(), ConfigValidationError> {
    if !(1..=MAX_TOOL_CALL_CAP).contains(&value) {
        return Err(ConfigValidationError::ToolCallCapOutOfRange {
            path: path.to_string(),
            value,
            max: MAX_TOOL_CALL_CAP,
        });
    }
    Ok(())
}

fn validate_tool_result_cap(path: &str, value: u32) -> Result<(), ConfigValidationError> {
    if value < MIN_TOOL_RESULT_CAP_BYTES {
        return Err(ConfigValidationError::ToolResultCapTooSmall {
            path: path.to_string(),
            value,
            min: MIN_TOOL_RESULT_CAP_BYTES,
        });
    }
    if value > MAX_TOOL_RESULT_CAP_BYTES {
        return Err(ConfigValidationError::ToolResultCapTooLarge {
            path: path.to_string(),
            value,
            max: MAX_TOOL_RESULT_CAP_BYTES,
        });
    }
    Ok(())
}

fn validate_openai_model(model_name: &str, model: &Model) -> Result<(), ConfigValidationError> {
    if model.identifier.is_none() {
        return Err(ConfigValidationError::OpenAiModelMissingIdentifier {
            model: model_name.to_string(),
        });
    }
    let weight_fields: [(bool, &'static str); 5] = [
        (model.model_id.is_some(), "model-id"),
        (model.model_path.is_some(), "model-path"),
        (model.model_file.is_some(), "model-file"),
        (model.revision.is_some(), "revision"),
        (model.context_length.is_some(), "context-length"),
    ];
    for (present, field) in weight_fields {
        if present {
            return Err(ConfigValidationError::OpenAiModelHasMistralrsField {
                model: model_name.to_string(),
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
        return Err(ConfigValidationError::MistralrsModelHasOpenAiField {
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

    if let Some(path) = model.model_path.as_deref()
        && let Some(root) = repo_root
    {
        let resolved = if path.is_absolute() {
            path.to_path_buf()
        } else {
            root.join(path)
        };
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

/// Validate the XOR constraint on container source fields: exactly one of
/// `image-name` or `dockerfile`+`context` must be set.
fn validate_container_source(
    container_name: &str,
    container: &ContainerConfig,
    repo_root: Option<&Path>,
) -> Result<(), ConfigValidationError> {
    let has_image_name = container.image_name.is_some();
    let has_dockerfile = container.dockerfile.is_some();
    let has_context = container.context.is_some();

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
            return Err(ConfigValidationError::ContainerSourceConflict {
                container: container_name.to_string(),
                fields: conflicts,
            });
        }
        if !container.build_args.is_empty() {
            return Err(ConfigValidationError::ContainerImageNameWithBuildArgs {
                container: container_name.to_string(),
            });
        }
        let name = container.image_name.as_deref().unwrap();
        if name.is_empty() {
            return Err(ConfigValidationError::ContainerImageNameEmpty {
                container: container_name.to_string(),
            });
        }
    } else {
        // Build path: require both dockerfile and context.
        match (has_dockerfile, has_context) {
            (false, false) => {
                return Err(ConfigValidationError::ContainerSourceMissing {
                    container: container_name.to_string(),
                });
            }
            (true, false) => {
                return Err(ConfigValidationError::ContainerHalfBuilt {
                    container: container_name.to_string(),
                    present: "dockerfile",
                    missing: "context",
                });
            }
            (false, true) => {
                return Err(ConfigValidationError::ContainerHalfBuilt {
                    container: container_name.to_string(),
                    present: "context",
                    missing: "dockerfile",
                });
            }
            (true, true) => {}
        }

        // On-disk existence checks for the build path.
        if let Some(root) = repo_root {
            let dockerfile = container.dockerfile.as_ref().unwrap();
            let df_path = root.join(dockerfile);
            if !df_path.exists() {
                return Err(ConfigValidationError::DockerfileMissing {
                    container: container_name.to_string(),
                    path: dockerfile.clone(),
                });
            }
            let context = container.context.as_ref().unwrap();
            let ctx_path = root.join(context);
            if !ctx_path.exists() {
                return Err(ConfigValidationError::ContextMissing {
                    container: container_name.to_string(),
                    path: context.clone(),
                });
            }
        }
    }

    Ok(())
}
