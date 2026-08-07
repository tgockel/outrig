//! Config-layer tests for `[models.<name>].alias` (task 0110): the flattening
//! walk's ordering and cycle rules, and the five validation rules that guard
//! the two model shapes.
//!
//! The resolver's half -- which candidate a build actually selects, and what it
//! prints -- lives in `outrig-cli`'s `tests/llm_resolve.rs`.

use std::fs;
use std::path::Path;

use tempfile::tempdir;

use outrig::config::{Config, ConfigValidationError, Model};
use outrig::error::OutrigError;

fn parse(s: &str) -> Config {
    Config::load_from_str(s).expect("config parses")
}

fn expect_validation_err(cfg: &Config, repo_root: Option<&Path>) -> ConfigValidationError {
    match cfg.validate(repo_root) {
        Err(OutrigError::ConfigValidation(e)) => e,
        Err(other) => panic!("expected ConfigValidation, got: {other:?}"),
        Ok(()) => panic!("expected validation error, got Ok"),
    }
}

/// One provider plus four concrete rows, so alias tests can name real targets
/// without restating the plumbing.
const CONCRETE: &str = r#"
[providers.p]
style    = "openai"
base-url = "https://example.invalid/v1"
api-key  = "${OUTRIG_TEST_ALIAS_KEY}"

[models.opus-5-bedrock]
provider   = "p"
identifier = "anthropic.claude-opus-5-v1:0"

[models.opus-5-anthropic]
provider   = "p"
identifier = "claude-opus-5"

[models.opus-5-azure]
provider   = "p"
identifier = "claude-opus-5-azure"

[models.haiku-5]
provider   = "p"
identifier = "claude-haiku-5"
"#;

fn with_aliases(aliases: &str) -> Config {
    parse(&format!("{CONCRETE}{aliases}"))
}

/// The task's own worked example: an alias naming an alias splices the nested
/// targets in at its position, so the result is depth-first in config order.
#[test]
fn nested_alias_flattens_depth_first_in_config_order() {
    let cfg = with_aliases(
        r#"
[models.smart]
alias = ["opus-5-bedrock", "opus-5-anthropic", "opus-5-azure"]

[models.default]
alias = ["smart", "haiku-5"]
"#,
    );
    cfg.validate(None).expect("valid");
    assert_eq!(
        cfg.model_candidates("default").expect("walks"),
        vec![
            "opus-5-bedrock",
            "opus-5-anthropic",
            "opus-5-azure",
            "haiku-5"
        ]
    );
}

/// A concrete row flattens to itself, so every caller can walk unconditionally
/// rather than testing the shape first.
#[test]
fn a_concrete_model_flattens_to_itself() {
    let cfg = with_aliases("");
    assert_eq!(
        cfg.model_candidates("haiku-5").expect("walks"),
        vec!["haiku-5"]
    );
}

/// Two paths reaching the same target keep the first occurrence, which is what
/// makes the flattened order a function of the config alone.
#[test]
fn repeated_target_keeps_first_occurrence() {
    let cfg = with_aliases(
        r#"
[models.left]
alias = ["opus-5-bedrock", "haiku-5"]

[models.right]
alias = ["opus-5-bedrock", "opus-5-azure"]

[models.both]
alias = ["left", "right"]
"#,
    );
    cfg.validate(None).expect("valid");
    assert_eq!(
        cfg.model_candidates("both").expect("walks"),
        vec!["opus-5-bedrock", "haiku-5", "opus-5-azure"]
    );
}

/// A name listed twice under one alias is a repeat, not a cycle. The
/// distinction is why cycle detection tracks the current path rather than
/// everything already emitted.
#[test]
fn a_repeated_target_is_not_a_cycle() {
    let cfg = with_aliases(
        r#"
[models.twice]
alias = ["haiku-5", "haiku-5"]
"#,
    );
    cfg.validate(None).expect("a repeat is legal");
    assert_eq!(
        cfg.model_candidates("twice").expect("walks"),
        vec!["haiku-5"]
    );
}

/// `alias = "x"` and `alias = ["x"]` are the same config. The single-string
/// form is what "point `opus` somewhere else" should cost.
#[test]
fn alias_string_and_single_element_array_parse_identically() {
    let bare = with_aliases("[models.opus]\nalias = \"haiku-5\"\n");
    let array = with_aliases("[models.opus]\nalias = [\"haiku-5\"]\n");
    assert_eq!(bare, array);
    assert_eq!(
        bare.models["opus"].alias.as_deref(),
        Some(&["haiku-5".to_string()][..])
    );
}

#[test]
fn alias_cycle_fails_validate() {
    let cfg = with_aliases(
        r#"
[models.a]
alias = "b"

[models.b]
alias = "a"
"#,
    );
    let err = expect_validation_err(&cfg, None);
    match &err {
        ConfigValidationError::ModelAliasCycle { cycle, .. } => {
            assert_eq!(cycle, "a -> b -> a", "the message names the cycle")
        }
        other => panic!("expected ModelAliasCycle, got: {other:?}"),
    }
}

/// A one-row cycle is still a cycle, and the rendering degrades sensibly.
#[test]
fn self_referential_alias_fails_validate() {
    let cfg = with_aliases("[models.a]\nalias = \"a\"\n");
    let err = expect_validation_err(&cfg, None);
    match &err {
        ConfigValidationError::ModelAliasCycle { cycle, .. } => assert_eq!(cycle, "a -> a"),
        other => panic!("expected ModelAliasCycle, got: {other:?}"),
    }
}

#[test]
fn dangling_alias_target_fails_validate() {
    let cfg = with_aliases("[models.opus]\nalias = [\"haiku-5\", \"ghost\"]\n");
    let err = expect_validation_err(&cfg, None);
    match &err {
        ConfigValidationError::UnknownModelAliasTarget { model, target, .. } => {
            assert_eq!(model, "opus");
            assert_eq!(target, "ghost", "the message names the dangling target");
        }
        other => panic!("expected UnknownModelAliasTarget, got: {other:?}"),
    }
}

#[test]
fn empty_alias_list_fails_validate() {
    let cfg = with_aliases("[models.opus]\nalias = []\n");
    let err = expect_validation_err(&cfg, None);
    assert!(
        matches!(&err, ConfigValidationError::ModelAliasEmpty { model, .. } if model == "opus"),
        "expected ModelAliasEmpty, got: {err:?}"
    );
}

/// An alias names other models instead of a provider, so any provider-shape
/// field alongside it is a contradiction -- and the message names every
/// offender rather than only the first.
#[test]
fn alias_with_provider_field_fails_validate() {
    let cfg = with_aliases(
        r#"
[models.opus]
alias      = "haiku-5"
provider   = "p"
identifier = "claude-opus-5"
"#,
    );
    let err = expect_validation_err(&cfg, None);
    match &err {
        ConfigValidationError::ModelSourceConflict { model, fields, .. } => {
            assert_eq!(model, "opus");
            assert!(
                fields.contains(&"alias")
                    && fields.contains(&"provider")
                    && fields.contains(&"identifier"),
                "the message names both keys, got: {fields:?}"
            );
        }
        other => panic!("expected ModelSourceConflict, got: {other:?}"),
    }
}

/// `max-tokens` is a provider-shape field like any other. An alias carrying a
/// ceiling for whichever candidate wins is coherent and may be allowed later;
/// forbidding it now keeps the rule to one clause, and relaxing is additive.
#[test]
fn alias_with_max_tokens_fails_validate() {
    let cfg = with_aliases("[models.opus]\nalias = \"haiku-5\"\nmax-tokens = 4096\n");
    let err = expect_validation_err(&cfg, None);
    assert!(
        matches!(
            &err,
            ConfigValidationError::ModelSourceConflict { fields, .. }
                if fields.contains(&"max-tokens")
        ),
        "expected ModelSourceConflict naming max-tokens, got: {err:?}"
    );
}

#[test]
fn model_with_neither_shape_fails_validate() {
    let cfg = with_aliases("[models.opus]\nidentifier = \"claude-opus-5\"\n");
    let err = expect_validation_err(&cfg, None);
    assert!(
        matches!(&err, ConfigValidationError::ModelSourceMissing { model, .. } if model == "opus"),
        "expected ModelSourceMissing, got: {err:?}"
    );
}

/// The shape and graph rules are *not* gated on `validate-llm`, unlike the
/// provider cross-reference rules `outrig build` skips. Until `provider` became
/// optional, serde's own "missing field" enforced half of this on every path,
/// builds included; gating it would have quietly let a build accept
/// `[models.x]` with nothing in it.
#[test]
fn build_load_still_validates_model_shape_and_alias_graph() {
    for (case, models, needle) in [
        (
            "neither shape",
            "[models.opus]\nidentifier = \"claude-opus-5\"\n",
            "neither `provider` nor `alias`",
        ),
        (
            "a cycle",
            "[models.a]\nalias = \"b\"\n\n[models.b]\nalias = \"a\"\n",
            "model alias cycle: a -> b -> a",
        ),
    ] {
        let tmp = tempdir().unwrap();
        let agents = tmp.path().join(".agents/outrig");
        fs::create_dir_all(&agents).unwrap();
        fs::write(agents.join("config.toml"), format!("{CONCRETE}{models}")).unwrap();

        let err = Config::load_for_build(tmp.path(), None)
            .expect_err(&format!("{case}: build load should reject it"));
        assert!(
            err.to_string().contains(needle),
            "{case}: expected {needle:?}, got: {err}"
        );
    }
}

/// The counterpart of the test above: a build load still skips the *provider*
/// cross-reference rules, so an alias whose target names a typo'd provider is
/// as acceptable to `outrig build` as a direct model with the same typo. Only
/// the shape and graph rules moved out of the gate.
#[test]
fn build_load_still_skips_the_provider_rules_under_an_alias() {
    let tmp = tempdir().unwrap();
    let agents = tmp.path().join(".agents/outrig");
    fs::create_dir_all(&agents).unwrap();
    fs::write(
        agents.join("config.toml"),
        "[models.opus]\nalias = \"typo\"\n\n[models.typo]\nprovider = \"nowhere\"\n",
    )
    .unwrap();

    let cfg = Config::load_for_build(tmp.path(), None)
        .expect("build load does not check that a provider exists");
    assert_eq!(
        cfg.model_candidates("opus").expect("walks"),
        vec!["typo"],
        "the graph is still walked, only the provider lookup is skipped"
    );
}

/// The walk must be total on a `Config` that never went through `validate` --
/// one built by hand in a test, or by a library embedder. A cycle there has to
/// come back as an error rather than recursing forever.
#[test]
fn a_hand_built_cycle_is_rejected_rather_than_hanging() {
    let mut cfg = Config::default();
    cfg.models.insert("a".to_string(), Model::alias(["b"]));
    cfg.models.insert("b".to_string(), Model::alias(["a"]));

    let err = cfg.model_candidates("a").expect_err("cycle");
    assert!(
        matches!(&err, ConfigValidationError::ModelAliasCycle { cycle, .. } if cycle == "a -> b -> a"),
        "got: {err:?}"
    );
}

/// `Model::alias` is the constructor `#[non_exhaustive]` makes necessary, and
/// it round-trips through the same shape validation a parsed config does.
#[test]
fn the_alias_constructor_builds_a_validatable_row() {
    let mut cfg = with_aliases("");
    cfg.models
        .insert("opus".to_string(), Model::alias(["haiku-5"]));
    cfg.validate(None).expect("valid");
    assert_eq!(
        cfg.model_candidates("opus").expect("walks"),
        vec!["haiku-5"]
    );
    assert!(cfg.models["opus"].provider.is_none());
}

/// Deduplicating leaves is not enough on its own: a diamond re-enters the
/// shared alias node once per path that reaches it, so `aN = [aN-1, aN-1]`
/// costs 2^N visits without a completed-node set. 26 rows is ~134 million
/// recursive calls -- a config that is perfectly valid, and that `Config::load`
/// walks on every startup.
///
/// The bound is wall-clock rather than a visit count because the visit count is
/// an implementation detail; what must hold is that ordinary validation of an
/// ordinary config does not hang.
#[test]
fn a_repeated_alias_subgraph_does_not_blow_up() {
    let mut cfg = Config::default();
    cfg.models.insert("a0".to_string(), Model::new("p"));
    for i in 1..=26 {
        cfg.models.insert(
            format!("a{i}"),
            Model::alias([format!("a{}", i - 1), format!("a{}", i - 1)]),
        );
    }

    let start = std::time::Instant::now();
    let candidates = cfg.model_candidates("a26").expect("walks");
    let elapsed = start.elapsed();

    assert_eq!(candidates, vec!["a0"], "every path lands on the one leaf");
    assert!(
        elapsed < std::time::Duration::from_secs(1),
        "traversal must be linear in the graph, not exponential in its depth; took {elapsed:?}"
    );
}

/// A diamond still splices its shared subtree in at the position the *first*
/// path reaches it, which is what the completed-node set must not disturb.
#[test]
fn skipping_an_expanded_alias_preserves_first_occurrence_order() {
    let cfg = with_aliases(
        r#"
[models.shared]
alias = ["opus-5-azure", "haiku-5"]

[models.left]
alias = ["opus-5-bedrock", "shared"]

[models.right]
alias = ["shared", "opus-5-anthropic"]

[models.both]
alias = ["left", "right"]
"#,
    );
    cfg.validate(None).expect("valid");
    assert_eq!(
        cfg.model_candidates("both").expect("walks"),
        vec![
            "opus-5-bedrock",
            "opus-5-azure",
            "haiku-5",
            "opus-5-anthropic"
        ]
    );
}

/// A chain longer than the depth bound reports a bad config rather than
/// exhausting the native stack and aborting the process.
#[test]
fn an_over_deep_alias_chain_is_reported_rather_than_overflowing() {
    let mut cfg = Config::default();
    cfg.models.insert("leaf".to_string(), Model::new("p"));
    let depth = outrig::config::MODEL_ALIAS_DEPTH_MAX + 5;
    cfg.models
        .insert("hop0".to_string(), Model::alias(["leaf"]));
    for i in 1..depth {
        cfg.models
            .insert(format!("hop{i}"), Model::alias([format!("hop{}", i - 1)]));
    }

    let err = cfg
        .model_candidates(&format!("hop{}", depth - 1))
        .expect_err("too deep");
    assert!(
        matches!(&err, ConfigValidationError::ModelAliasTooDeep { max, .. }
            if *max == outrig::config::MODEL_ALIAS_DEPTH_MAX),
        "got: {err:?}"
    );
}

/// A chain right at the bound still resolves, so the limit is off-by-one clean.
#[test]
fn a_chain_at_the_depth_bound_still_resolves() {
    let mut cfg = Config::default();
    cfg.models.insert("leaf".to_string(), Model::new("p"));
    cfg.models
        .insert("hop0".to_string(), Model::alias(["leaf"]));
    for i in 1..outrig::config::MODEL_ALIAS_DEPTH_MAX {
        cfg.models
            .insert(format!("hop{i}"), Model::alias([format!("hop{}", i - 1)]));
    }

    assert_eq!(
        cfg.model_candidates(&format!("hop{}", outrig::config::MODEL_ALIAS_DEPTH_MAX - 1))
            .expect("a chain at the bound is legal"),
        vec!["leaf"]
    );
}

/// The depth bound must not depend on which sibling reached a shared node
/// first. Expanding `shared` from the short branch used to make the long branch
/// short-circuit before its own depth was ever counted, so the same structure
/// passed or failed depending on the order the targets were written in.
#[test]
fn the_depth_bound_does_not_depend_on_sibling_order() {
    // The chain is sized so that *only* `shared` lands on the limit: the
    // deepest chain node sits one hop inside it, so nothing trips the bound
    // except the memoized node itself. A chain long enough to fail on its own
    // would pass this test with the bug still present.
    const MAX: usize = outrig::config::MODEL_ALIAS_DEPTH_MAX;
    const TOP: usize = MAX - 2;

    fn cfg_with(order: [&str; 2]) -> Config {
        let mut cfg = Config::default();
        cfg.models.insert("leaf".to_string(), Model::new("p"));
        cfg.models
            .insert("shared".to_string(), Model::alias(["leaf"]));
        cfg.models
            .insert("chain0".to_string(), Model::alias(["shared"]));
        for i in 1..=TOP {
            cfg.models.insert(
                format!("chain{i}"),
                Model::alias([format!("chain{}", i - 1)]),
            );
        }
        cfg.models.insert(
            "root".to_string(),
            Model::alias([order[0].to_string(), order[1].to_string()]),
        );
        cfg
    }

    let deep = format!("chain{TOP}");
    for order in [["shared", deep.as_str()], [deep.as_str(), "shared"]] {
        let err = cfg_with(order)
            .model_candidates("root")
            .expect_err(&format!("order {order:?} must be rejected"));
        assert!(
            matches!(err, ConfigValidationError::ModelAliasTooDeep { .. }),
            "order {order:?}: got {err:?}"
        );
    }
}

/// Many names over one shared alias is an ordinary shape, not a pathological
/// one. Flattening it afresh per root -- with a linear membership scan per leaf
/// on top -- is cubic, so validating a few hundred rows would stall a load that
/// has linear input.
#[test]
fn many_roots_over_one_shared_alias_validate_quickly() {
    const N: usize = 800;
    let mut cfg = Config::default();
    cfg.providers.insert(
        "p".to_string(),
        outrig::config::LlmProvider::openai(
            "https://example.invalid/v1",
            outrig::config::ApiKeyRef::parse("${OUTRIG_TEST_ALIAS_KEY}").expect("api-key ref"),
            None,
        ),
    );
    let leaves: Vec<String> = (0..N).map(|i| format!("leaf{i}")).collect();
    for leaf in &leaves {
        let mut model = Model::new("p");
        model.identifier = Some("gpt-4o".to_string());
        cfg.models.insert(leaf.clone(), model);
    }
    cfg.models
        .insert("shared".to_string(), Model::alias(leaves.clone()));
    for i in 0..N {
        cfg.models
            .insert(format!("root{i}"), Model::alias(["shared"]));
    }

    // Through the real entry point, so this measures what a load pays.
    let start = std::time::Instant::now();
    cfg.validate(None).expect("the graph is valid");
    let elapsed = start.elapsed();
    // Two orders of magnitude of headroom: re-flattening per root measures
    // ~1.1s at this size against ~0.01s for one shared pass, so the threshold
    // is nowhere near either the pass or the fail.
    assert!(
        elapsed < std::time::Duration::from_millis(500),
        "validating a shared subtree must not be cubic in the table size; took {elapsed:?}"
    );
}

/// Sharing `expanded` across roots is only sound if a broken row can never hide
/// behind an already-walked ancestor. It cannot -- a node is recorded only after
/// completing without error, so nothing under it can still fail -- but that is
/// the property the optimization risks, so it is worth a test that does not
/// depend on wall-clock timing.
#[test]
fn a_broken_row_under_a_shared_subtree_is_still_caught() {
    let mut cfg = Config::default();
    cfg.models.insert("leaf".to_string(), Model::new("p"));
    // Walked first (BTreeMap order), and valid, so `expanded` fills up.
    cfg.models
        .insert("aaa_first".to_string(), Model::alias(["leaf"]));
    // Reachable only from a later root, and dangling.
    cfg.models
        .insert("zzz_broken".to_string(), Model::alias(["ghost"]));
    cfg.models.insert(
        "zzz_root".to_string(),
        Model::alias(["aaa_first", "zzz_broken"]),
    );

    let err = expect_validation_err(&cfg, None);
    assert!(
        matches!(
            &err,
            ConfigValidationError::UnknownModelAliasTarget { target, .. } if target == "ghost"
        ),
        "memoizing across roots must not skip an unvalidated row; got: {err:?}"
    );
}

/// A row that is both shapes at once is rejected by the walk rather than
/// silently treated as an alias, which would discard a `provider` its author
/// meant. Reachable only for a hand-built config -- validation catches it
/// first on every load path -- including when the offending row is *nested*
/// under an otherwise-valid alias.
#[test]
fn a_nested_both_shapes_row_is_rejected_by_the_walk() {
    let mut cfg = Config::default();
    cfg.models.insert("leaf".to_string(), Model::new("p"));
    let mut both = Model::alias(["leaf"]);
    both.provider = Some("p".to_string());
    cfg.models.insert("both".to_string(), both);
    cfg.models
        .insert("root".to_string(), Model::alias(["both"]));

    let err = cfg.model_candidates("root").expect_err("nested conflict");
    assert!(
        matches!(&err, ConfigValidationError::ModelSourceConflict { model, .. } if model == "both"),
        "got: {err:?}"
    );
}

/// `Config::validate` and a fresh `model_candidates` must reach the same
/// verdict on the same config. Sharing `expanded` across roots nearly broke
/// that: with bottom-up lexical names every suffix is already expanded when its
/// parent is walked, so validation never recursed deep enough to notice a chain
/// past the limit -- and the config loaded cleanly, then failed at resolve time.
/// Memoizing each node's subtree *height* is what keeps the two in step.
#[test]
fn validate_and_model_candidates_agree_on_an_over_deep_bottom_up_chain() {
    const MAX: usize = outrig::config::MODEL_ALIAS_DEPTH_MAX;
    let mut cfg = Config::default();
    cfg.providers.insert(
        "p".to_string(),
        outrig::config::LlmProvider::openai(
            "https://example.invalid/v1",
            outrig::config::ApiKeyRef::parse("${OUTRIG_TEST_ALIAS_KEY}").expect("api-key ref"),
            None,
        ),
    );
    let mut leaf = Model::new("p");
    leaf.identifier = Some("gpt-4o".to_string());
    cfg.models.insert("leaf".to_string(), leaf);
    // Zero-padded so the BTreeMap walks the chain bottom-up, which is the
    // ordering that makes every suffix memoized before its parent is reached.
    cfg.models.insert("a00".to_string(), Model::alias(["leaf"]));
    for i in 1..=MAX {
        cfg.models
            .insert(format!("a{i:02}"), Model::alias([format!("a{:02}", i - 1)]));
    }

    let via_validate = expect_validation_err(&cfg, None);
    assert!(
        matches!(
            via_validate,
            ConfigValidationError::ModelAliasTooDeep { .. }
        ),
        "validate must reject it too, got: {via_validate:?}"
    );
    let via_walk = cfg
        .model_candidates(&format!("a{MAX:02}"))
        .expect_err("the chain is past the limit");
    assert!(
        matches!(via_walk, ConfigValidationError::ModelAliasTooDeep { .. }),
        "got: {via_walk:?}"
    );
}

/// The conflict list is the shared collector, so a nested row reports every
/// offending key rather than the two the walk used to hard-code.
#[test]
fn a_nested_conflict_names_every_offending_field() {
    let mut cfg = Config::default();
    cfg.models.insert("leaf".to_string(), Model::new("p"));
    let mut both = Model::alias(["leaf"]);
    both.identifier = Some("gpt-4o".to_string());
    both.max_tokens = Some(4096);
    both.device = Some("cpu".to_string());
    cfg.models.insert("bad".to_string(), both);
    cfg.models.insert("root".to_string(), Model::alias(["bad"]));

    let err = cfg.model_candidates("root").expect_err("nested conflict");
    match &err {
        ConfigValidationError::ModelSourceConflict { model, fields, .. } => {
            assert_eq!(model, "bad");
            for expected in ["alias", "identifier", "device", "max-tokens"] {
                assert!(
                    fields.contains(&expected),
                    "missing {expected:?}: {fields:?}"
                );
            }
            assert!(
                !fields.contains(&"provider"),
                "provider is unset: {fields:?}"
            );
        }
        other => panic!("expected ModelSourceConflict, got: {other:?}"),
    }
}

/// A leaf that names no provider is not a usable candidate, so the walk rejects
/// it rather than emitting it. That is what lets callers filter on this one
/// contract instead of re-deriving the shape rules.
#[test]
fn an_alias_onto_a_shapeless_leaf_is_rejected() {
    let mut cfg = Config::default();
    let mut shapeless = Model::new("p");
    shapeless.provider = None;
    cfg.models.insert("shapeless".to_string(), shapeless);
    cfg.models
        .insert("root".to_string(), Model::alias(["shapeless"]));

    let err = cfg.model_candidates("root").expect_err("shapeless leaf");
    assert!(
        matches!(&err, ConfigValidationError::ModelSourceMissing { model, .. } if model == "shapeless"),
        "got: {err:?}"
    );
}
