//! Merge global + repo `Config` values. Repo entries replace global entries
//! with the same key (no per-key merging within an entry), except extra
//! workspace mounts which are concatenated in global-then-repo order.

use std::sync::Arc;

use super::Config;

/// Merge `global` and `repo`, with `repo` winning on every collision.
///
/// - For each map (`providers`, `models`, `agents`, `images`, `sidecars`,
///   `bindings`):
///   repo entries replace global entries with the same key. Entries unique to
///   either side are preserved as-is. A repo image-config can therefore name a
///   sidecar the user declared globally.
/// - For top-level scalars (`default-image`, `default-agent`,
///   `default-model`, `session-root`, `tool-call-max`,
///   `tool-result-max`, `subagent-depth-max`, `subagent-width-max`,
///   `retry-budget-secs`): repo's value wins if set, else global's.
/// - `[network].mode` follows repo precedence when the repo config declares
///   `mode`; a `[network]` table that declares no mode inherits the global
///   one. Policy keys (`default`, `allow`, `deny`) are global-only: this
///   function never reads them from the repo side, so a repo value carrying
///   its own policy cannot widen or inject one, whatever built it.
/// - `[events].mode` is the global config's when it declares one; a repo's
///   applies only when the global config is silent.
/// - `[policy]` is the global config's. A repo's `[policy]` is kept beside it
///   as a restriction: each decides a request on its own and the stricter
///   action wins, so a repo can add a deny or an escalation and can never
///   allow what the operator's rules hold back.
/// - The global config itself is kept, as read, as the merged config's
///   [`operator`](Config::operator) layer -- or the global's own operator
///   layer, when `global` is itself a merged config -- so what a repo must not
///   choose can still be read from the operator's files.
/// - `[workspace]` primary fields merge per key, like the scalars above: a
///   repo declaration wins, otherwise a global one is inherited, otherwise the
///   key stays `None` and [`Workspace`](super::Workspace)'s accessors apply
///   the built-in default. Extra `workspace.mounts` are concatenated so
///   user-level resource mounts and repo-level resource mounts both
///   participate.
///
/// The result is a flattened snapshot, not a config file: provenance rides
/// along in memory but is `#[serde(skip)]`, so re-serializing a merged config
/// emits each inherited path as the text its source file used and loses the
/// base directory that text meant. Nothing in outrig writes a merged config
/// back to disk.
pub fn merge(global: Config, repo: Config) -> Config {
    let operator = Some(
        global
            .operator
            .clone()
            .unwrap_or_else(|| Arc::new(global.clone())),
    );

    let mut restrictions = global.restrictions;
    restrictions.extend(
        std::iter::once(repo.policy)
            .chain(repo.restrictions)
            .filter(|table| !table.is_default()),
    );

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

    Config {
        default_image: repo.default_image.or(global.default_image),
        default_agent: repo.default_agent.or(global.default_agent),
        default_model: repo.default_model.or(global.default_model),
        session_root: repo.session_root.or(global.session_root),
        tool_call_max: repo.tool_call_max.or(global.tool_call_max),
        tool_result_max: repo.tool_result_max.or(global.tool_result_max),
        subagent_depth_max: repo.subagent_depth_max.or(global.subagent_depth_max),
        subagent_width_max: repo.subagent_width_max.or(global.subagent_width_max),
        retry_budget_secs: repo.retry_budget_secs.or(global.retry_budget_secs),
        network,
        events,
        providers,
        models,
        agents,
        workspace,
        images,
        sidecars,
        bindings,
        policy: global.policy,
        restrictions,
        operator,
    }
}
