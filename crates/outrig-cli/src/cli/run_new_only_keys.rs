//! The config keys only `outrig run-new` reads, and the startup warning
//! `outrig run` prints when a config sets one.
//!
//! `[models.<name>].context-window` and `[events] mode = "record"` live on
//! types both loops share, so `run` parses them and does nothing with them.
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
pub(crate) fn warnings<'a>(cfg: &Config, models: impl IntoIterator<Item = &'a str>) -> Vec<String> {
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
        assert!(warnings(&cfg, ["fast"]).is_empty());
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
            warnings(&cfg, ["fast"]),
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
            warnings(&cfg, ["first", "second", "third"]),
            [
                ignored("context-window on [models.second], [models.third]"),
                ignored("[events] mode = \"record\""),
            ]
        );
    }
}
