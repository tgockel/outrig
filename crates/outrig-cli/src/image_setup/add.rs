//! `outrig image add` -- interactive scaffolding of a new image-config.
//!
//! Walks the user through name, base image, toolchains, and MCP servers, then
//! writes `.agents/outrig/images/<name>/Dockerfile` and appends matching
//! `[images.<name>]` and `[images.<name>.mcp]` blocks to the repo
//! `config.toml`. The TOML mutation goes through `toml_edit` so any
//! surrounding comments and formatting survive. An inline
//! `images = { ... }` can't hold those blocks, so it becomes a standard
//! `[images]` table first; comments between its entries are not kept.
//!
//! `run` constructs real terminal I/O; `run_with` is the test seam that takes
//! an arbitrary `PromptSource`.

use std::path::Path;

use heck::ToKebabCase;
use toml_edit::{Array, Decor, DocumentMut, InlineTable, Item, Key, Table, Value};

use crate::error::{OutrigError, Result};
use crate::image_setup::render::{self, BaseImage, McpServer, Toolchain};
use crate::init::prompt::{self, Field, PromptSource};
use crate::init::repo as init_repo;
use crate::paths::{
    global_config_path, image_dir, image_dir_rel, repo_config_path, write_atomic_all,
};
use outrig::config::{ConfigValidationError, check_build_image_name};
use outrig::error::IoPathExt;

/// CLI entry point. Resolves the repo root from `cwd` (walking up, with a
/// fallback prompt to bootstrap a fresh `.agents/outrig/config.toml` if
/// none is found) before running the interactive image-add flow. One
/// `PromptSource` is threaded through both halves so the user sees a
/// single conversation. `global_override` plumbs `--global-config` into
/// the bootstrap path so the model-section can list models from the
/// right global config.
pub async fn run(
    cwd: &Path,
    global_override: Option<&Path>,
    name: Option<String>,
    force: bool,
) -> Result<()> {
    // Ahead of the bootstrap, which would walk a fresh repo through its
    // prompts and write its config before `run_with` refused the name.
    if let Some(name) = &name {
        check_name(name).map_err(OutrigError::from)?;
    }
    let global_path = global_config_path(global_override);
    let mut prompt = prompt::auto();
    let mut hf = crate::hf::auto();
    let (repo_root, bootstrapped_name) =
        init_repo::resolve_or_bootstrap(cwd, &global_path, &mut prompt, &mut hf).await?;
    // CLI-provided name wins; otherwise reuse whatever the bootstrap
    // already asked for.
    let effective = name.or(bootstrapped_name);
    run_with(&repo_root, effective, force, &mut prompt).await
}

/// Drives the interactive flow against an arbitrary `PromptSource`.
///
/// The config is read and its `images` checked before any prompt. A
/// `name_arg` that can't name a build image is refused next, `--force` or
/// not; one typed at the prompt is asked for again. The idempotency probe
/// (Dockerfile path + existing `[images.<name>]` block) runs right after the
/// name, so neither a config this can't extend nor an accidental re-run
/// burns through the user's input before bailing. Nothing is written until
/// every answer is in.
pub async fn run_with(
    repo_root: &Path,
    name_arg: Option<String>,
    force: bool,
    prompt: &mut impl PromptSource,
) -> Result<()> {
    let cfg_path = repo_config_path(repo_root);
    let mut doc = load_doc(&cfg_path)?;
    let images = images_table(&mut doc, &cfg_path)?;

    let name = match name_arg {
        Some(n) => {
            check_name(&n).map_err(OutrigError::from)?;
            n
        }
        None => ask_name(prompt, repo_root).await?,
    };
    // The key as the config spells it: quoted when the name holds a `.`.
    let key = Key::new(name.as_str());

    let dockerfile_path = image_dir(repo_root, &name).join("Dockerfile");

    if !force {
        if dockerfile_path.exists() {
            return Err(OutrigError::Configuration(format!(
                "{} already exists; pass --force to overwrite.",
                dockerfile_path.display()
            ))
            .into());
        }
        if images.contains_key(&name) {
            return Err(OutrigError::Configuration(format!(
                "[images.{key}] already exists in {}; pass --force to overwrite.",
                cfg_path.display()
            ))
            .into());
        }
    }

    let base_idx = prompt.ask_select(&BASE_FIELD, 0).await?;
    let base = BaseImage::ALL[base_idx];

    let toolchain_indices = prompt.ask_multiselect(&TOOLCHAIN_FIELD, &[]).await?;
    let toolchains: Vec<Toolchain> = toolchain_indices
        .iter()
        .map(|&i| Toolchain::ALL[i])
        .collect();

    // Defaults to `fs` only -- the most-defensible v0 minimum.
    let default_mcps: Vec<usize> = vec![DEFAULT_MCP_INDEX];
    let mcp_indices = prompt.ask_multiselect(&MCP_FIELD, &default_mcps).await?;
    let mcps: Vec<McpServer> = mcp_indices.iter().map(|&i| McpServer::ALL[i]).collect();

    let dockerfile = render::render(base, &toolchains, &mcps);
    insert_image_block(images, &name, &mcps);
    // Staged together, so a config that can't be written doesn't leave a
    // Dockerfile behind to refuse the retry. The Dockerfile lands first, so
    // a config never names one that isn't there.
    write_atomic_all(&[
        (&dockerfile_path, &dockerfile),
        (&cfg_path, &doc.to_string()),
    ])?;
    eprintln!(
        "[outrig] wrote {}",
        display_rel(&dockerfile_path, repo_root)
    );
    eprintln!(
        "[outrig] added [images.{key}] block to {}",
        display_rel(&cfg_path, repo_root)
    );
    if mcps.is_empty() {
        eprintln!("[outrig] [images.{key}.mcp] is empty");
    } else {
        let names: Vec<&str> = mcps.iter().map(|m| m.as_str()).collect();
        eprintln!(
            "[outrig] added [images.{key}.mcp] entries: {}",
            names.join(", ")
        );
    }

    Ok(())
}

// ---- prompt fields --------------------------------------------------------

const NAME_FIELD: Field = Field {
    name: "Image name",
    description: "Names the image-config: its [images.<name>] key, its directory under \
                  .agents/outrig/images/, and the repository of the image built from it. \
                  Lowercase letters and digits, separated by one `.`, one or two `_`, or \
                  a run of `-`, e.g. `rust-dev`.",
    options: &[],
    doc_link: "doc/usage/image.md",
};

const BASES: &[(&str, &str)] = &[
    (
        BaseImage::DebianBookwormSlim.as_str(),
        BaseImage::DebianBookwormSlim.description(),
    ),
    (
        BaseImage::Ubuntu24_04.as_str(),
        BaseImage::Ubuntu24_04.description(),
    ),
    (
        BaseImage::AlpineLatest.as_str(),
        BaseImage::AlpineLatest.description(),
    ),
    (
        BaseImage::Node20BookwormSlim.as_str(),
        BaseImage::Node20BookwormSlim.description(),
    ),
    (
        BaseImage::Python3_12Slim.as_str(),
        BaseImage::Python3_12Slim.description(),
    ),
];

const BASE_FIELD: Field = Field {
    name: "Base image",
    description: "The Dockerfile's `FROM` line. Pick one of the curated starting points.",
    options: BASES,
    doc_link: "doc/usage/image.md",
};

const TOOLCHAINS: &[(&str, &str)] = &[
    (
        "rust",
        "rustup + stable toolchain (cargo, rustfmt, clippy).",
    ),
    ("node", "Node 20 LTS via the base image's package manager."),
    ("python", "CPython 3 with pip and venv."),
    ("go", "Go 1.27."),
    ("none", "Just the base image -- nothing extra installed."),
];

const TOOLCHAIN_FIELD: Field = Field {
    name: "Language toolchains",
    description: "Pick zero or more language toolchains to install in the image. \
                  The Dockerfile template adds the corresponding install steps; \
                  you can edit the file afterwards.",
    options: TOOLCHAINS,
    doc_link: "doc/usage/image.md",
};

const MCPS: &[(&str, &str)] = &[
    (McpServer::Fs.as_str(), McpServer::Fs.description()),
    (McpServer::Git.as_str(), McpServer::Git.description()),
];

const MCP_FIELD: Field = Field {
    name: "MCP servers",
    description: "Pick zero or more MCP servers to install in the image. The \
                  Dockerfile installs each server's package and the matching \
                  [images.<name>.mcp] entry is appended to config.toml.",
    options: MCPS,
    doc_link: "doc/concepts/mcp-servers.md",
};

/// Slice of every `Field` declared in this module, for `prompt_doc_sync.rs`.
#[cfg_attr(not(feature = "internal-test-api"), allow(dead_code))]
pub const DOC_SYNC_FIELDS: &[&Field] = &[&NAME_FIELD, &BASE_FIELD, &TOOLCHAIN_FIELD, &MCP_FIELD];

/// Index into `McpServer::ALL` of the default selection (the `fs`
/// filesystem server). A `const` rather than a runtime `position()` lookup
/// since `ALL`'s order is itself a deliberate stable contract.
const DEFAULT_MCP_INDEX: usize = 0;
const _: () = assert!(matches!(McpServer::ALL[DEFAULT_MCP_INDEX], McpServer::Fs));

// ---- helpers --------------------------------------------------------------

/// Asks for the image-config name, suggesting [`default_image_name`], until
/// the answer is one [`check_name`] accepts, so a typo costs a retype rather
/// than the run. The bootstrap in `init::repo` asks through here too, since
/// it writes the answer as `default-image`.
pub(crate) async fn ask_name(prompt: &mut impl PromptSource, repo_root: &Path) -> Result<String> {
    let default = default_image_name(repo_root);
    loop {
        let name = prompt.ask_string(&NAME_FIELD, &default).await?;
        match check_name(&name) {
            Ok(()) => return Ok(name),
            Err(e) => eprintln!("[outrig] {e}"),
        }
    }
}

/// `<repo-folder-kebab>-standard`, so the image (and `default-image`)
/// carries the repo's identity by default. Falls back to plain
/// `"standard"` when that isn't a name the image can take: the path has
/// no usable last component, or the folder's name holds a letter outside
/// ASCII, as `café` does. The prompt would refuse such a default, and
/// Enter would never get past it.
fn default_image_name(repo_root: &Path) -> String {
    repo_root
        .file_name()
        .and_then(|s| s.to_str())
        .map(|s| format!("{}-standard", s.to_kebab_case()))
        .filter(|name| check_build_image_name(name).is_ok())
        .unwrap_or_else(|| "standard".to_string())
}

/// Refuses a name the block `image add` writes can't be keyed by, with the
/// error a load of the config would give, so the two explain a name alike.
/// A name that passes is also one component of the path it names under
/// `.agents/outrig/images/`.
fn check_name(name: &str) -> std::result::Result<(), ConfigValidationError> {
    check_build_image_name(name).map_err(|_| ConfigValidationError::BuildImageNameInvalid {
        image: name.to_string(),
    })
}

fn display_rel<'a>(path: &'a Path, root: &Path) -> std::path::Display<'a> {
    path.strip_prefix(root).unwrap_or(path).display()
}

fn load_doc(cfg_path: &Path) -> Result<DocumentMut> {
    let text = match std::fs::read_to_string(cfg_path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e).path_ctx("read", cfg_path).map_err(Into::into),
    };
    text.parse::<DocumentMut>().map_err(|e| {
        OutrigError::Configuration(format!("parsing {}: {e}", cfg_path.display())).into()
    })
}

/// The `images` table both the duplicate check and [`insert_image_block`]
/// work on, created when absent. `[images.<name>]` can't nest inside an
/// inline table, so an inline `images = { ... }` becomes a standard
/// `[images]` table holding the same entries, with the comments above and
/// after its line moved to the header. Anything else `images` could be is
/// refused.
fn images_table<'d>(doc: &'d mut DocumentMut, cfg_path: &Path) -> Result<&'d mut Table> {
    if let Some((mut key, item)) = doc.get_key_value_mut("images")
        && let Item::Value(Value::InlineTable(slot)) = item
    {
        let inline = std::mem::take(slot);
        // Left on the key, `images = `'s decor would render inside the
        // brackets, as `[images ]`.
        let line = std::mem::take(key.leaf_decor_mut());
        let mut decor = Decor::default();
        // Nothing above the line parses as an explicit empty prefix, which
        // would take away the blank line a header gets by default.
        if let Some(above) = line.prefix().filter(|p| p.as_str() != Some("")) {
            decor.set_prefix(above.clone());
        }
        if let Some(after) = inline.decor().suffix() {
            decor.set_suffix(after.clone());
        }
        let mut table = inline.into_table();
        *table.decor_mut() = decor;
        *item = Item::Table(table);
    }
    match doc.entry("images").or_insert_with(|| {
        let mut t = Table::new();
        t.set_implicit(true);
        Item::Table(t)
    }) {
        Item::Table(images) => Ok(images),
        other => Err(OutrigError::Configuration(format!(
            "`images` in {} must be a table of image-configs; got {}",
            cfg_path.display(),
            other.type_name()
        ))
        .into()),
    }
}

fn insert_image_block(images: &mut Table, name: &str, mcps: &[McpServer]) {
    let rel = image_dir_rel(name);
    let dockerfile = rel.join("Dockerfile").to_string_lossy().into_owned();
    let context = rel.to_string_lossy().into_owned();

    let mut entry = Table::new();
    entry.insert("dockerfile", Item::Value(Value::from(dockerfile)));
    entry.insert("context", Item::Value(Value::from(context)));

    let mut mcp = Table::new();
    for server in mcps {
        mcp.insert(server.as_str(), mcp_value(*server));
    }
    entry.insert("mcp", Item::Table(mcp));

    images.insert(name, Item::Table(entry));
}

fn mcp_value(server: McpServer) -> Item {
    let mut cmd = Array::new();
    for arg in server.command_args() {
        cmd.push(*arg);
    }
    let mut full = InlineTable::new();
    full.insert("command", Value::Array(cmd));
    Item::Value(Value::InlineTable(full))
}

#[cfg(test)]
mod tests {
    use outrig::config::Config;

    use super::*;

    /// What `run_with` appends for `coding` with the default `fs` server.
    const CODING: &str = "\n[images.coding]\n\
        dockerfile = \".agents/outrig/images/coding/Dockerfile\"\n\
        context = \".agents/outrig/images/coding\"\n\
        \n\
        [images.coding.mcp]\n\
        fs = { command = [\"mcp-server-filesystem\", \"/workspace\"] }\n";

    /// #186's config with its inline `images` rewritten, `coding` aside.
    const REWRITTEN: &str = "default-image = \"base\"\n\
        \n\
        [images]\n\
        base = { image-name = \"debian\" }\n";

    fn add_coding(seed: &str) -> String {
        let mut doc = seed.parse::<DocumentMut>().expect("seed parses");
        let images = images_table(&mut doc, Path::new("config.toml")).expect("images is a table");
        insert_image_block(images, "coding", &[McpServer::Fs]);
        doc.to_string()
    }

    /// #186's seed: `images` becomes a standard table after the top-level
    /// keys, set off by a blank line like any header, its entry unchanged.
    /// The rewrite keeps entries, not the layout between them, so a TOML 1.1
    /// multi-line inline table loses the comments between its entries.
    #[test]
    fn an_inline_images_becomes_a_standard_table() {
        for seed in [
            "default-image = \"base\"\n\
             images = { base = { image-name = \"debian\" } }\n",
            "default-image = \"base\"\n\
             images = {\n  \
               # The stock image.\n  \
               base = { image-name = \"debian\" },\n\
             }\n",
        ] {
            assert_eq!(add_coding(seed), format!("{REWRITTEN}{CODING}"), "{seed:?}");
        }
    }

    #[test]
    fn comments_on_an_inline_images_line_move_to_its_header() {
        let seed = "default-image = \"base\"\n\
                    \n\
                    # The stock image.\n\
                    images = { base = { image-name = \"debian\" } } # keep\n";
        let want = "default-image = \"base\"\n\
                    \n\
                    # The stock image.\n\
                    [images] # keep\n\
                    base = { image-name = \"debian\" }\n";
        assert_eq!(add_coding(seed), format!("{want}{CODING}"));
    }

    /// A header can't come before a top-level key, so the table lands after
    /// the last of them -- and still ahead of the tables that followed it.
    #[test]
    fn an_inline_images_moves_after_the_top_level_keys() {
        let seed = "images = { base = { image-name = \"debian\" } }\n\
                    default-image = \"base\"\n\
                    \n\
                    [agents.coder]\n\
                    preamble = \"hi\"\n";
        let tail = "\n[agents.coder]\npreamble = \"hi\"\n";
        assert_eq!(add_coding(seed), format!("{REWRITTEN}{CODING}{tail}"));
    }

    /// Only an inline `images` is rewritten; every other spelling gains the
    /// entry and is otherwise left exactly as written.
    #[test]
    fn a_table_images_is_left_as_written() {
        for seed in [
            "[images.base] # keep\nimage-name = \"debian\"\n",
            "[images]\nbase = { image-name = \"debian\" }\n",
            "images.base.image-name = \"debian\"\n",
        ] {
            assert_eq!(add_coding(seed), format!("{seed}{CODING}"), "{seed:?}");
        }
    }

    /// However `images` is spelled, the loader reads back the entries the
    /// seed had, unchanged, with `coding` beside them.
    #[test]
    fn every_spelling_of_images_loads_with_the_new_entry() {
        for seed in [
            "[images.base]\nimage-name = \"debian\"\n",
            "[images]\nbase = { image-name = \"debian\" }\n",
            "images.base.image-name = \"debian\"\n",
            "images = { base = { image-name = \"debian\" } }\n",
            "images = {\n  # The stock image.\n  base = { image-name = \"debian\" },\n}\n",
            "images = { base.image-name = \"debian\" }\n",
            "images = {}\n",
        ] {
            let before = Config::load_from_str(seed).expect("the seed loads");
            let text = add_coding(seed);
            let mut after =
                Config::load_from_str(&text).unwrap_or_else(|e| panic!("{seed:?}: {e}\n{text}"));
            after
                .validate(None)
                .unwrap_or_else(|e| panic!("{seed:?}: {e}\n{text}"));
            let coding = after.images.remove("coding").expect("coding is added");
            assert!(coding.mcp.contains_key("fs"), "{seed:?}:\n{text}");
            assert_eq!(after.images, before.images, "{seed:?}:\n{text}");
        }
    }

    /// A folder whose name can't make one falls back to `standard`, or the
    /// prompt would refuse its own default and Enter would never get past it.
    #[test]
    fn the_default_image_name_is_one_the_prompt_accepts() {
        for (folder, want) in [
            ("/src/My Project", "my-project-standard"),
            ("/src/hello_outrig", "hello-outrig-standard"),
            ("/src/café", "standard"),
            ("/src/___", "standard"),
            ("/", "standard"),
        ] {
            assert_eq!(default_image_name(Path::new(folder)), want, "{folder:?}");
        }
    }
}
