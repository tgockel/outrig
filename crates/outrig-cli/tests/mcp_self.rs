//! Integration smoke for `outrig mcp self`.
//!
//! The self-description server is host-only: it does not require podman,
//! a repo config, or the e2e feature. The test drives it as an MCP client
//! over stdio and exercises every advertised tool.

use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rmcp::model::{CallToolRequestParams, RawContent};
use rmcp::service::serve_client;
use serde::de::DeserializeOwned;
use serde_json::Value;
use tokio::process::Command;
use tokio::time::timeout;

mod common;
use common::{init_tracing, stream_lines};

const TEST_TIMEOUT: Duration = Duration::from_secs(30);

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_self_serves_docs_schema_suggestions_and_validators() {
    init_tracing();

    let cwd = tempfile::tempdir().expect("tempdir");
    let bin = env!("CARGO_BIN_EXE_outrig");
    let mut child = Command::new(bin)
        .args(["mcp", "self"])
        .current_dir(cwd.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn outrig mcp self");

    let child_stdin = child.stdin.take().expect("stdin piped");
    let child_stdout = child.stdout.take().expect("stdout piped");
    let stderr = child.stderr.take().expect("stderr piped");

    let stderr_buf = Arc::new(Mutex::new(String::new()));
    let stderr_task = tokio::spawn(stream_lines(stderr, stderr_buf.clone(), "stderr"));

    let work = async {
        let service = serve_client((), (child_stdout, child_stdin))
            .await
            .expect("serve_client initialize");

        let listing = service
            .list_tools(Default::default())
            .await
            .expect("tools/list");
        let names: Vec<String> = listing
            .tools
            .iter()
            .map(|tool| tool.name.as_ref().to_string())
            .collect();
        for expected in [
            "list_docs",
            "get_doc",
            "get_config_schema",
            "list_base_images",
            "list_mcp_server_suggestions",
            "validate_dockerfile",
            "validate_config",
        ] {
            assert!(
                names.iter().any(|name| name == expected),
                "missing tool {expected:?} in {names:?}",
            );
        }
        for tool in &listing.tools {
            let annotations = tool
                .annotations
                .as_ref()
                .unwrap_or_else(|| panic!("{} should have annotations", tool.name));
            assert_eq!(annotations.read_only_hint, Some(true));
            assert_eq!(annotations.open_world_hint, Some(false));
        }

        let docs: Value = call_json(&service, "list_docs", serde_json::json!({})).await;
        assert!(
            docs["docs"]
                .as_array()
                .expect("docs array")
                .iter()
                .any(|doc| doc["page"] == "concepts/mcp-trust-model"),
            "list_docs should include trust model: {docs}",
        );

        let trust: Value = call_json(
            &service,
            "get_doc",
            serde_json::json!({"page": "concepts/mcp-trust-model"}),
        )
        .await;
        assert!(
            trust["markdown"]
                .as_str()
                .expect("markdown string")
                .contains("# MCP Trust Model"),
            "get_doc returned unexpected content: {trust}",
        );

        let schema: Value = call_json(&service, "get_config_schema", serde_json::json!({})).await;
        assert_eq!(schema["paths"]["repo_config"], ".agents/outrig/config.toml");
        assert!(schema["image_config_schema"].is_object());

        let bases: Value = call_json(&service, "list_base_images", serde_json::json!({})).await;
        assert!(
            bases["note"]
                .as_str()
                .expect("base note")
                .contains("suggestions only"),
            "base image response should carry suggestions-only note: {bases}",
        );
        let suggestions: Value = call_json(
            &service,
            "list_mcp_server_suggestions",
            serde_json::json!({}),
        )
        .await;
        assert!(
            suggestions["note"]
                .as_str()
                .expect("suggestion note")
                .contains("suggestions only"),
            "suggestion response should carry suggestions-only note: {suggestions}",
        );
        assert!(
            suggestions["items"]
                .as_array()
                .expect("suggestion items")
                .iter()
                .any(|item| item["name"] == "shell"
                    && item["guidance"]
                        .as_str()
                        .is_some_and(|guidance| guidance.contains("arbitrary MCP"))),
            "suggestions should include shell guidance: {suggestions}",
        );

        let dockerfile = include_str!("fixtures/self/user.Dockerfile");
        let docker: Value = call_json(
            &service,
            "validate_dockerfile",
            serde_json::json!({ "dockerfile": dockerfile }),
        )
        .await;
        let warnings = docker["warnings"].as_array().expect("warnings array");
        assert!(
            warnings.iter().any(|w| w["code"] == "user_ignored"),
            "expected USER warning: {docker}",
        );

        let config: Value = call_json(
            &service,
            "validate_config",
            serde_json::json!({ "toml": include_str!("fixtures/self/invalid-config.toml") }),
        )
        .await;
        assert_eq!(config["valid"], false);
        assert!(
            config["errors"][0]["message"]
                .as_str()
                .expect("error message")
                .contains("invalid mcp server name"),
            "expected invalid mcp server-name error: {config}",
        );

        let _ = service.cancel().await;
    };

    timeout(TEST_TIMEOUT, work)
        .await
        .unwrap_or_else(|_| panic!("mcp self work did not finish within {TEST_TIMEOUT:?}"));

    let status = timeout(TEST_TIMEOUT, child.wait())
        .await
        .unwrap_or_else(|_| panic!("mcp self process did not exit within {TEST_TIMEOUT:?}"))
        .expect("child.wait");
    let _ = stderr_task.await;
    let stderr_str = stderr_buf.lock().unwrap().clone();

    assert!(
        status.success(),
        "outrig mcp self exited with {status:?}; stderr was: {stderr_str}",
    );
    assert!(
        stderr_str.contains("[outrig] mcp self server ready"),
        "stderr lacked readiness line: {stderr_str}",
    );
}

async fn call_json<T>(
    service: &rmcp::service::RunningService<rmcp::RoleClient, ()>,
    name: &str,
    args: Value,
) -> T
where
    T: DeserializeOwned,
{
    let arguments = Some(args.as_object().expect("object args").clone());
    let mut request = CallToolRequestParams::new(name.to_string());
    if let Some(arguments) = arguments {
        request = request.with_arguments(arguments);
    }
    let result = service
        .call_tool(request)
        .await
        .unwrap_or_else(|err| panic!("tools/call {name}: {err}"));
    assert!(
        result.is_error != Some(true),
        "tools/call {name} returned error: {result:?}",
    );
    let body = result
        .content
        .iter()
        .filter_map(|content| match &content.raw {
            RawContent::Text(t) => Some(t.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    serde_json::from_str(&body)
        .unwrap_or_else(|err| panic!("tools/call {name} returned invalid JSON {body:?}: {err}"))
}
