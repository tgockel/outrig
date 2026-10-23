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

/// The lines `outrig run` prints, in order, for keys `cfg` sets that only
/// `outrig run-new` reads. Empty when it sets none.
///
/// `models` is every row the session may run against, so a failover chain's
/// fallback is named too: one line, naming each row that sets a window.
/// `agent` is the session's agent, whose own limits count as the top
/// level's do.
pub(crate) fn warnings<'a>(
    cfg: &Config,
    agent: Option<&str>,
    models: impl IntoIterator<Item = &'a str>,
) -> Vec<String> {
    let mut lines = Vec::new();

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
        lines.push(ignored(&format!(
            "context-window on {}",
            windowed.join(", ")
        )));
    }

    if matches!(cfg.events.mode(), EventsMode::Record) {
        lines.push(ignored("[events] mode = \"record\""));
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
    ];
    for (key, top, own) in limits {
        if top || own {
            lines.push(ignored(key));
        }
    }

    // `run` sends the agent's preamble alone, whichever way the key is set.
    if agent.is_some_and(|a| a.repo_instructions.is_some()) {
        lines.push(ignored("repo-instructions"));
    }

    lines
}

fn ignored(what: &str) -> String {
    format!("[outrig] warning: {what} is ignored by `run`; only `run-new` reads it")
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
    /// one, and the events line follows it.
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
            [
                ignored("context-window on [models.second], [models.third]"),
                ignored("[events] mode = \"record\""),
            ]
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
            [ignored("children-max"), ignored("token-budget")]
        );
        assert_eq!(
            warnings(&cfg, None, ["fast"]),
            [ignored("token-budget")]
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
