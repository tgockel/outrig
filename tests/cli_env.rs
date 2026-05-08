//! Integration tests for `--env` CLI flag parsing, validation, and merge
//! semantics exposed through [`CliEnvEntries`].

use std::collections::BTreeMap;

use outrig::cli::env_arg::{CliEnvEntries, CliEnvParseError};
use outrig::config::EnvValue;

// --- Parse acceptance ---

#[test]
fn parse_global_literal() {
    let entries = CliEnvEntries::parse(&["RUST_LOG=debug".to_string()]).unwrap();
    assert_eq!(
        entries.global["RUST_LOG"],
        EnvValue::Literal("debug".to_string())
    );
}

#[test]
fn parse_global_env_ref() {
    let entries = CliEnvEntries::parse(&["TOKEN=${GITHUB_TOKEN}".to_string()]).unwrap();
    assert_eq!(
        entries.global["TOKEN"],
        EnvValue::EnvRef("GITHUB_TOKEN".to_string())
    );
}

#[test]
fn parse_per_server_literal() {
    let entries = CliEnvEntries::parse(&["fs:VERBOSE=1".to_string()]).unwrap();
    assert_eq!(
        entries.per_server["fs"]["VERBOSE"],
        EnvValue::Literal("1".to_string())
    );
}

#[test]
fn parse_per_server_env_ref() {
    let entries = CliEnvEntries::parse(&["build:KEY=${SECRET}".to_string()]).unwrap();
    assert_eq!(
        entries.per_server["build"]["KEY"],
        EnvValue::EnvRef("SECRET".to_string())
    );
}

#[test]
fn parse_value_with_equals() {
    let entries = CliEnvEntries::parse(&["OPTS=--foo=bar".to_string()]).unwrap();
    assert_eq!(
        entries.global["OPTS"],
        EnvValue::Literal("--foo=bar".to_string())
    );
}

#[test]
fn parse_empty_value_is_literal() {
    let entries = CliEnvEntries::parse(&["EMPTY=".to_string()]).unwrap();
    assert_eq!(entries.global["EMPTY"], EnvValue::Literal(String::new()));
}

// --- Parse rejection ---

#[test]
fn parse_rejects_no_equals() {
    let err = CliEnvEntries::parse(&["NOEQUALS".to_string()]).unwrap_err();
    assert!(matches!(err, CliEnvParseError::MissingEquals { .. }));
}

#[test]
fn parse_rejects_empty_key() {
    let err = CliEnvEntries::parse(&["=value".to_string()]).unwrap_err();
    assert!(matches!(err, CliEnvParseError::EmptyKey { .. }));
}

#[test]
fn parse_rejects_empty_key_in_per_server() {
    let err = CliEnvEntries::parse(&["fs:=value".to_string()]).unwrap_err();
    assert!(matches!(err, CliEnvParseError::EmptyKey { .. }));
}

#[test]
fn parse_rejects_empty_server_name() {
    let err = CliEnvEntries::parse(&[":KEY=value".to_string()]).unwrap_err();
    assert!(matches!(err, CliEnvParseError::EmptyKey { .. }));
}

// --- Merge precedence ---

#[test]
fn for_server_global_only() {
    let entries = CliEnvEntries::parse(&["A=1".to_string(), "B=2".to_string()]).unwrap();
    let merged = entries.for_server("any");
    assert_eq!(merged.len(), 2);
    assert_eq!(merged["A"], EnvValue::Literal("1".to_string()));
    assert_eq!(merged["B"], EnvValue::Literal("2".to_string()));
}

#[test]
fn for_server_per_server_overrides_global() {
    let entries =
        CliEnvEntries::parse(&["KEY=global".to_string(), "fs:KEY=per_server".to_string()]).unwrap();
    let merged = entries.for_server("fs");
    assert_eq!(merged["KEY"], EnvValue::Literal("per_server".to_string()));
}

#[test]
fn for_server_unrelated_per_server_ignored() {
    let entries =
        CliEnvEntries::parse(&["KEY=global".to_string(), "build:KEY=build_val".to_string()])
            .unwrap();
    let merged = entries.for_server("fs");
    assert_eq!(merged["KEY"], EnvValue::Literal("global".to_string()));
}

#[test]
fn for_server_empty_entries_returns_empty_map() {
    let entries = CliEnvEntries::default();
    let merged = entries.for_server("anything");
    assert!(merged.is_empty());
}

// --- Last-wins within scope ---

#[test]
fn last_global_wins() {
    let entries = CliEnvEntries::parse(&["X=first".to_string(), "X=second".to_string()]).unwrap();
    assert_eq!(entries.global["X"], EnvValue::Literal("second".to_string()));
}

#[test]
fn last_per_server_wins() {
    let entries =
        CliEnvEntries::parse(&["fs:X=first".to_string(), "fs:X=second".to_string()]).unwrap();
    assert_eq!(
        entries.per_server["fs"]["X"],
        EnvValue::Literal("second".to_string())
    );
}

// --- Utility ---

#[test]
fn is_empty_true_for_default() {
    assert!(CliEnvEntries::default().is_empty());
}

#[test]
fn is_empty_false_with_entries() {
    let entries = CliEnvEntries::parse(&["A=1".to_string()]).unwrap();
    assert!(!entries.is_empty());
}

#[test]
fn per_server_names_collects_all_named_servers() {
    let entries = CliEnvEntries::parse(&[
        "fs:A=1".to_string(),
        "build:B=2".to_string(),
        "shell:C=3".to_string(),
    ])
    .unwrap();
    let mut names: Vec<&str> = entries.per_server_names().collect();
    names.sort();
    assert_eq!(names, vec!["build", "fs", "shell"]);
}

// --- EnvValue resolution integration ---

#[test]
fn env_ref_resolves_from_host_env() {
    let var = "OUTRIG_TEST_CLI_ENV_RESOLVE";
    // SAFETY: unique var name, single-threaded test.
    unsafe {
        std::env::set_var(var, "hello");
    }
    let entries = CliEnvEntries::parse(&[format!("TOKEN=${{{var}}}")]).unwrap();
    let merged = entries.for_server("any");
    let resolved = merged["TOKEN"].resolve().expect("should resolve");
    unsafe {
        std::env::remove_var(var);
    }
    assert_eq!(resolved, "hello");
}

#[test]
fn literal_value_unaffected_by_env() {
    let entries = CliEnvEntries::parse(&["PATH=/usr/bin".to_string()]).unwrap();
    let merged = entries.for_server("any");
    let resolved = merged["PATH"].resolve().expect("should resolve");
    assert_eq!(resolved, "/usr/bin");
}

// --- Merge overlay on BTreeMap (simulates config-file + CLI) ---

#[test]
fn cli_overlay_onto_config_env() {
    // Simulate config-file env for a server.
    let mut config_env: BTreeMap<String, EnvValue> = BTreeMap::new();
    config_env.insert(
        "CARGO_HOME".to_string(),
        EnvValue::Literal("/workspace/.cargo".to_string()),
    );
    config_env.insert("DEBUG".to_string(), EnvValue::Literal("0".to_string()));

    // CLI adds a global override and a new key.
    let entries = CliEnvEntries::parse(&["DEBUG=1".to_string(), "EXTRA=yes".to_string()]).unwrap();
    let cli_overlay = entries.for_server("build");

    // Merge: CLI on top of config.
    for (k, v) in &cli_overlay {
        config_env.insert(k.clone(), v.clone());
    }

    assert_eq!(
        config_env["CARGO_HOME"],
        EnvValue::Literal("/workspace/.cargo".to_string()),
        "untouched config entry preserved"
    );
    assert_eq!(
        config_env["DEBUG"],
        EnvValue::Literal("1".to_string()),
        "CLI overrides config"
    );
    assert_eq!(
        config_env["EXTRA"],
        EnvValue::Literal("yes".to_string()),
        "new CLI entry added"
    );
}
