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
use outrig_cli::image_setup::render::{self, BaseImage, McpServer, Toolchain};

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

/// The sidecar examples on the config reference and the containers page mount
/// `~/.cache/example`, and `outrig mcp self` hands both pages to agents as well
/// as readers. Until `~` meant the home directory, that resolved to
/// `<repo>/~/.cache/example`, so a config holding either example could never
/// load (#187). A host path that resolves to a `~`-named directory under the
/// repo is that contradiction again, whatever the examples go on to name. Only
/// under the repo: the home directory's own name may start with `~`.
#[test]
fn the_sidecar_mount_examples_resolve_as_written() {
    let repo = Path::new("/srv/repo");
    for (page, heading) in [
        ("doc/reference/config.md", "\n## `[sidecars.<sc>]`\n"),
        ("doc/concepts/containers.md", "\n## Sidecar containers\n"),
    ] {
        let path = workspace_root().join(page);
        let markdown =
            std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        let example = fenced_block_after(&markdown, heading, "toml");
        let cfg = Config::load_from_str(&example)
            .and_then(|cfg| cfg.validate(None).map(|()| cfg))
            .unwrap_or_else(|e| panic!("the sidecar example in {page} does not load: {e}"));

        let mounts: Vec<_> = cfg.sidecars.values().flat_map(|sc| &sc.mounts).collect();
        assert!(
            !mounts.is_empty(),
            "the sidecar example in {page} mounts nothing"
        );
        for mount in mounts {
            let resolved = mount.resolved_host_path(repo);
            assert!(
                !resolved
                    .strip_prefix(repo)
                    .is_ok_and(|rest| rest.to_string_lossy().starts_with('~')),
                "{page}: host-path {:?} resolves to {resolved:?}",
                mount.host_path(),
            );
        }
    }
}

/// The Dockerfile the image page shows is the one `outrig image add` writes
/// for the page's own walkthrough: the default base, `rust, node`, and the
/// default `fs`. The page is a copy, and `outrig mcp self` and `outrig design
/// prompt` hand it to whatever designs an image, so a template fix that misses
/// it goes on being taught -- as #183's `/root/.cargo` install was.
#[test]
fn the_image_pages_dockerfile_is_what_image_add_writes() {
    let page = workspace_root().join("doc/usage/image.md");
    let markdown =
        std::fs::read_to_string(&page).unwrap_or_else(|e| panic!("{}: {e}", page.display()));
    let shown = fenced_block_after(&markdown, "\n## `outrig image add`\n", "Dockerfile");
    let written = render::render(
        BaseImage::DebianBookwormSlim,
        &[Toolchain::Rust, Toolchain::Node],
        &[McpServer::Fs],
    );

    assert_eq!(
        shown,
        written,
        "the Dockerfile {} shows is not the one `outrig image add` writes for its walkthrough",
        page.display()
    );
}
