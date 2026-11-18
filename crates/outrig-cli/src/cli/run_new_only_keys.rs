//! The config keys only `outrig run-new` reads, and the startup warning
//! `outrig run` prints when a config sets one.
//!
//! `[models.<name>].context-window`, `[events] mode = "record"`, the limits on
//! `run-new`'s children and model calls, and `[agents.<name>].repo-instructions`
//! live on types both loops share, so `run` parses them and does nothing with
//! them.
//! Honoring them in `run`'s loop would be a second implementation of what
//! `run-new` does; saying so at startup is the cheap fix. See
//! `plan/phase/0003-python/crate-split-tradeoffs.md`.
//!
//! A key belongs here when `run` ignoring it changes what the session does.
//! Delete this module when `run` is retargeted at the Python loop.

use outrig::config::{Config, EventsMode};

/// The single warning `outrig run` prints for keys `cfg` sets that only
/// `outrig run-new` reads. Empty when it sets none.
///
/// `models` is every row the session may run against, so a failover chain's
/// fallback is named too, naming each row that sets a window.
/// `agent` is the session's agent, whose own limits count as the top
/// level's do.
pub(crate) fn warnings<'a>(
    cfg: &Config,
    agent: Option<&str>,
    models: impl IntoIterator<Item = &'a str>,
) -> Vec<String> {
    let mut keys = Vec::new();

    let windowed: Vec<String> = models
        .into_iter()
        .filter(|name| {
            cfg.models
                .get(*name)
                .is_some_and(|model| model.context_window.is_some())
        })
        .map(|name| format!("[models.{name}]"))
        .collect();
    if !windowed.is_empty() {
        keys.push(format!("context-window on {}", windowed.join(", ")));
    }

    if matches!(cfg.events.mode(), EventsMode::Record) {
        keys.push("[events] mode = \"record\"".to_string());
    }

    let agent = agent.and_then(|name| cfg.agents.get(name));
    let limits = [
        ("children-max", cfg.children_max.is_some(), agent.is_some_and(|a| a.children_max.is_some())),
        (
            "model-concurrency-max",
            cfg.model_concurrency_max.is_some(),
            agent.is_some_and(|a| a.model_concurrency_max.is_some()),
        ),
        ("requests-max", cfg.requests_max.is_some(), agent.is_some_and(|a| a.requests_max.is_some())),
        ("token-budget", cfg.token_budget.is_some(), agent.is_some_and(|a| a.token_budget.is_some())),
        (
            "model-request-limit",
            cfg.model_request_limit.is_some(),
            agent.is_some_and(|a| a.model_request_limit.is_some()),
        ),
        (
            "model-request-warnings",
            cfg.model_request_warnings.is_some(),
            agent.is_some_and(|a| a.model_request_warnings.is_some()),
        ),
    ];
    for (key, top, own) in limits {
        if top || own {
            keys.push(key.to_string());
        }
    }

    // `run` sends the agent's preamble alone, whichever way the key is set.
    if agent.is_some_and(|a| a.repo_instructions.is_some()) {
        keys.push("repo-instructions".to_string());
    }

    if keys.is_empty() {
        Vec::new()
    } else {
        vec![ignored(&keys.join("; "))]
    }
}

fn ignored(what: &str) -> String {
    format!("[outrig] warning: `run` ignores these settings; only `run-new` reads them: {what}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(toml_text: &str) -> Config {
        toml::from_str(toml_text).expect("config parses")
    }

    /// Neither key is set where it counts: the event log is off, and the only
    /// window is on a model outside the session.
    #[test]
    fn a_config_that_sets_neither_key_for_the_session_warns_of_nothing() {
        let cfg = config(
            r#"
[events]
mode = "off"

[models.fast]
provider = "openai"

[models.other]
provider       = "openai"
context-window = 32000
"#,
        );
        assert!(warnings(&cfg, None, ["fast"]).is_empty());
    }

    #[test]
    fn recording_events_is_named() {
        let cfg = config(
            r#"
[events]
mode = "record"

[models.fast]
provider = "openai"
"#,
        );
        assert_eq!(
            warnings(&cfg, None, ["fast"]),
            [ignored("[events] mode = \"record\"")]
        );
    }

    /// The window of a failover chain's fallback is ignored as surely as the
    /// first row's, so the one line names every row of the chain that sets
    /// one, with the events setting in the same warning.
    #[test]
    fn each_chain_row_that_sets_a_window_is_named_on_one_line() {
        let cfg = config(
            r#"
[events]
mode = "record"

[models.first]
provider = "openai"

[models.second]
provider       = "openai"
context-window = 32000

[models.third]
provider       = "openai"
context-window = 64000
"#,
        );
        assert_eq!(
            warnings(&cfg, None, ["first", "second", "third"]),
            [ignored(
                "context-window on [models.second], [models.third]; [events] mode = \"record\""
            )]
        );
    }

    /// A limit on `run-new`'s children or model calls is named whether the
    /// top level sets it or the session's agent does; another agent's is not
    /// the session's.
    #[test]
    fn child_limits_are_named_for_the_top_level_and_the_sessions_agent() {
        let cfg = config(
            r#"
token-budget = 1000

[models.fast]
provider = "openai"

[agents.coding]
children-max = 4

[agents.other]
requests-max = 4
"#,
        );
        assert_eq!(
            warnings(&cfg, Some("coding"), ["fast"]),
            [ignored("children-max; token-budget")]
        );
        assert_eq!(
            warnings(&cfg, None, ["fast"]),
            [ignored("token-budget")]
        );
    }

    /// An agent's model-request limit and warnings are `run-new`'s, set at
    /// the top level or by the session's agent.
    #[test]
    fn the_model_request_limit_is_named() {
        let cfg = config(
            r#"
model-request-warnings = [5, 1]

[models.fast]
provider = "openai"

[agents.coding]
model-request-limit = 40
"#,
        );
        assert_eq!(
            warnings(&cfg, Some("coding"), ["fast"]),
            [ignored("model-request-limit; model-request-warnings")]
        );
        assert_eq!(
            warnings(&cfg, None, ["fast"]),
            [ignored("model-request-warnings")]
        );
    }

    /// All applicable keys share one warning. A key set at both scopes is
    /// named only once, and unused models do not add settings.
    #[test]
    fn all_ignored_settings_share_one_warning() {
        let cfg = config(
            r#"
children-max = 2
model-concurrency-max = 3
requests-max = 4
token-budget = 1000
model-request-limit = 40
model-request-warnings = [5, 1]

[events]
mode = "record"

[models.fast]
provider = "openai"
context-window = 32000

[models.unused]
provider = "openai"
context-window = 64000

[agents.coding]
children-max = 4
repo-instructions = "agents-md"
"#,
        );
        assert_eq!(
            warnings(&cfg, Some("coding"), ["fast"]),
            [ignored(
                "context-window on [models.fast]; [events] mode = \"record\"; \
                 children-max; model-concurrency-max; requests-max; token-budget; \
                 model-request-limit; model-request-warnings; repo-instructions"
            )]
        );
    }

    /// `run` loads no instructions file, so a config that says whether the
    /// session's agent should is named; another agent's setting is not the
    /// session's.
    #[test]
    fn repo_instructions_are_named_for_the_sessions_agent_only() {
        let cfg = config(
            r#"
[models.fast]
provider = "openai"

[agents.coding]
repo-instructions = "agents-md"

[agents.other]
repo-instructions = "none"

[agents.plain]
preamble = "You write Python."
"#,
        );
        assert_eq!(
            warnings(&cfg, Some("coding"), ["fast"]),
            [ignored("repo-instructions")]
        );
        assert!(warnings(&cfg, Some("plain"), ["fast"]).is_empty());
        assert!(warnings(&cfg, None, ["fast"]).is_empty());
    }
}
