//! Merge global + repo `Config` values. Repo entries replace global entries
//! with the same key (no per-key merging within an entry), except extra
//! workspace mounts which are concatenated in global-then-repo order.

use super::Config;

/// Merge `global` and `repo`, with `repo` winning on every collision.
///
/// - For each map (`providers`, `models`, `agents`, `images`, `sidecars`,
///   `bindings`):
///   repo entries replace global entries with the same key. Entries unique to
///   either side are preserved as-is. A repo image-config can therefore name a
///   sidecar the user declared globally. Which providers a repo file may
///   declare is not this function's question: [`Config::validate_as_repo`]
///   refuses a repo provider that carries an `api-key` before the file gets
///   here, and a repo value built by hand must be held to it the same way.
/// - For top-level scalars (`default-image`, `default-agent`,
///   `default-model`, `tool-call-max`, `tool-result-max`,
///   `subagent-depth-max`, `subagent-width-max`, `children-max`,
///   `model-concurrency-max`, `requests-max`, `token-budget`,
///   `retry-budget-secs`, `mcp-call-timeout-secs`):
///   repo's value wins if set, else global's.
/// - `session-root` is global-only: it is taken from `global` and never read
///   from `repo`, so a repo value cannot move where this machine writes,
///   whatever built it.
/// - `[repo]` is global-only too, for the same reason: a repo value cannot
///   widen which host variables a repo config may reference, whatever built
///   it. [`Config::validate`] reads that list off the merged result.
/// - `[network].mode` follows repo precedence when the repo config declares
///   `mode`; a `[network]` table that declares no mode inherits the global
///   one. Policy keys (`default`, `allow`, `deny`) are global-only: this
///   function never reads them from the repo side, so a repo value carrying
///   its own policy cannot widen or inject one, whatever built it.
/// - `[events].mode` is the global config's when it declares one; a repo's
///   applies only when the global config is silent.
/// - `[host]` merges per key, like the scalars above: a repo declaration wins.
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
/// base directory that text meant. `session-root` is the exception: it was
/// resolved as it was read, so it comes out absolute. Nothing in outrig
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

    // A repo binding replacing a global one keeps the repo's source, so it
    // still needs the operator's approval.
    let mut bindings = global.bindings;
    bindings.extend(repo.bindings);

    let mut workspace = repo.workspace;
    workspace.inherit_missing_primary_fields(&global.workspace);
    let mut mounts = global.workspace.mounts;
    mounts.extend(workspace.mounts);
    workspace.mounts = mounts;

    let mut network = global.network;
    network.apply_repo_overrides(&repo.network);

    let mut events = global.events;
    events.inherit_from_repo(&repo.events);

    let mut host = repo.host;
    host.inherit_from_global(&global.host);

    let mut warnings = global.warnings;
    warnings.extend(repo.warnings);

    Config {
        default_image: repo.default_image.or(global.default_image),
        default_agent: repo.default_agent.or(global.default_agent),
        default_model: repo.default_model.or(global.default_model),
        session_root: global.session_root,
        tool_call_max: repo.tool_call_max.or(global.tool_call_max),
        tool_result_max: repo.tool_result_max.or(global.tool_result_max),
        subagent_depth_max: repo.subagent_depth_max.or(global.subagent_depth_max),
        subagent_width_max: repo.subagent_width_max.or(global.subagent_width_max),
        children_max: repo.children_max.or(global.children_max),
        model_concurrency_max: repo.model_concurrency_max.or(global.model_concurrency_max),
        requests_max: repo.requests_max.or(global.requests_max),
        token_budget: repo.token_budget.or(global.token_budget),
        retry_budget_secs: repo.retry_budget_secs.or(global.retry_budget_secs),
        mcp_call_timeout_secs: repo.mcp_call_timeout_secs.or(global.mcp_call_timeout_secs),
        network,
        events,
        host,
        repo: global.repo,
        providers,
        models,
        agents,
        workspace,
        images,
        sidecars,
        bindings,
        warnings,
    }
}
