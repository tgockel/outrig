//! Cross-reference validation for a merged `Config`. Mirrors the rules in
//! `doc/reference/config.md`'s "Validation rules" section.
//!
//! `api-key` syntax is enforced at parse time by `super::api_key`; this module
//! only checks cross-references, MCP server-name shape, and disk-existence of
//! container `dockerfile` / `context` paths.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use regex::Regex;
use thiserror::Error;

use super::{Config, McpServerSpec};

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

    #[error("container {container:?} dockerfile path {path:?} does not exist")]
    DockerfileMissing { container: String, path: PathBuf },

    #[error("container {container:?} context path {path:?} does not exist")]
    ContextMissing { container: String, path: PathBuf },

    #[error("session-root {path:?} must be an absolute path")]
    SessionRootNotAbsolute { path: PathBuf },
}

pub(super) fn validate(
    cfg: &Config,
    repo_root: Option<&Path>,
) -> Result<(), ConfigValidationError> {
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

    for (model_name, model) in &cfg.models {
        if !cfg.providers.contains_key(&model.provider) {
            return Err(ConfigValidationError::UnknownModelProvider {
                model: model_name.clone(),
                provider: model.provider.clone(),
            });
        }
    }

    let server_name_re = mcp_server_name_re();
    for (container_name, container) in &cfg.containers {
        for (server_name, spec) in &container.mcp {
            if !server_name_re.is_match(server_name) {
                return Err(ConfigValidationError::InvalidMcpServerName {
                    container: container_name.clone(),
                    server: server_name.clone(),
                });
            }
            let command_empty = match spec {
                McpServerSpec::Short(cmd) => cmd.is_empty(),
                McpServerSpec::Full { command, .. } => command.is_empty(),
            };
            if command_empty {
                return Err(ConfigValidationError::EmptyMcpCommand {
                    container: container_name.clone(),
                    server: server_name.clone(),
                });
            }
        }

        if let Some(root) = repo_root {
            let dockerfile = root.join(&container.dockerfile);
            if !dockerfile.exists() {
                return Err(ConfigValidationError::DockerfileMissing {
                    container: container_name.clone(),
                    path: container.dockerfile.clone(),
                });
            }
            let context = root.join(&container.context);
            if !context.exists() {
                return Err(ConfigValidationError::ContextMissing {
                    container: container_name.clone(),
                    path: container.context.clone(),
                });
            }
        }
    }

    if let Some(path) = &cfg.session_root
        && !path.is_absolute()
    {
        return Err(ConfigValidationError::SessionRootNotAbsolute { path: path.clone() });
    }

    Ok(())
}

fn mcp_server_name_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"^[a-zA-Z][a-zA-Z0-9_-]*$").expect("mcp server-name regex compiles")
    })
}
