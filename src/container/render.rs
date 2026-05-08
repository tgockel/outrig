//! Pure Dockerfile assembly for `outrig container add`.
//!
//! `render(base, toolchains, mcps)` stitches a header (per base-family) plus
//! per-(toolchain, family) fragments plus an MCP-server install block plus a
//! universal footer into a single string. No I/O. The fragments are
//! `include_str!`'d from `templates/`; the MCP install lines are inline
//! string literals because they're short and need to be family-aware.

use std::collections::BTreeSet;
use std::fmt::Write as _;

/// Apt vs apk: drives every install line below.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Family {
    Debian,
    Alpine,
}

/// One of the curated base images offered by `container add`. Each carries
/// the family discriminant and the full image string used in `FROM
/// docker.io/library/<image>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BaseImage {
    DebianBookwormSlim,
    Ubuntu24_04,
    AlpineLatest,
    Node20BookwormSlim,
    Python3_12Slim,
}

impl BaseImage {
    pub const ALL: &'static [BaseImage] = &[
        Self::DebianBookwormSlim,
        Self::Ubuntu24_04,
        Self::AlpineLatest,
        Self::Node20BookwormSlim,
        Self::Python3_12Slim,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::DebianBookwormSlim => "debian:bookworm-slim",
            Self::Ubuntu24_04 => "ubuntu:24.04",
            Self::AlpineLatest => "alpine:latest",
            Self::Node20BookwormSlim => "node:20-bookworm-slim",
            Self::Python3_12Slim => "python:3.12-slim",
        }
    }

    pub const fn description(self) -> &'static str {
        match self {
            Self::DebianBookwormSlim => "Debian 12 slim. Apt-based; small but full-featured.",
            Self::Ubuntu24_04 => "Ubuntu 24.04 LTS. Apt-based; superset of Debian.",
            Self::AlpineLatest => "Alpine. Apk + musl; smallest footprint.",
            Self::Node20BookwormSlim => "Debian-slim with Node 20 LTS preinstalled.",
            Self::Python3_12Slim => "Debian-slim with CPython 3.12 + pip preinstalled.",
        }
    }

    pub fn family(self) -> Family {
        match self {
            Self::AlpineLatest => Family::Alpine,
            _ => Family::Debian,
        }
    }

    /// `true` if the base image already ships a working `node + npm`. Used
    /// by the MCP install block to skip a redundant runtime-install step.
    fn has_node(self) -> bool {
        matches!(self, Self::Node20BookwormSlim)
    }

    /// `true` if the base image already ships a working `python3 + pip`.
    fn has_python(self) -> bool {
        matches!(self, Self::Python3_12Slim)
    }
}

/// One of the curated language toolchains offered by the toolchain
/// multi-select. Canonical render order: `Rust, Node, Python, Go`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Toolchain {
    Rust,
    Node,
    Python,
    Go,
    None,
}

impl Toolchain {
    /// Order matters here: it's the canonical (deterministic) render order
    /// and the order shown in the prompt's option list.
    pub const ALL: &'static [Toolchain] =
        &[Self::Rust, Self::Node, Self::Python, Self::Go, Self::None];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Rust => "rust",
            Self::Node => "node",
            Self::Python => "python",
            Self::Go => "go",
            Self::None => "none",
        }
    }
}

/// Curated MCP server package recipes rendered by `container add`.
/// Config can still declare any MCP command, including shell servers; this
/// enum only covers recipes OutRig can install without more user input.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum McpServer {
    Fs,
    Git,
}

impl McpServer {
    /// Canonical render order matches prompt order.
    pub const ALL: &'static [McpServer] = &[Self::Fs, Self::Git];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Fs => "fs",
            Self::Git => "git",
        }
    }

    pub const fn description(self) -> &'static str {
        match self {
            Self::Fs => "Filesystem MCP server (npm @modelcontextprotocol/server-filesystem).",
            Self::Git => "Git MCP server (PyPI mcp-server-git).",
        }
    }

    /// argv of the runtime `command` for this server's
    /// `[containers.<name>.mcp.<server>]` entry. The first element is the
    /// binary name, the rest are arguments.
    pub fn command_args(self) -> &'static [&'static str] {
        match self {
            Self::Fs => &["mcp-server-filesystem", "/workspace"],
            Self::Git => &["mcp-server-git", "--repository", "/workspace"],
        }
    }

    /// `true` if this server is installed via npm (and so needs node + npm
    /// available at build time).
    fn needs_node(self) -> bool {
        matches!(self, Self::Fs)
    }

    /// `true` if this server is installed via pip (and so needs python3 +
    /// pip available at build time).
    fn needs_python(self) -> bool {
        matches!(self, Self::Git)
    }

    /// The bare `<package-manager> install ...` line for this server. The
    /// caller is responsible for ensuring the package manager is available.
    pub(crate) fn install_cmd(self) -> &'static str {
        match self {
            Self::Fs => "npm install -g @modelcontextprotocol/server-filesystem",
            // `--break-system-packages` lets pip write into the system
            // site-packages on Debian/Python 3.11+; cheaper than a venv for
            // a one-binary install. Alpine's `py3-pip` accepts it too.
            Self::Git => "pip install --break-system-packages mcp-server-git",
        }
    }
}

const HEADER_DEBIAN: &str = include_str!("templates/header.debian.dockerfile");
const HEADER_ALPINE: &str = include_str!("templates/header.alpine.dockerfile");

const RUST_DEBIAN: &str = include_str!("templates/rust.debian.dockerfile");
const RUST_ALPINE: &str = include_str!("templates/rust.alpine.dockerfile");
const NODE_DEBIAN: &str = include_str!("templates/node.debian.dockerfile");
const NODE_ALPINE: &str = include_str!("templates/node.alpine.dockerfile");
const PYTHON_DEBIAN: &str = include_str!("templates/python.debian.dockerfile");
const PYTHON_ALPINE: &str = include_str!("templates/python.alpine.dockerfile");
const GO_DEBIAN: &str = include_str!("templates/go.debian.dockerfile");
const GO_ALPINE: &str = include_str!("templates/go.alpine.dockerfile");

const FOOTER: &str = include_str!("templates/footer.dockerfile");

fn header_template(family: Family) -> &'static str {
    match family {
        Family::Debian => HEADER_DEBIAN,
        Family::Alpine => HEADER_ALPINE,
    }
}

fn toolchain_fragment(t: Toolchain, family: Family) -> Option<&'static str> {
    let frag = match (t, family) {
        (Toolchain::Rust, Family::Debian) => RUST_DEBIAN,
        (Toolchain::Rust, Family::Alpine) => RUST_ALPINE,
        (Toolchain::Node, Family::Debian) => NODE_DEBIAN,
        (Toolchain::Node, Family::Alpine) => NODE_ALPINE,
        (Toolchain::Python, Family::Debian) => PYTHON_DEBIAN,
        (Toolchain::Python, Family::Alpine) => PYTHON_ALPINE,
        (Toolchain::Go, Family::Debian) => GO_DEBIAN,
        (Toolchain::Go, Family::Alpine) => GO_ALPINE,
        (Toolchain::None, _) => return None,
    };
    Some(frag)
}

/// Family-aware `apt-get install` / `apk add` for the named system
/// packages. Packages are space-separated.
fn ensure_pkgs(family: Family, pkgs: &str) -> String {
    match family {
        Family::Debian => format!(
            "apt-get update && apt-get install -y --no-install-recommends {pkgs} \
             && rm -rf /var/lib/apt/lists/*"
        ),
        Family::Alpine => format!("apk add --no-cache {pkgs}"),
    }
}

/// Build the `# MCP servers` block: a `RUN` that ensures any required
/// runtimes are present, then runs each server's install command.
fn mcp_block(
    base: BaseImage,
    toolchains: &BTreeSet<Toolchain>,
    mcps: &BTreeSet<McpServer>,
) -> String {
    if mcps.is_empty() {
        return String::new();
    }
    let family = base.family();

    let need_node = mcps.iter().any(|m| m.needs_node())
        && !base.has_node()
        && !toolchains.contains(&Toolchain::Node);
    let need_python = mcps.iter().any(|m| m.needs_python())
        && !base.has_python()
        && !toolchains.contains(&Toolchain::Python);

    let mut lines: Vec<String> = Vec::new();
    if need_node {
        // Bookworm and Alpine both ship a usable `nodejs npm` in the
        // default repos -- older than NodeSource's setup_20.x but enough
        // to run `npm install -g`. Saves the curl-and-pipe-bash dance.
        lines.push(ensure_pkgs(family, "nodejs npm"));
    }
    if need_python {
        let pkgs = match family {
            Family::Debian => "python3 python3-pip",
            Family::Alpine => "python3 py3-pip",
        };
        lines.push(ensure_pkgs(family, pkgs));
    }
    for m in mcps {
        lines.push(m.install_cmd().to_string());
    }

    let mut out = String::from("# MCP servers\nRUN ");
    for (i, line) in lines.iter().enumerate() {
        if i > 0 {
            out.push_str(" \\\n && ");
        }
        out.push_str(line);
    }
    out.push('\n');
    out
}

/// Append `section` to `out`, ensuring it ends in exactly one `\n`. Used
/// to normalize the trailing newline of `include_str!`'d templates that
/// may or may not have one depending on editor settings.
fn push_section(out: &mut String, section: &str) {
    out.push_str(section);
    if !out.ends_with('\n') {
        out.push('\n');
    }
}

/// Stitch the header, toolchain fragments, MCP block, and footer into a
/// final Dockerfile string.
///
/// `toolchains` and `mcps` are deduped + sorted into canonical order so
/// output is byte-stable regardless of multi-select input order.
/// `Toolchain::None` in the toolchain set is treated as "no toolchain
/// section" (skipped silently).
pub fn render(base: BaseImage, toolchains: &[Toolchain], mcps: &[McpServer]) -> String {
    let toolchains: BTreeSet<Toolchain> = toolchains
        .iter()
        .copied()
        .filter(|t| *t != Toolchain::None)
        .collect();
    let mcps: BTreeSet<McpServer> = mcps.iter().copied().collect();

    let family = base.family();
    let mut out = String::new();

    let header = header_template(family).replace("{IMAGE}", base.as_str());
    push_section(&mut out, &header);

    for &t in Toolchain::ALL {
        if t == Toolchain::None || !toolchains.contains(&t) {
            continue;
        }
        let frag = toolchain_fragment(t, family).expect("non-None toolchain has a fragment");
        out.push('\n');
        let _ = writeln!(out, "# {} toolchain", t.as_str());
        push_section(&mut out, frag);
    }

    let block = mcp_block(base, &toolchains, &mcps);
    if !block.is_empty() {
        out.push('\n');
        out.push_str(&block);
    }

    out.push('\n');
    push_section(&mut out, FOOTER);

    out
}
