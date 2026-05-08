//! Parse and validate `--env` CLI entries for `outrig run` and `outrig mcp`.
//!
//! Each `--env` value is either:
//! - `KEY=VALUE` -- applied to every MCP server (global).
//! - `SERVER:KEY=VALUE` -- applied only to the named MCP server.
//!
//! The right-hand side is processed through [`EnvValue::from_raw`] so that
//! `${VAR}` references resolve from the host environment at MCP startup,
//! identically to config-file values.

use std::collections::BTreeMap;

use thiserror::Error;

use crate::config::EnvValue;

/// Errors specific to `--env` parsing.
#[derive(Debug, Error)]
pub enum CliEnvParseError {
    #[error("invalid value '{raw}' for '--env <KEY=VALUE>': missing '='")]
    MissingEquals { raw: String },

    #[error("invalid value '{raw}' for '--env <KEY=VALUE>': empty key")]
    EmptyKey { raw: String },
}

/// Parsed `--env` entries, split into global and per-server maps.
#[derive(Debug, Clone, Default)]
pub struct CliEnvEntries {
    /// `KEY=VALUE` entries applied to every MCP server.
    pub global: BTreeMap<String, EnvValue>,
    /// `SERVER:KEY=VALUE` entries keyed by server name.
    pub per_server: BTreeMap<String, BTreeMap<String, EnvValue>>,
}

impl CliEnvEntries {
    /// Parse raw `--env` strings into classified entries.
    ///
    /// Within a scope (global or per-server), last-wins for duplicate keys --
    /// entries are inserted in order, so later values replace earlier ones.
    pub fn parse(raw: &[String]) -> Result<Self, CliEnvParseError> {
        let mut result = Self::default();

        for entry in raw {
            // Find the first `=` to split key-side from value.
            let eq_pos = entry
                .find('=')
                .ok_or_else(|| CliEnvParseError::MissingEquals { raw: entry.clone() })?;

            let key_side = &entry[..eq_pos];
            let value_side = &entry[eq_pos + 1..];

            if key_side.is_empty() {
                return Err(CliEnvParseError::EmptyKey { raw: entry.clone() });
            }

            let env_value = EnvValue::from_raw(value_side.to_string());

            // Check for `SERVER:KEY` form. A colon in the key-side means
            // per-server -- but only if there's text on both sides of the
            // colon.
            if let Some(colon_pos) = key_side.find(':') {
                let server = &key_side[..colon_pos];
                let key = &key_side[colon_pos + 1..];

                if server.is_empty() || key.is_empty() {
                    // Treat as global if the colon form is malformed (e.g.
                    // `:KEY=VAL` or `SERVER:=VAL`). The empty-key case will
                    // be caught, the empty-server-but-nonempty-key case is
                    // treated as a literal key including the leading colon
                    // (matches env-var naming flexibility). Actually, let's
                    // be strict: empty server or empty key after colon is an
                    // error.
                    if key.is_empty() {
                        return Err(CliEnvParseError::EmptyKey { raw: entry.clone() });
                    }
                    // Empty server (`:KEY=VAL`) -- treat as global with key
                    // `:KEY` would be confusing; error out.
                    return Err(CliEnvParseError::EmptyKey { raw: entry.clone() });
                }

                result
                    .per_server
                    .entry(server.to_string())
                    .or_default()
                    .insert(key.to_string(), env_value);
            } else {
                result.global.insert(key_side.to_string(), env_value);
            }
        }

        Ok(result)
    }

    /// Produce the merged env overlay for a specific server: global entries
    /// with per-server entries layered on top (per-server wins on conflict).
    pub fn for_server(&self, name: &str) -> BTreeMap<String, EnvValue> {
        let mut merged = self.global.clone();
        if let Some(per) = self.per_server.get(name) {
            for (k, v) in per {
                merged.insert(k.clone(), v.clone());
            }
        }
        merged
    }

    /// Return `true` if there are no entries at all.
    pub fn is_empty(&self) -> bool {
        self.global.is_empty() && self.per_server.is_empty()
    }

    /// Return the set of server names referenced in per-server entries.
    pub fn per_server_names(&self) -> impl Iterator<Item = &str> {
        self.per_server.keys().map(String::as_str)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_global_entry() {
        let entries = CliEnvEntries::parse(&["FOO=bar".to_string()]).unwrap();
        assert_eq!(entries.global.len(), 1);
        assert_eq!(entries.global["FOO"], EnvValue::Literal("bar".to_string()));
        assert!(entries.per_server.is_empty());
    }

    #[test]
    fn parse_per_server_entry() {
        let entries = CliEnvEntries::parse(&["fs:DEBUG=1".to_string()]).unwrap();
        assert!(entries.global.is_empty());
        assert_eq!(entries.per_server.len(), 1);
        assert_eq!(
            entries.per_server["fs"]["DEBUG"],
            EnvValue::Literal("1".to_string())
        );
    }

    #[test]
    fn parse_mixed_entries() {
        let raw = vec![
            "RUST_LOG=info".to_string(),
            "build:CARGO_TERM_COLOR=always".to_string(),
            "fs:DEBUG=1".to_string(),
        ];
        let entries = CliEnvEntries::parse(&raw).unwrap();
        assert_eq!(entries.global.len(), 1);
        assert_eq!(entries.per_server.len(), 2);
        assert_eq!(
            entries.global["RUST_LOG"],
            EnvValue::Literal("info".to_string())
        );
        assert_eq!(
            entries.per_server["build"]["CARGO_TERM_COLOR"],
            EnvValue::Literal("always".to_string())
        );
    }

    #[test]
    fn parse_env_ref_in_value() {
        let entries = CliEnvEntries::parse(&["GH_TOKEN=${GITHUB_TOKEN}".to_string()]).unwrap();
        assert_eq!(
            entries.global["GH_TOKEN"],
            EnvValue::EnvRef("GITHUB_TOKEN".to_string())
        );
    }

    #[test]
    fn parse_last_wins_within_scope() {
        let raw = vec!["FOO=first".to_string(), "FOO=second".to_string()];
        let entries = CliEnvEntries::parse(&raw).unwrap();
        assert_eq!(
            entries.global["FOO"],
            EnvValue::Literal("second".to_string())
        );
    }

    #[test]
    fn parse_last_wins_per_server() {
        let raw = vec!["fs:DEBUG=0".to_string(), "fs:DEBUG=1".to_string()];
        let entries = CliEnvEntries::parse(&raw).unwrap();
        assert_eq!(
            entries.per_server["fs"]["DEBUG"],
            EnvValue::Literal("1".to_string())
        );
    }

    #[test]
    fn parse_rejects_missing_equals() {
        let err = CliEnvEntries::parse(&["FOO".to_string()]).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("missing '='"), "got: {msg}");
    }

    #[test]
    fn parse_rejects_empty_key() {
        let err = CliEnvEntries::parse(&["=value".to_string()]).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("empty key"), "got: {msg}");
    }

    #[test]
    fn parse_rejects_empty_key_after_colon() {
        let err = CliEnvEntries::parse(&["fs:=value".to_string()]).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("empty key"), "got: {msg}");
    }

    #[test]
    fn parse_rejects_empty_server_before_colon() {
        let err = CliEnvEntries::parse(&[":KEY=value".to_string()]).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("empty key"), "got: {msg}");
    }

    #[test]
    fn parse_allows_empty_value() {
        let entries = CliEnvEntries::parse(&["FOO=".to_string()]).unwrap();
        assert_eq!(entries.global["FOO"], EnvValue::Literal(String::new()));
    }

    #[test]
    fn parse_allows_value_containing_equals() {
        let entries = CliEnvEntries::parse(&["OPTS=--flag=val".to_string()]).unwrap();
        assert_eq!(
            entries.global["OPTS"],
            EnvValue::Literal("--flag=val".to_string())
        );
    }

    #[test]
    fn for_server_merges_global_and_per_server() {
        let raw = vec![
            "GLOBAL=yes".to_string(),
            "SHARED=global_val".to_string(),
            "fs:SHARED=fs_val".to_string(),
            "fs:LOCAL=only_fs".to_string(),
        ];
        let entries = CliEnvEntries::parse(&raw).unwrap();
        let merged = entries.for_server("fs");

        assert_eq!(merged["GLOBAL"], EnvValue::Literal("yes".to_string()));
        assert_eq!(merged["SHARED"], EnvValue::Literal("fs_val".to_string()));
        assert_eq!(merged["LOCAL"], EnvValue::Literal("only_fs".to_string()));
    }

    #[test]
    fn for_server_returns_only_global_when_no_per_server() {
        let raw = vec!["GLOBAL=yes".to_string()];
        let entries = CliEnvEntries::parse(&raw).unwrap();
        let merged = entries.for_server("unknown");

        assert_eq!(merged.len(), 1);
        assert_eq!(merged["GLOBAL"], EnvValue::Literal("yes".to_string()));
    }

    #[test]
    fn is_empty_on_default() {
        assert!(CliEnvEntries::default().is_empty());
    }

    #[test]
    fn is_empty_false_with_global() {
        let entries = CliEnvEntries::parse(&["X=1".to_string()]).unwrap();
        assert!(!entries.is_empty());
    }

    #[test]
    fn per_server_names_lists_servers() {
        let raw = vec!["fs:A=1".to_string(), "build:B=2".to_string()];
        let entries = CliEnvEntries::parse(&raw).unwrap();
        let mut names: Vec<&str> = entries.per_server_names().collect();
        names.sort();
        assert_eq!(names, vec!["build", "fs"]);
    }
}
