//! The rules `crates/outrig/public-api.txt` has to keep.
//!
//! 0002-43 settled where the MCP SDK stops being an implementation detail: an
//! SDK type is public only where the item exists to participate in the SDK's
//! own machinery. After 0002-47 applied that to the error surface, the rule
//! has a mechanical form -- every `rmcp::` in the snapshot is under
//! `outrig::mcp_proxy`, and nowhere else.
//!
//! The second is broader: the snapshot names only the crates listed in
//! `PUBLIC_CRATES`. rig and reqwest, which the agent loop is built on, appear
//! nowhere -- the loop's entry point was designed so that they need not, and
//! `plan/phase/0003-python/crate-split-tradeoffs.md` records why.
//!
//! This is not the snapshot gate in `scripts/check-public-api.py`, and the two
//! are not symmetric. That one says "the file is current"; this one says "the
//! file obeys the boundary". The second used to be worth only what the first
//! was, because nothing verified the snapshot's currency and a surface change
//! that was never regenerated left this green. This repo's CI now regenerates
//! and fails on any difference, so here the committed file is fresh data. In a
//! vendored or unpacked copy it is only as fresh as the release it shipped
//! with, which is the right thing for it to be and is why both it and this
//! test are `exclude`d from the package.
//!
//! What it catches that a byte-exact diff does not is a *deliberate*
//! regeneration carrying an SDK type back into `outrig::error` -- the diff
//! would accept that as the new truth.
//!
//! The rule stays here rather than moving into the generator. Reading the
//! committed file costs neither nightly nor `cargo-public-api`, so it runs on
//! every `cargo test` rather than only where the pinned toolchain is
//! available; restating it in Python would create the second copy it exists to
//! prevent.

use std::path::Path;

/// The snapshot's item lines. The header is `# ...`; an item line may start
/// with `#[non_exhaustive]` and must not be skipped along with it.
fn snapshot_items() -> Vec<String> {
    let snapshot = Path::new(env!("CARGO_MANIFEST_DIR")).join("public-api.txt");
    let text = std::fs::read_to_string(&snapshot)
        .unwrap_or_else(|e| panic!("read {}: {e}", snapshot.display()));
    text.lines()
        .filter(|line| !line.starts_with('#') || line.starts_with("#["))
        .map(str::to_string)
        .collect()
}

#[test]
fn mcp_sdk_types_reach_the_public_surface_only_through_mcp_proxy() {
    let items = snapshot_items();
    let sdk_lines: Vec<&str> = items
        .iter()
        .map(String::as_str)
        .filter(|line| line.contains("rmcp::"))
        .collect();

    // Without this, a snapshot truncated to nothing would pass the real check
    // below by saying nothing at all.
    assert!(
        !sdk_lines.is_empty(),
        "the proxy's SDK-typed surface is frozen, so some lines must name `rmcp::`; \
         an empty result means public-api.txt is not what it should be",
    );

    let escaped: Vec<&str> = sdk_lines
        .into_iter()
        .filter(|line| !line.contains("outrig::mcp_proxy::"))
        .collect();
    assert!(
        escaped.is_empty(),
        "an MCP SDK type escaped `outrig::mcp_proxy`, so an SDK major would be an \
         outrig major for more than the proxy:\n  {}",
        escaped.join("\n  ")
    );
}

/// The crates the public surface may name: the standard library, this crate,
/// and the dependencies whose types are deliberately part of the API.
///
/// A dependency not listed here is private, and a type of its showing up in
/// the snapshot means a release of it can force an outrig major. rig and
/// reqwest, which the agent loop is built on, are the ones this was added for.
/// Adding a crate here is a decision to make it public, and should be taken
/// as one.
const PUBLIC_CRATES: &[&str] = &[
    "outrig",
    "core",
    "alloc",
    "std",
    "rmcp",
    "schemars",
    "serde_core",
    "serde_json",
    "tempfile",
    "tokio",
    "toml",
];

/// The first segment of every path on `line`. The snapshot prints a type by
/// the crate that defines it (`alloc::`, `serde_core::`), whatever name it was
/// imported under, so the first segment is the defining crate.
fn path_roots(line: &str) -> Vec<&str> {
    let mut roots = Vec::new();
    let mut from = 0;
    while let Some(at) = line[from..].find("::") {
        let sep = from + at;
        let start = line[..sep]
            .rfind(|c: char| !(c.is_alphanumeric() || c == '_'))
            .map_or(0, |i| i + 1);
        // A segment after `::` is not a root; `<T as X>::Y` has no segment.
        if start < sep && !line[..start].ends_with(':') {
            roots.push(&line[start..sep]);
        }
        from = sep + 2;
    }
    roots
}

#[test]
fn path_roots_are_first_segments_only() {
    assert_eq!(
        path_roots("pub fn outrig::f(&rig_core::a::B) -> <S as serde_core::C>::Ok"),
        ["outrig", "rig_core", "serde_core"]
    );
}

/// Whether `root` names a type rather than a crate: `Self`, or a blanket
/// impl's parameter, whose methods the snapshot prints as `F::method`. A crate
/// name is lower-case, so an upper-case first letter is never one.
fn is_type_parameter(root: &str) -> bool {
    root.starts_with(|c: char| c.is_ascii_uppercase())
}

#[test]
fn a_type_parameter_is_not_a_crate() {
    assert!(is_type_parameter("Self") && is_type_parameter("F"));
    assert!(!is_type_parameter("rig_core") && !is_type_parameter("outrig"));
}

#[test]
fn the_public_surface_names_only_the_public_crates() {
    let leaked: Vec<String> = snapshot_items()
        .into_iter()
        .filter(|line| {
            path_roots(line)
                .iter()
                .any(|root| !is_type_parameter(root) && !PUBLIC_CRATES.contains(root))
        })
        .collect();
    assert!(
        leaked.is_empty(),
        "a crate outside PUBLIC_CRATES reached the public surface, so a release of it \
         would be an outrig major:\n  {}",
        leaked.join("\n  ")
    );
}

/// The session API is the public way into the agent loop, and the provisional
/// entry point it replaced is gone: `outrig::harness` and its session are in
/// the snapshot, and no `PythonAgent` or crate-root `UserChannel` is.
#[test]
fn the_harness_is_public_and_python_agent_is_gone() {
    let items = snapshot_items();
    for want in [
        "pub mod outrig::harness",
        "pub mod outrig::harness::event",
        "pub struct outrig::harness::Session",
        "pub struct outrig::harness::SessionBuilder",
        "pub struct outrig::harness::ShutdownReport",
        "pub struct outrig::harness::event::Subscription",
    ] {
        assert!(
            items.iter().any(|line| line.ends_with(want)),
            "the snapshot has no `{want}`"
        );
    }
    let stale: Vec<&String> = items
        .iter()
        .filter(|line| line.contains("PythonAgent") || line.contains("outrig::UserChannel"))
        .collect();
    assert!(
        stale.is_empty(),
        "the replaced entry point is still public: {stale:#?}"
    );
}

/// Every history, view and event type the harness publishes takes a field or a
/// variant without a break: `#[non_exhaustive]`, so an embedder reads them and
/// constructs none. The exceptions have private fields, which is the same
/// promise: an id, an agent's name, and a subscription.
#[test]
fn every_harness_event_type_grows_without_a_break() {
    const PRIVATE_FIELDS: &[&str] = &[
        "outrig::harness::event::AttemptId",
        "outrig::harness::event::CallId",
        "outrig::harness::event::ExecId",
        "outrig::harness::event::MessageId",
        "outrig::harness::event::Subject",
        "outrig::harness::event::Subscription",
    ];
    let open: Vec<String> = snapshot_items()
        .into_iter()
        .filter(|line| {
            (line.starts_with("pub struct outrig::harness::event::")
                || line.starts_with("pub enum outrig::harness::event::"))
                && !PRIVATE_FIELDS.iter().any(|name| {
                    line.split_whitespace()
                        .nth(2)
                        .is_some_and(|item| item.trim_end_matches("(_)") == *name)
                })
        })
        .collect();
    assert!(
        open.is_empty(),
        "a harness event type a later field or variant would break: {open:#?}"
    );
}
