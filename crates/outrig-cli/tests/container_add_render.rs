//! Pure-function tests for `outrig_cli::container_setup::render::render`. Asserts
//! invariants on the generated Dockerfile across every (base x toolchain x
//! mcp) combination, plus determinism + ordering properties.

use outrig_cli::container_setup::render::{self, BaseImage, McpServer, Toolchain};

fn assert_invariants(out: &str, base: BaseImage, toolchains: &[Toolchain], mcps: &[McpServer]) {
    let label = format!("base={:?} toolchains={toolchains:?} mcps={mcps:?}", base);

    // FROM line.
    let expected_from = format!("FROM docker.io/library/{}", base.as_str());
    assert!(
        out.starts_with(&expected_from),
        "[{label}] expected output to start with `{expected_from}`; got:\n{out}",
    );

    // Footer is exact and final (no `USER` directive between WORKDIR and EOF).
    assert!(
        out.contains("WORKDIR /workspace\n"),
        "[{label}] missing WORKDIR /workspace:\n{out}",
    );
    assert!(
        out.trim_end().ends_with("CMD [\"sleep\", \"infinity\"]"),
        "[{label}] expected output to end with the sleep-infinity CMD:\n{out}",
    );
    assert!(
        out.ends_with('\n'),
        "[{label}] expected trailing newline:\n{out:?}",
    );
    assert!(
        !out.contains("\nUSER "),
        "[{label}] generated Dockerfile must not set a USER:\n{out}",
    );

    // The bootstrap user-creation tools must be available.
    match base {
        BaseImage::AlpineLatest => {
            assert!(
                out.contains("shadow"),
                "[{label}] alpine header must install shadow (for useradd):\n{out}",
            );
        }
        _ => {
            assert!(
                out.contains("passwd"),
                "[{label}] debian-family header must install passwd:\n{out}",
            );
        }
    }

    // No trailing whitespace on any line.
    for (i, line) in out.lines().enumerate() {
        assert!(
            line == line.trim_end(),
            "[{label}] line {i} has trailing whitespace: {line:?}",
        );
    }

    // Every selected toolchain leaves a comment marker.
    for t in toolchains {
        if *t == Toolchain::None {
            continue;
        }
        let needle = format!("# {} toolchain", t.as_str());
        assert!(
            out.contains(&needle),
            "[{label}] missing toolchain marker `{needle}`:\n{out}",
        );
    }

    // Every selected MCP server has its install token present.
    for m in mcps {
        let needle = match m {
            McpServer::Fs => "@modelcontextprotocol/server-filesystem",
            McpServer::Git => "mcp-server-git",
        };
        assert!(
            out.contains(needle),
            "[{label}] missing mcp install token `{needle}`:\n{out}",
        );
    }

    // If any MCP servers were requested, the block has a header comment.
    if !mcps.is_empty() {
        assert!(
            out.contains("# MCP servers"),
            "[{label}] missing # MCP servers header:\n{out}",
        );
    }
}

#[test]
fn matrix_invariants() {
    let toolchain_sets: &[&[Toolchain]] = &[
        &[],
        &[Toolchain::None],
        &[Toolchain::Rust],
        &[Toolchain::Node],
        &[Toolchain::Python],
        &[Toolchain::Go],
        &[
            Toolchain::Rust,
            Toolchain::Node,
            Toolchain::Python,
            Toolchain::Go,
        ],
    ];
    let mcp_sets: &[&[McpServer]] = &[
        &[],
        &[McpServer::Fs],
        &[McpServer::Git],
        &[McpServer::Fs, McpServer::Git],
    ];

    for base in BaseImage::ALL {
        for toolchains in toolchain_sets {
            for mcps in mcp_sets {
                let out = render::render(*base, toolchains, mcps);
                assert_invariants(&out, *base, toolchains, mcps);
            }
        }
    }
}

#[test]
fn render_is_deterministic() {
    let a = render::render(
        BaseImage::DebianBookwormSlim,
        &[Toolchain::Rust, Toolchain::Node],
        &[McpServer::Fs],
    );
    let b = render::render(
        BaseImage::DebianBookwormSlim,
        &[Toolchain::Rust, Toolchain::Node],
        &[McpServer::Fs],
    );
    assert_eq!(a, b);
}

#[test]
fn input_order_does_not_affect_output() {
    let a = render::render(
        BaseImage::DebianBookwormSlim,
        &[Toolchain::Node, Toolchain::Rust],
        &[McpServer::Git, McpServer::Fs],
    );
    let b = render::render(
        BaseImage::DebianBookwormSlim,
        &[Toolchain::Rust, Toolchain::Node],
        &[McpServer::Fs, McpServer::Git],
    );
    assert_eq!(a, b);
}

#[test]
fn none_toolchain_skips_section() {
    let out = render::render(
        BaseImage::DebianBookwormSlim,
        &[Toolchain::None],
        &[McpServer::Fs],
    );
    assert!(
        !out.contains("# none toolchain"),
        "Toolchain::None should not produce a fragment:\n{out}",
    );
    // Sanity: the rest of the structure still renders.
    assert!(out.contains("FROM docker.io/library/debian:bookworm-slim"));
    assert!(out.contains("# MCP servers"));
}

#[test]
fn fs_on_debian_base_installs_node_runtime() {
    // A debian-family base without the `node` toolchain selected. The MCP
    // block must add `nodejs npm` so `npm install -g ...` resolves.
    let out = render::render(BaseImage::DebianBookwormSlim, &[], &[McpServer::Fs]);
    assert!(
        out.contains("nodejs npm"),
        "expected runtime ensure-install for nodejs+npm:\n{out}",
    );
}

#[test]
fn fs_on_node_base_skips_runtime_install() {
    // node:20-bookworm-slim ships node + npm already; the MCP block must
    // not redundantly install them.
    let out = render::render(BaseImage::Node20BookwormSlim, &[], &[McpServer::Fs]);
    assert!(
        !out.contains("apt-get install -y --no-install-recommends nodejs"),
        "should not redundantly install nodejs on node-base:\n{out}",
    );
}

#[test]
fn git_on_python_base_skips_runtime_install() {
    let out = render::render(BaseImage::Python3_12Slim, &[], &[McpServer::Git]);
    assert!(
        !out.contains("python3-pip"),
        "should not redundantly install python3-pip on python-base:\n{out}",
    );
    assert!(out.contains("pip install --break-system-packages mcp-server-git"));
}

#[test]
fn git_on_alpine_base_uses_apk_pip() {
    let out = render::render(BaseImage::AlpineLatest, &[], &[McpServer::Git]);
    assert!(
        out.contains("apk add --no-cache python3 py3-pip"),
        "expected apk-based python install on alpine:\n{out}",
    );
}
