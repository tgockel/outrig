//! Merge global + repo `Config` values. Repo entries replace global entries
//! with the same key (no per-key merging within an entry), except extra
//! workspace mounts which are concatenated in global-then-repo order.

use super::Config;

/// Merge `global` and `repo`, with `repo` winning on every collision.
///
/// - For each map (`providers`, `models`, `agents`, `containers`): repo entries
///   replace global entries with the same key. Entries unique to either side
///   are preserved as-is.
/// - For top-level scalars (`default-container`, `default-agent`,
///   `default-model`, `session-root`, `model-cache-root`,
///   `tool-call-cap`, `tool-result-cap`): repo's value wins if set,
///   else global's.
/// - `[workspace]` primary fields are repo-owned. Since `Workspace` has serde
///   defaults, an absent block in the repo file deserializes to those defaults
///   -- so taking repo `host-path`/`container-path` unconditionally matches
///   the documented "rare to set globally; repo still wins block-level" rule.
///   Extra `workspace.mounts` are concatenated so user-level resource mounts
///   and repo-level resource mounts both participate.
pub fn merge(global: Config, repo: Config) -> Config {
    let mut providers = global.providers;
    providers.extend(repo.providers);

    let mut models = global.models;
    models.extend(repo.models);

    let mut agents = global.agents;
    agents.extend(repo.agents);

    let mut containers = global.containers;
    containers.extend(repo.containers);

    let mut workspace = repo.workspace;
    let mut mounts = global.workspace.mounts;
    mounts.extend(workspace.mounts);
    workspace.mounts = mounts;

    Config {
        default_container: repo.default_container.or(global.default_container),
        default_agent: repo.default_agent.or(global.default_agent),
        default_model: repo.default_model.or(global.default_model),
        session_root: repo.session_root.or(global.session_root),
        model_cache_root: repo.model_cache_root.or(global.model_cache_root),
        tool_call_cap: repo.tool_call_cap.or(global.tool_call_cap),
        tool_result_cap: repo.tool_result_cap.or(global.tool_result_cap),
        providers,
        models,
        agents,
        workspace,
        containers,
    }
}
