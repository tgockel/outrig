//! Merge global + repo `Config` values. Repo entries replace global entries
//! with the same key (no per-key merging within an entry), except extra
//! workspace mounts which are concatenated in global-then-repo order.

use super::Config;

/// Merge `global` and `repo`, with `repo` winning on every collision.
///
/// - For each map (`providers`, `models`, `agents`, `images`, `sidecars`):
///   repo entries replace global entries with the same key. Entries unique to
///   either side are preserved as-is. A repo image-config can therefore name a
///   sidecar the user declared globally.
/// - For top-level scalars (`default-image`, `default-agent`,
///   `default-model`, `session-root`, `model-cache-root`,
///   `tool-call-max`, `tool-result-max`): repo's value wins if set,
///   else global's.
/// - `[network].mode` follows repo precedence when the repo file declares the
///   table. Policy keys (`default`, `allow`, `deny`) are global-only and are
///   always taken from the global config.
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

    let mut images = global.images;
    images.extend(repo.images);

    let mut sidecars = global.sidecars;
    sidecars.extend(repo.sidecars);

    let mut workspace = repo.workspace;
    let mut mounts = global.workspace.mounts;
    mounts.extend(workspace.mounts);
    workspace.mounts = mounts;

    let mut network = global.network;
    if repo.network.is_declared() {
        network.mode = repo.network.mode;
        network.set_declared(true);
    } else {
        network.set_declared(false);
    }

    Config {
        default_image: repo.default_image.or(global.default_image),
        default_agent: repo.default_agent.or(global.default_agent),
        default_model: repo.default_model.or(global.default_model),
        session_root: repo.session_root.or(global.session_root),
        model_cache_root: repo.model_cache_root.or(global.model_cache_root),
        tool_call_max: repo.tool_call_max.or(global.tool_call_max),
        tool_result_max: repo.tool_result_max.or(global.tool_result_max),
        subagent_max_depth: repo.subagent_max_depth.or(global.subagent_max_depth),
        network,
        providers,
        models,
        agents,
        workspace,
        images,
        sidecars,
    }
}
