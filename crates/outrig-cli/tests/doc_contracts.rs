//! Doc examples, executed.
//!
//! `doc/` is design-first and its examples are a contract: the minimal config
//! on a usage page is the one a new reader copies first, so it is the one that
//! has to parse. This reads the page rather than a transcription of it, so an
//! edit that reintroduces a key the schema rejects fails here instead of at the
//! reader's first `outrig mcp`. `doc/usage/mcp.md` advertised `[workspace]
//! root = "."` for three releases; `Workspace` is `deny_unknown_fields` and has
//! no `root`.

use std::path::Path;

use outrig::config::Config;

/// `doc/` lives at the workspace root; this crate's manifest dir is
/// `crates/outrig-cli`. Same walk as `prompt_doc_sync.rs`.
fn workspace_root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root")
}

/// The first fenced `lang` block following `heading` in `markdown`.
fn fenced_block_after(markdown: &str, heading: &str, lang: &str) -> String {
    let after_heading = markdown
        .split_once(heading)
        .unwrap_or_else(|| panic!("no {heading:?} heading"))
        .1;
    let fence = format!("```{lang}\n");
    after_heading
        .split_once(fence.as_str())
        .unwrap_or_else(|| panic!("no ```{lang} block under {heading:?}"))
        .1
        .split_once("```")
        .unwrap_or_else(|| panic!("unterminated ```{lang} block under {heading:?}"))
        .0
        .to_string()
}

#[test]
fn the_mcp_pages_minimal_config_parses() {
    let page = workspace_root().join("doc/usage/mcp.md");
    let markdown =
        std::fs::read_to_string(&page).unwrap_or_else(|e| panic!("{}: {e}", page.display()));
    let example = fenced_block_after(&markdown, "\n## Minimal Config\n", "toml");

    Config::load_from_str(&example).unwrap_or_else(|e| {
        panic!(
            "the minimal config in {} does not parse: {e}\n--- example ---\n{example}",
            page.display()
        )
    });
}
