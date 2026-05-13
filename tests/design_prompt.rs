//! Integration coverage for `outrig design prompt`.

use std::process::{Command, Output};

use serde_json::Value as JsonValue;
use toml::Value as TomlValue;

fn run(args: &[&str]) -> Output {
    let cwd = tempfile::tempdir().expect("tempdir");
    Command::new(env!("CARGO_BIN_EXE_outrig"))
        .args(args)
        .current_dir(cwd.path())
        .output()
        .unwrap_or_else(|err| panic!("spawn outrig {args:?}: {err}"))
}

fn successful_stdout(args: &[&str]) -> String {
    let output = run(args);
    assert!(
        output.status.success(),
        "outrig {args:?} exited {:?}; stderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stderr),
    );
    String::from_utf8(output.stdout).expect("stdout utf-8")
}

#[test]
fn prompt_prints_self_contained_bundle() {
    let stdout = successful_stdout(&["design", "prompt"]);
    assert!(!stdout.trim().is_empty(), "prompt should not be empty");
    assert!(
        stdout.contains(env!("CARGO_PKG_VERSION")),
        "prompt should include binary version:\n{stdout}",
    );
    for marker in [
        "# Containers",
        "# Config Reference",
        "# AI-assisted design",
        "### Worked example: Rust container",
        "### Worked example: Node container",
        "### Worked example: Multi-MCP container",
    ] {
        assert!(stdout.contains(marker), "prompt lacked {marker:?}");
    }
}

#[test]
fn claude_code_snippet_is_shell_command() {
    let stdout = successful_stdout(&["design", "prompt", "--print-mcp-config", "claude-code"]);
    assert_eq!(stdout, "claude mcp add outrig-self -- outrig mcp self\n");
}

#[test]
fn claude_desktop_snippet_is_json_mcp_servers_block() {
    let stdout = successful_stdout(&["design", "prompt", "--print-mcp-config", "claude-desktop"]);
    let value: JsonValue = serde_json::from_str(&stdout).expect("valid JSON");
    let server = &value["mcpServers"]["outrig-self"];
    assert_eq!(server["command"].as_str(), Some("outrig"));
    assert_eq!(server["args"], serde_json::json!(["mcp", "self"]));
}

#[test]
fn codex_snippet_is_toml_mcp_servers_block() {
    let stdout = successful_stdout(&["design", "prompt", "--print-mcp-config", "codex"]);
    let value: TomlValue = toml::from_str(&stdout).expect("valid TOML");
    let server = value
        .get("mcp_servers")
        .and_then(|servers| servers.get("outrig-self"))
        .expect("outrig-self server");
    assert_eq!(
        server.get("command").and_then(TomlValue::as_str),
        Some("outrig")
    );
    let args = server
        .get("args")
        .and_then(TomlValue::as_array)
        .expect("args array");
    assert_eq!(
        args.iter()
            .filter_map(TomlValue::as_str)
            .collect::<Vec<_>>(),
        vec!["mcp", "self"],
    );
}

#[test]
fn cursor_snippet_is_json_stdio_mcp_servers_block() {
    let stdout = successful_stdout(&["design", "prompt", "--print-mcp-config", "cursor"]);
    let value: JsonValue = serde_json::from_str(&stdout).expect("valid JSON");
    let server = &value["mcpServers"]["outrig-self"];
    assert_eq!(server["type"].as_str(), Some("stdio"));
    assert_eq!(server["command"].as_str(), Some("outrig"));
    assert_eq!(server["args"], serde_json::json!(["mcp", "self"]));
}

#[test]
fn bogus_tool_fails_with_valid_tool_names() {
    let output = run(&["design", "prompt", "--print-mcp-config", "bogus"]);
    assert!(
        !output.status.success(),
        "bogus tool should fail; stdout:\n{}",
        String::from_utf8_lossy(&output.stdout),
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    for expected in ["claude-code", "claude-desktop", "codex", "cursor"] {
        assert!(
            stderr.contains(expected),
            "stderr should list {expected:?}; stderr:\n{stderr}",
        );
    }
}
