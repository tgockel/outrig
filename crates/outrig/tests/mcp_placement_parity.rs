//! Config/library parity for `[images.<n>.mcp]` placement: every shape the TOML
//! can express is reachable through `McpServerSpec`'s constructors, and a
//! constructed spec answers to the same validation rules a parsed one does.
//!
//! The shape this file exists for is the named-sidecar entrypoint-stdio server
//! -- no `command`, a `sidecar` naming a `[sidecars.<sc>]` block -- which had no
//! constructor until `entrypoint_in_sidecar`.
//!
//! `fixtures/builtin-default.toml` is a byte copy of
//! `crates/outrig-cli/src/builtin_image/default.toml`, not an `include_str!`
//! of it: `crates/outrig` is published with its `tests/` directory, and a path
//! reaching into a sibling crate would not travel into the `.crate`. The copy
//! is held to the original by
//! `builtin_image::tests::the_parity_fixture_is_a_copy_of_this_config`.
//!
//! Deliberately not `#![cfg(feature = "e2e")]`, unlike `library_surface.rs`:
//! nothing here starts a container, and the subject is a code path with no
//! in-tree caller, so gating it behind a feature CI only compiles would leave it
//! exercised nowhere.

use std::path::Path;

use outrig::config::{
    Config, ConfigValidationError, ImageConfig, McpServerSpec, SidecarConfig, SidecarView,
};
use outrig::error::OutrigError;

const BUILTIN_DEFAULT: &str = include_str!("fixtures/builtin-default.toml");

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

/// An `[images.coding]` block plus whatever top-level blocks are appended.
fn image_block(rest: &str) -> Config {
    parse(&format!(
        r#"
[images.coding]
dockerfile = "D"
context    = "ctx"
{rest}
"#
    ))
}

#[test]
fn entrypoint_in_sidecar_builds_the_named_sidecar_shape() {
    let spec = McpServerSpec::entrypoint_in_sidecar("sc");

    assert_eq!(spec.command(), None, "got: {spec:?}");
    assert_eq!(spec.sidecar(), Some("sc"), "got: {spec:?}");
    assert_eq!(spec.image(), None, "got: {spec:?}");
    assert!(spec.is_entrypoint_stdio(), "got: {spec:?}");

    // The rest of `Full` stays where the TOML defaults leave it, so the entry
    // serializes as the two-key table the config path writes.
    assert!(spec.args().is_empty(), "got: {spec:?}");
    assert_eq!(spec.view(), SidecarView::None, "got: {spec:?}");
    assert!(spec.env().is_empty(), "got: {spec:?}");
}

/// The parity claim at one-entry granularity: the constructor must land on the
/// same value `{ sidecar = "..." }` deserializes to, not merely on something
/// that passes the same accessors.
#[test]
fn entrypoint_in_sidecar_equals_what_the_toml_parses_to() {
    let cfg = image_block(
        r#"
  [sidecars.tools]
  image = "mcp-tools"

  [images.coding.mcp]
  fs = { sidecar = "tools" }
"#,
    );
    cfg.validate(None).expect("the parsed form is valid");

    assert_eq!(
        cfg.images["coding"].mcp["fs"],
        McpServerSpec::entrypoint_in_sidecar("tools"),
    );
}

/// The parity claim at whole-config granularity, against the config outrig
/// itself ships: an embedder must be able to build it without writing TOML.
#[test]
fn the_builtin_default_shape_is_reachable_from_constructors() {
    let mut built = Config::default();

    let mut primary = ImageConfig::from_image_name("docker.io/library/buildpack-deps:bookworm-scm");
    primary.mcp.insert(
        "fs".to_string(),
        McpServerSpec::entrypoint_in_sidecar("outrig-default-fs"),
    );
    primary.mcp.insert(
        "shell".to_string(),
        McpServerSpec::entrypoint_in_sidecar("outrig-default-shell"),
    );
    built.images.insert("outrig-default".to_string(), primary);
    built.images.insert(
        "outrig-default-fs".to_string(),
        ImageConfig::from_image_name("docker.io/mcp/filesystem:latest"),
    );

    let mut fs = SidecarConfig::new("outrig-default-fs");
    fs.view = SidecarView::Primary;
    fs.args = vec!["/workspace".to_string()];
    built.sidecars.insert("outrig-default-fs".to_string(), fs);

    let mut shell = SidecarConfig::new("outrig-default-shell");
    shell.view = SidecarView::Primary;
    built
        .sidecars
        .insert("outrig-default-shell".to_string(), shell);

    built
        .validate(None)
        .expect("the hand-built config is subject to, and passes, the config rules");
    assert_eq!(built, parse(BUILTIN_DEFAULT));
}

/// Parity cuts both ways: a constructed spec is not exempt from the rule that a
/// `sidecar` names a block that exists.
#[test]
fn entrypoint_in_sidecar_requires_a_declared_block() {
    let mut cfg = image_block("");
    cfg.images
        .get_mut("coding")
        .expect("the image block parsed")
        .mcp
        .insert(
            "fs".to_string(),
            McpServerSpec::entrypoint_in_sidecar("nope"),
        );

    let err = expect_validation_err(&cfg, None);
    match err {
        ConfigValidationError::McpUnknownSidecar {
            image,
            server,
            sidecar,
        } => {
            assert_eq!(image, "coding");
            assert_eq!(server, "fs");
            assert_eq!(sidecar, "nope");
        }
        other => panic!("expected McpUnknownSidecar, got: {other:?}"),
    }
}

/// The new constructor is the way to reach the named-block entrypoint shape
/// *because* stacking `with_sidecar` onto the anonymous form is still rejected.
#[test]
fn an_image_and_a_sidecar_are_still_a_placement_conflict() {
    let mut cfg = image_block(
        r#"
  [sidecars.tools]
  image = "mcp-tools"
"#,
    );
    cfg.images
        .get_mut("coding")
        .expect("the image block parsed")
        .mcp
        .insert(
            "fs".to_string(),
            McpServerSpec::entrypoint("ghcr.io/example/mcp-fs:1").with_sidecar("tools"),
        );

    let err = expect_validation_err(&cfg, None);
    assert!(
        matches!(
            err,
            ConfigValidationError::McpPlacementConflict { ref server, .. }
                if server == "fs"
        ),
        "got: {err:?}"
    );
}

/// `with_args` becomes reachable in combination with a named block only now, so
/// both halves of "declare the arguments in one place" are newly library-facing.
#[test]
fn entrypoint_in_sidecar_carries_args_the_block_does_not() {
    let mut cfg = image_block(
        r#"
  [sidecars.tools]
  image = "mcp-tools"
"#,
    );
    cfg.images
        .get_mut("coding")
        .expect("the image block parsed")
        .mcp
        .insert(
            "fs".to_string(),
            McpServerSpec::entrypoint_in_sidecar("tools").with_args(["/workspace"]),
        );

    cfg.validate(None)
        .expect("a block without `args` leaves the entry free to declare them");
}

#[test]
fn entrypoint_in_sidecar_rejects_args_the_block_already_declares() {
    let mut cfg = image_block(
        r#"
  [sidecars.tools]
  image = "mcp-tools"
  args  = ["/workspace"]
"#,
    );
    cfg.images
        .get_mut("coding")
        .expect("the image block parsed")
        .mcp
        .insert(
            "fs".to_string(),
            McpServerSpec::entrypoint_in_sidecar("tools").with_args(["/elsewhere"]),
        );

    let err = expect_validation_err(&cfg, None);
    assert!(
        matches!(
            err,
            ConfigValidationError::McpArgsDeclaredTwice { ref sidecar, .. }
                if sidecar == "tools"
        ),
        "got: {err:?}"
    );
}
