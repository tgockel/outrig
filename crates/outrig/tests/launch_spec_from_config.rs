//! Integration tests for what `LaunchSpec::from_config` lowers out of a
//! parsed `Config`, written as an external consumer sees it.
//!
//! The `[network]` block is the subject: it was the one security-relevant
//! block the constructor did not read, so a config that declared `audit` or
//! `filter` produced a spec with interception off. These assert the lowering
//! rather than the enforcement -- `library_surface.rs` carries the e2e half
//! that checks an interceptor actually starts.
//!
//! Every config here is sidecar-free, which is what keeps the file out of the
//! `e2e` feature: `from_config` only reaches podman to resolve a sidecar image.

use std::path::Path;

use tempfile::tempdir;

use outrig::LaunchSpec;
use outrig::config::{Config, NetworkAction, NetworkEntry, NetworkMode, NetworkPolicy, merge};

/// A global `[network]` block with a mode and a full policy, as an operator
/// would write one. Every filter-side test resolves against exactly this.
const GLOBAL_NETWORK: &str = r#"
[network]
mode    = "filter"
default = "allow"
allow   = ["github.com:443", "*.npmjs.org"]
deny    = ["*:22"]
"#;

/// The two non-filter modes, spelled out rather than left to silence, so the
/// tests below distinguish "declared `default`" from "declared nothing".
const DEFAULT_NETWORK: &str = r#"
[network]
mode = "default"
"#;

const AUDIT_NETWORK: &str = r#"
[network]
mode = "audit"
"#;

fn parse(s: &str) -> Config {
    Config::load_from_str(s).expect("config parses")
}

/// A config declaring one `[images.primary]` block plus `network`, which is
/// the minimum `from_config` accepts.
fn config_with_network(network: &str) -> Config {
    parse(&format!(
        r#"
[images.primary]
image-name = "localhost/outrig-unused:latest"
{network}"#
    ))
}

/// Lower `cfg`'s `primary` image. Both tempdirs are scratch: nothing here
/// launches, so the log directory the spec names is never written to.
async fn lower(cfg: &Config) -> LaunchSpec {
    let session = tempdir().expect("tempdir session");
    let repo_root = tempdir().expect("tempdir repo_root");
    LaunchSpec::from_config(
        cfg,
        "primary",
        repo_root.path(),
        session.path().join("logs"),
    )
    .await
    .expect("from_config lowers a sidecar-free config")
}

/// The expected filter policy: `allow` by default, with the two allow entries
/// and the one deny entry `GLOBAL_NETWORK` declares, in declaration order.
fn global_policy() -> NetworkPolicy {
    NetworkPolicy::builder()
        .default_action(NetworkAction::Allow)
        .allow_host_port("github.com", 443)
        .allow_host("*.npmjs.org")
        .deny_host_port("*", 22)
        .build()
        .expect("policy builds")
}

#[tokio::test]
async fn default_mode_lowers_with_no_policy() {
    let cfg = config_with_network(DEFAULT_NETWORK);
    let spec = lower(&cfg).await;

    assert_eq!(spec.network.mode, NetworkMode::Default);
    assert_eq!(
        spec.network.policy, None,
        "a non-filter spec must carry no rules at all: one that holds them \
         arms itself the moment a caller assigns to `mode`",
    );
}

#[tokio::test]
async fn absent_network_block_lowers_as_default() {
    let cfg = config_with_network("");
    let spec = lower(&cfg).await;

    assert_eq!(spec.network.mode, NetworkMode::Default);
    assert_eq!(spec.network.policy, None);
}

#[tokio::test]
async fn audit_mode_lowers_with_no_policy() {
    let cfg = config_with_network(AUDIT_NETWORK);
    let spec = lower(&cfg).await;

    assert_eq!(
        spec.network.mode,
        NetworkMode::Audit,
        "a config asking for audit must not lower as `default`, which is the \
         mode that starts no interceptor",
    );
    assert_eq!(
        spec.network.policy, None,
        "audit takes its allow-everything policy from the interceptor; a copy \
         of the config's rules here is dead weight that can be armed",
    );
}

#[tokio::test]
async fn filter_mode_lowers_the_whole_policy() {
    let cfg = config_with_network(GLOBAL_NETWORK);
    cfg.validate(None).expect("the filter policy is valid");
    let spec = lower(&cfg).await;

    assert_eq!(spec.network.mode, NetworkMode::Filter);
    assert_eq!(
        spec.network.policy.as_ref(),
        Some(&global_policy()),
        "lowering the mode and dropping `default`/`allow`/`deny` is the same \
         bug one layer down",
    );
}

/// The path a real embedder takes: `merge` first, then lower. 0002-38 made the
/// repo side able to choose the mode and nothing else, and that rule has to
/// still hold at launch -- a lowering that read the repo block directly would
/// undo it.
#[tokio::test]
async fn merged_config_lowers_the_merged_policy() {
    let global = config_with_network(GLOBAL_NETWORK);
    let repo = parse("[network]\nmode = \"audit\"\n");

    let merged = merge(global, repo);
    let spec = lower(&merged).await;

    assert_eq!(
        spec.network.mode,
        NetworkMode::Audit,
        "the repo's declared mode reaches the spec",
    );
    assert_eq!(spec.network.policy, None);

    // ... and with the repo asking for the mode the policy belongs to, the
    // operator's rules arrive intact.
    let global = config_with_network(GLOBAL_NETWORK);
    let merged = merge(global, parse("[network]\nmode = \"filter\"\n"));
    let spec = lower(&merged).await;

    assert_eq!(spec.network.mode, NetworkMode::Filter);
    assert_eq!(spec.network.policy, Some(global_policy()));
}

/// A repo config carrying its own `allow` cannot widen what the spec enforces.
/// `merge` never reads repo policy keys, so the widened entry is absent from
/// the merged config and therefore from the lowering -- and the same repo file
/// is rejected outright at load time.
#[tokio::test]
async fn repo_policy_cannot_reach_the_lowered_spec() {
    let global = config_with_network(GLOBAL_NETWORK);
    let repo = parse(
        r#"
[network]
mode  = "filter"
allow = ["evil.example:443"]
"#,
    );
    repo.validate_as_repo()
        .expect_err("a repo config declaring policy is rejected on load");

    let merged = merge(global, repo);
    let spec = lower(&merged).await;

    let policy = spec.network.policy.expect("filter mode carries a policy");
    assert_eq!(policy, global_policy());
    assert!(
        !policy
            .allow
            .contains(&NetworkEntry::with_port("evil.example", 443)),
        "a repo-declared allow entry must not reach the launch spec, got: {:?}",
        policy.allow,
    );
}

/// `from_config` resolves repo-relative paths against `repo_root`; the network
/// lowering must not have disturbed that. Cheap regression guard on the
/// neighbors of the line this task changed.
#[tokio::test]
async fn lowering_network_leaves_the_workspace_alone() {
    let repo_root = tempdir().expect("tempdir repo_root");
    let session = tempdir().expect("tempdir session");
    let cfg = parse(&format!(
        r#"
[workspace]
host-path      = "{root}"
container-path = "/workspace"

[images.primary]
image-name = "localhost/outrig-unused:latest"
{GLOBAL_NETWORK}"#,
        root = repo_root.path().display(),
    ));

    let spec = LaunchSpec::from_config(
        &cfg,
        "primary",
        repo_root.path(),
        session.path().join("logs"),
    )
    .await
    .expect("from_config");

    let ws = spec.workspace.expect("a declared workspace is lowered");
    assert_eq!(ws.container, Path::new("/workspace"));
    assert_eq!(spec.network.mode, NetworkMode::Filter);
}
