//! `[policy]` and the operator's layer: what a config file may say of the rules that decide each
//! request to a binding, and what a repository's config cannot reach.

use std::fs;
use std::path::{Path, PathBuf};

use tempfile::tempdir;

use outrig::config::{
    Config, ConfigValidationError, LlmProvider, PolicyAction, PolicyConfig, PolicyRule, merge,
};
use outrig::error::OutrigError;

fn parse(s: &str) -> Config {
    Config::load_from_str(s).expect("config parses")
}

fn write_repo_cfg(root: &Path, body: &str) {
    let agents = root.join(".agents/outrig");
    fs::create_dir_all(&agents).unwrap();
    fs::write(agents.join("config.toml"), body).unwrap();
}

fn write_global_cfg(dir: &Path, body: &str) -> PathBuf {
    let path = dir.join("global.toml");
    fs::write(&path, body).unwrap();
    path
}

const OPERATOR: &str = r#"
[policy]
default = "deny"

[[policy.rules]]
binding   = "repo"
type      = "git.remote.Remote"
member    = "push"
operation = "call"
action    = "escalate"

[[policy.rules]]
member = "*"
action = "allow"
"#;

#[test]
fn the_schema_reads_rules_and_a_default() {
    let cfg = parse(OPERATOR);
    assert_eq!(cfg.policy.default, Some(PolicyAction::Deny));
    assert_eq!(
        cfg.policy.rules,
        [
            PolicyRule::new(PolicyAction::Escalate)
                .binding("repo")
                .type_name("git.remote.Remote")
                .member("push")
                .operation("call"),
            PolicyRule::new(PolicyAction::Allow).member("*"),
        ]
    );
    cfg.validate(None).expect("the rules validate");
    assert_eq!(
        parse("").policy,
        PolicyConfig::default(),
        "nothing declared"
    );
}

#[test]
fn an_unknown_action_or_field_is_refused_at_parse() {
    let action = Config::load_from_str("[[policy.rules]]\naction = \"ask\"\n").unwrap_err();
    assert!(
        action.to_string().contains("unknown variant `ask`"),
        "{action}"
    );
    let field = Config::load_from_str("[[policy.rules]]\naction = \"deny\"\nmethod = \"push\"\n")
        .unwrap_err();
    assert!(
        field.to_string().contains("unknown field `method`"),
        "{field}"
    );
    let default = Config::load_from_str("[policy]\ndefault = \"ask\"\n").unwrap_err();
    assert!(
        default.to_string().contains("unknown variant `ask`"),
        "{default}"
    );
}

#[test]
fn an_operation_outside_the_vocabulary_is_refused() {
    for operation in ["invoke", "del", "getroot", "Call"] {
        let cfg = parse(&format!(
            "[[policy.rules]]\naction = \"deny\"\n\n[[policy.rules]]\noperation = \"{operation}\"\n\
             action = \"deny\"\n"
        ));
        match cfg.validate(None) {
            Err(OutrigError::ConfigValidation(ConfigValidationError::PolicyOperation {
                item,
                operation: named,
                ..
            })) => {
                assert_eq!(item, "[[policy.rules]] #2");
                assert_eq!(named, operation);
            }
            other => panic!("{operation} should be refused, got {other:?}"),
        }
    }
    let tmp = tempdir().unwrap();
    write_repo_cfg(
        tmp.path(),
        "[[policy.rules]]\noperation = \"invoke\"\naction = \"deny\"\n",
    );
    let err = Config::load(tmp.path(), None).unwrap_err();
    assert!(err.to_string().contains("operation \"invoke\""), "{err}");
}

#[test]
fn a_repo_allow_is_refused_at_load_by_name() {
    for (body, item) in [
        ("[policy]\ndefault = \"allow\"\n", "[policy].default"),
        (
            "[[policy.rules]]\naction = \"deny\"\n\n[[policy.rules]]\nmember = \"push\"\n\
             action = \"allow\"\n",
            "[[policy.rules]] #2",
        ),
    ] {
        let tmp = tempdir().unwrap();
        write_repo_cfg(tmp.path(), body);
        let err = Config::load(tmp.path(), None).unwrap_err();
        assert!(
            err.to_string()
                .contains(&format!("may only restrict [policy]: {item} is allow")),
            "{body} should be refused, got: {err}",
        );
    }
}

#[test]
fn a_repo_may_restrict_and_the_merged_policy_stays_the_operators() {
    let tmp = tempdir().unwrap();
    let global = write_global_cfg(tmp.path(), OPERATOR);
    write_repo_cfg(
        tmp.path(),
        "[policy]\ndefault = \"evaluate\"\n\n[[policy.rules]]\nmember = \"fetch\"\n\
         action = \"deny\"\n",
    );
    let cfg = Config::load(tmp.path(), Some(&global)).expect("a repo may restrict");
    assert_eq!(
        cfg.policy,
        parse(OPERATOR).policy,
        "the repo's rules are kept apart"
    );
    assert_eq!(cfg.operator().policy, cfg.policy);

    // Written out and read back, a merged config keeps the operator's rules: only the
    // repository's restrictions, which are not serialized, are lost.
    let text = toml::to_string(&cfg).expect("serializes");
    assert_eq!(parse(&text).policy, parse(OPERATOR).policy);
}

#[test]
fn a_missing_global_file_still_keeps_the_repos_rules_out_of_the_operators() {
    let tmp = tempdir().unwrap();
    write_repo_cfg(tmp.path(), "[policy]\ndefault = \"deny\"\n");
    let cfg = Config::load(tmp.path(), Some(&tmp.path().join("absent.toml"))).expect("loads");
    assert_eq!(cfg.policy, PolicyConfig::default());
    assert_eq!(cfg.operator().policy, PolicyConfig::default());
}

const MODELS: &str = r#"
[providers.p]
style    = "openai"
base-url = "https://operator.example/v1"
api-key  = "${OPERATOR_KEY}"

[models.small]
provider   = "p"
identifier = "operator-small"

[models.fast]
alias = ["small"]
"#;

/// A repository config that replaces the global `[models.fast]` alias, and the provider it
/// reaches, leaves the operator's layer resolving `fast` to the global rows and the global
/// endpoint -- what `0003-25`'s evaluator reads -- while the merged config resolves it to the
/// repository's, as before.
#[test]
fn a_repo_cannot_redefine_the_operators_model_names() {
    let tmp = tempdir().unwrap();
    let global = write_global_cfg(tmp.path(), MODELS);
    write_repo_cfg(
        tmp.path(),
        r#"
[providers.p]
style    = "openai"
base-url = "https://repo.example/v1"
api-key  = "${REPO_KEY}"

[models.big]
provider   = "p"
identifier = "repo-big"

[models.fast]
alias = ["big"]
"#,
    );
    let cfg = Config::load(tmp.path(), Some(&global)).expect("loads");
    let base_url = |cfg: &Config| match &cfg.providers["p"] {
        LlmProvider::OpenAi { base_url, .. } => base_url.clone(),
        other => panic!("{other:?}"),
    };

    assert_eq!(cfg.model_candidates("fast").expect("walks"), ["big"]);
    assert_eq!(base_url(&cfg), "https://repo.example/v1");

    let operator = cfg.operator();
    assert_eq!(operator.model_candidates("fast").expect("walks"), ["small"]);
    assert_eq!(
        operator.models["small"].identifier.as_deref(),
        Some("operator-small")
    );
    assert_eq!(base_url(operator), "https://operator.example/v1");
    assert!(!operator.models.contains_key("big"));
}

/// A config a program built -- here by parsing, with no file layer -- is its own operator's
/// layer; merging it under a repository's keeps it as that.
#[test]
fn a_config_with_no_kept_layer_is_the_operators() {
    let built = parse(MODELS);
    assert!(std::ptr::eq(built.operator(), &built));

    let merged = merge(built.clone(), parse("[models.fast]\nalias = [\"small\"]\n"));
    assert_eq!(merged.operator(), &built);
    let again = merge(merged.clone(), Config::default());
    assert_eq!(
        again.operator(),
        &built,
        "the first global stays the operator's"
    );
}
