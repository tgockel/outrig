//! Integration smoke for `outrig mcp self`.
//!
//! The self-description server is host-only: it does not require podman,
//! a repo config, or the e2e feature. The test drives it as an MCP client
//! over stdio and exercises every advertised tool.

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use outrig::mcp_proxy::SUPPORTED_PROTOCOL_VERSIONS;
use rmcp::model::{CallToolRequestParams, ContentBlock};
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
            "validate_image_toml",
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
        assert!(schema["paths"].get("image_config").is_none());
        assert_eq!(schema["image_labels"]["mcp"], "org.outrig.mcp");
        assert_eq!(schema["image_labels"]["schema"], "org.outrig.schema");
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

        let image_toml: Value = call_json(
            &service,
            "validate_image_toml",
            serde_json::json!({
                "toml": r#"
[image]
ref = "rust-dev"

[mcp]
fs = ["mcp-server-filesystem", "/workspace"]
"#
            }),
        )
        .await;
        assert_eq!(
            image_toml["valid"], true,
            "expected valid image.toml: {image_toml}"
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
        .filter_map(|content| match content {
            ContentBlock::Text(t) => Some(t.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    serde_json::from_str(&body)
        .unwrap_or_else(|err| panic!("tools/call {name} returned invalid JSON {body:?}: {err}"))
}

/// `mcp self` starts no container and creates no session, so it refuses
/// each `McpArgs` option rather than ignore it. Every one has a row here.
#[test]
fn mcp_self_refuses_the_session_options() {
    let cwd = tempfile::tempdir().expect("tempdir");
    let bin = env!("CARGO_BIN_EXE_outrig");
    for (cmdline, flag) in [
        ("mcp self --image x", "--image"),
        ("mcp self --session-dir /tmp", "--session-dir"),
        ("mcp self --attach foo", "--attach"),
        // `--listen` is not global, so it is only accepted ahead of `self`.
        ("mcp --listen 127.0.0.1:7331 self", "--listen"),
        ("mcp self --env K=V", "--env"),
        ("mcp self --env not-a-valid-env", "--env"),
        // A global option parses ahead of `self` as well as after it.
        ("mcp --env K=V self", "--env"),
        ("mcp self --network audit", "--network"),
        ("mcp self --volume /tmp:/tmp", "--volume"),
    ] {
        let output = std::process::Command::new(bin)
            .args(cmdline.split_whitespace())
            .current_dir(cwd.path())
            .stdin(Stdio::null())
            .output()
            .expect("run outrig");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(
            output.status.code(),
            Some(1),
            "`outrig {cmdline}` should be refused; stderr was: {stderr}",
        );
        assert!(
            stderr.contains(&format!("remove {flag}")),
            "`outrig {cmdline}` should name {flag}; stderr was: {stderr}",
        );
    }
}

/// Regression for the `tools/list` rejection reported against protocol revision
/// `2026-07-28`, which made the SEP-2549 `ttlMs` / `cacheScope` fields mandatory
/// on list results. The server negotiated that revision but omitted both, so a
/// conforming client rejected the response and loaded no tools at all.
///
/// `2026-07-28` has no `initialize` handshake: a client speaking it names the
/// revision in every request's `_meta`, and since rmcp 3.2 that is the only way
/// the revision is served. An `initialize` asking for it is answered in the
/// server's default instead, so the fields are checked on that path as well.
///
/// Deliberately raw line-delimited JSON-RPC rather than rmcp's own client: both
/// fields deserialize into `Option`, so a typed client happily accepts their
/// absence and would not have caught the regression. Only the wire bytes will.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tools_list_carries_cache_metadata_on_the_2026_07_28_revision() {
    init_tracing();

    for (client, served) in [
        (Revision::PerRequest("2026-07-28"), "2026-07-28"),
        (Revision::Initialize("2026-07-28"), "2025-11-25"),
        // A revision OutRig has not audited must never be echoed back. rmcp
        // answers an unsupported request with the server's own default rather
        // than the highest version it supports, so this lands on 2025-11-25 --
        // the point is only that it is a version from the pinned list.
        (Revision::Initialize("2027-01-01"), "2025-11-25"),
    ] {
        let responses = raw_list_responses(client).await;

        if let Revision::Initialize(requested) = client {
            assert_eq!(
                responses["initialize"]["result"]["protocolVersion"], served,
                "requesting {requested} should negotiate {served}: {responses}",
            );
        }

        let listing = &responses["tools/list"]["result"];
        // `resultType` (SEP-2322) is serialized from 2026-07-28 on and stripped for
        // older peers, so it is the wire's own record of which revision answered.
        let result_type = if served < "2026-07-28" {
            Value::Null
        } else {
            Value::from("complete")
        };
        assert_eq!(
            listing["resultType"], result_type,
            "{client:?} should be answered in {served}: {listing}",
        );
        assert!(
            listing["ttlMs"].is_number(),
            "tools/list must carry a numeric ttlMs on {served}: {listing}",
        );
        assert!(
            matches!(listing["cacheScope"].as_str(), Some("public" | "private")),
            "tools/list must carry a public/private cacheScope on {served}: {listing}",
        );
        assert_eq!(
            listing["cacheScope"], "public",
            "the self server's tool set is compiled in, so it is publicly cacheable",
        );
        assert_eq!(
            listing["ttlMs"], 300_000,
            "tools/list TTL should match the server's declared freshness window",
        );
        assert!(
            !listing["tools"].as_array().expect("tools array").is_empty(),
            "tools/list should not be empty: {listing}",
        );

        // The capability set is tools-only, so the other list methods must say
        // method-not-found rather than answer. rmcp's default handler bodies
        // return an empty success carrying `resultType` but neither cache field
        // -- the same malformed shape, on a surface this server does not have.
        for method in LIST_METHODS.into_iter().filter(|&m| m != "tools/list") {
            let error = &responses[method]["error"];
            assert_eq!(
                error["code"], -32601,
                "{method} should be method-not-found on {served}: {error}",
            );
        }
    }
}

/// A revision named per request is held to the pinned list too. Where the
/// handshake falls back to the server's default, a request whose own `_meta`
/// names an unsupported revision is refused, and the error lists the revisions
/// the server does serve.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_per_request_revision_outside_the_pinned_list_is_refused() {
    init_tracing();

    let responses = raw_list_responses(Revision::PerRequest("2027-01-01")).await;
    let supported =
        serde_json::to_value(SUPPORTED_PROTOCOL_VERSIONS).expect("serialize pinned revisions");
    for method in LIST_METHODS {
        let error = &responses[method]["error"];
        assert_eq!(
            error["code"], -32022,
            "{method} should refuse an unsupported protocol version: {error}",
        );
        assert_eq!(
            error["data"]["requested"], "2027-01-01",
            "{method}'s refusal should name the requested revision: {error}",
        );
        assert_eq!(
            error["data"]["supported"], supported,
            "{method}'s refusal should list exactly the pinned revisions: {error}",
        );
    }
}

/// The list methods [`raw_list_responses`] sends, in order.
const LIST_METHODS: [&str; 4] = [
    "tools/list",
    "resources/list",
    "resources/templates/list",
    "prompts/list",
];

/// How a hand-written client names the protocol revision it speaks.
#[derive(Clone, Copy, Debug)]
enum Revision {
    /// Asked for once, in an `initialize` handshake.
    Initialize(&'static str),
    /// Named in every request's `_meta`, with no handshake.
    PerRequest(&'static str),
}

/// Drive `outrig mcp self` over stdio with hand-written JSON-RPC: the `initialize`
/// handshake first if `client` negotiates one, then each of [`LIST_METHODS`].
/// Returns every raw response message, keyed by the method it answers.
async fn raw_list_responses(client: Revision) -> Value {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let cwd = tempfile::tempdir().expect("tempdir");
    let bin = env!("CARGO_BIN_EXE_outrig");
    let mut child = Command::new(bin)
        .args(["mcp", "self"])
        .current_dir(cwd.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn outrig mcp self");

    let mut stdin = child.stdin.take().expect("stdin piped");
    let mut stdout = BufReader::new(child.stdout.take().expect("stdout piped")).lines();

    let client_info = serde_json::json!({"name": "outrig-test", "version": "1.0.0"});
    let mut requests = Vec::new();
    let mut pending = HashMap::new();
    let params = match client {
        Revision::Initialize(revision) => {
            requests.push(serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": revision,
                    "capabilities": {},
                    "clientInfo": client_info,
                },
            }));
            requests.push(serde_json::json!({
                "jsonrpc": "2.0",
                "method": "notifications/initialized",
            }));
            pending.insert(1, "initialize");
            serde_json::json!({})
        }
        // With no handshake, each request carries what `initialize` would have.
        Revision::PerRequest(revision) => serde_json::json!({
            "_meta": {
                "io.modelcontextprotocol/protocolVersion": revision,
                "io.modelcontextprotocol/clientCapabilities": {},
                "io.modelcontextprotocol/clientInfo": client_info,
            },
        }),
    };
    for (id, method) in (2..).zip(LIST_METHODS) {
        requests.push(serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }));
        pending.insert(id, method);
    }

    let work = async {
        for request in &requests {
            stdin
                .write_all(format!("{request}\n").as_bytes())
                .await
                .expect("write request");
            stdin.flush().await.expect("flush request");
        }

        // Responses carry the request id; notifications carry none. Collect one
        // reply per request sent and ignore anything else on the stream.
        let mut responses = serde_json::Map::new();
        while !pending.is_empty() {
            let line = stdout
                .next_line()
                .await
                .expect("read stdout")
                .expect("server closed stdout before answering");
            let Ok(message) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            let id = message.get("id").and_then(Value::as_u64);
            if let Some(method) = id.and_then(|id| pending.remove(&id)) {
                responses.insert(method.to_string(), message);
            }
        }
        Value::Object(responses)
    };

    let result = timeout(TEST_TIMEOUT, work)
        .await
        .unwrap_or_else(|_| panic!("raw list requests did not finish within {TEST_TIMEOUT:?}"));
    let _ = child.kill().await;
    result
}
