//! Merge global + repo `Config` values. Repo entries replace global entries
//! with the same key (no per-key merging within an entry), except extra
//! workspace mounts which are concatenated in global-then-repo order.

use super::Config;

/// Merge `global` and `repo`, with `repo` winning on every collision.
///
/// - For each map (`providers`, `models`, `agents`, `images`, `sidecars`):
///   repo entries replace global entries with the same key. Entries unique to
///   either side are preserved as-is. A repo image-config can therefore name a
///   sidecar the user declared globally. Which providers a repo file may
///   declare is not this function's question: [`Config::validate_as_repo`]
///   refuses a repo provider that carries an `api-key` before the file gets
///   here, and a repo value built by hand must be held to it the same way.
/// - For top-level scalars (`default-image`, `default-agent`,
///   `default-model`, `tool-call-max`, `tool-result-max`,
///   `subagent-depth-max`, `subagent-width-max`, `retry-budget-secs`,
///   `mcp-call-timeout-secs`):
///   repo's value wins if set, else global's.
/// - `session-root` and `model-cache-root` are global-only: they are taken
///   from `global` and never read from `repo`, so a repo value cannot move
///   where this machine writes, whatever built it.
/// - `[network].mode` follows repo precedence when the repo config declares
///   `mode`; a `[network]` table that declares no mode inherits the global
///   one. Policy keys (`default`, `allow`, `deny`) are global-only: this
///   function never reads them from the repo side, so a repo value carrying
///   its own policy cannot widen or inject one, whatever built it.
/// - `[workspace]` primary fields merge per key, like the scalars above: a
///   repo declaration wins, otherwise a global one is inherited, otherwise the
///   key stays `None` and [`Workspace`](super::Workspace)'s accessors apply
///   the built-in default. Extra `workspace.mounts` are concatenated so
///   user-level resource mounts and repo-level resource mounts both
///   participate.
/// - [`warnings`](Config::warnings) are concatenated, global first, so
///   whatever either load set aside is still reported.
///
/// The result is a flattened snapshot, not a config file: provenance rides
/// along in memory but is `#[serde(skip)]`, so re-serializing a merged config
/// emits each inherited path as the text its source file used and loses the
/// base directory that text meant. The two roots are the exception: they were
/// resolved as they were read, so they come out absolute. Nothing in outrig
/// writes a merged config back to disk.
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
    workspace.inherit_missing_primary_fields(&global.workspace);
    let mut mounts = global.workspace.mounts;
    mounts.extend(workspace.mounts);
    workspace.mounts = mounts;

    let mut network = global.network;
    network.apply_repo_overrides(&repo.network);

    let mut warnings = global.warnings;
    warnings.extend(repo.warnings);

    Config {
        default_image: repo.default_image.or(global.default_image),
        default_agent: repo.default_agent.or(global.default_agent),
        default_model: repo.default_model.or(global.default_model),
        session_root: global.session_root,
        model_cache_root: global.model_cache_root,
        tool_call_max: repo.tool_call_max.or(global.tool_call_max),
        tool_result_max: repo.tool_result_max.or(global.tool_result_max),
        subagent_depth_max: repo.subagent_depth_max.or(global.subagent_depth_max),
        subagent_width_max: repo.subagent_width_max.or(global.subagent_width_max),
        retry_budget_secs: repo.retry_budget_secs.or(global.retry_budget_secs),
        mcp_call_timeout_secs: repo.mcp_call_timeout_secs.or(global.mcp_call_timeout_secs),
        network,
        providers,
        models,
        agents,
        workspace,
        images,
        sidecars,
        warnings,
    }
}
