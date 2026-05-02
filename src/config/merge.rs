//! Merge global + repo `Config` values. Repo entries replace global entries
//! with the same key (no per-key merging within an entry).

use super::Config;

/// Merge `global` and `repo`, with `repo` winning on every collision.
///
/// - For each map (`providers`, `models`, `agents`, `containers`): repo entries
///   replace global entries with the same key. Entries unique to either side
///   are preserved as-is.
/// - For top-level scalars (`default-container`, `default-agent`,
///   `default-model`, `session-root`): repo's value wins if set, else global's.
/// - `[workspace]` is repo-only at the block level. Since `Workspace` has serde
///   defaults, an absent block in the repo file deserializes to those defaults
///   -- so taking `repo.workspace` unconditionally matches the documented
///   "rare to set globally; repo still wins block-level" rule.
pub fn merge(global: Config, repo: Config) -> Config {
    let mut providers = global.providers;
    providers.extend(repo.providers);

    let mut models = global.models;
    models.extend(repo.models);

    let mut agents = global.agents;
    agents.extend(repo.agents);

    let mut containers = global.containers;
    containers.extend(repo.containers);

    Config {
        default_container: repo.default_container.or(global.default_container),
        default_agent: repo.default_agent.or(global.default_agent),
        default_model: repo.default_model.or(global.default_model),
        session_root: repo.session_root.or(global.session_root),
        providers,
        models,
        agents,
        workspace: repo.workspace,
        containers,
    }
}
