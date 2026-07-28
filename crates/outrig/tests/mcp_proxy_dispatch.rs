//! Unit-style integration tests for `outrig::mcp_proxy::ProxyServer` driven
//! against an in-process [`BackingClient`] fake. Exercises the namespace +
//! dispatch contract without spinning up real MCP children.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use outrig::error::OutrigError;
use outrig::mcp_proxy::{BackingClient, ProxyServer};
use outrig::{McpTool, McpToolResult};
use rmcp::model::{CallToolRequestParams, ContentBlock};
use serde_json::{Value, json};

/// Per-tool canned response. `Ok` becomes a successful `CallToolResult`;
/// `Err` becomes the "backing client failed" path that surfaces as
/// `CallToolResult { is_error: Some(true), ... }` carrying the error text.
type CallResponse = Result<McpToolResult, OutrigError>;

#[derive(Default)]
struct FakeClient {
    name: String,
    tools: Vec<McpTool>,
    /// Recorded `(tool_name, args)` for every `call_tool` invocation.
    received: Mutex<Vec<(String, Value)>>,
    /// Canned response per backend tool name.
    responses: HashMap<String, CallResponse>,
}

impl FakeClient {
    fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            ..Default::default()
        }
    }

    fn with_tool(mut self, tool_name: &str) -> Self {
        let mut tool = McpTool::new(tool_name, json!({"type": "object"}));
        tool.description = Some(format!("desc for {tool_name}"));
        self.tools.push(tool);
        self
    }

    fn respond_ok(mut self, tool_name: &str, body: &str, is_error: bool) -> Self {
        self.responses.insert(
            tool_name.to_string(),
            Ok(if is_error {
                McpToolResult::error(body)
            } else {
                McpToolResult::ok(body)
            }),
        );
        self
    }

    fn respond_err(mut self, tool_name: &str, msg: &str) -> Self {
        self.responses.insert(
            tool_name.to_string(),
            Err(OutrigError::Configuration(msg.to_string())),
        );
        self
    }
}

impl BackingClient for FakeClient {
    fn name(&self) -> &str {
        &self.name
    }

    async fn list_tools(&self) -> outrig::error::Result<Vec<McpTool>> {
        Ok(self.tools.clone())
    }

    async fn call_tool(&self, name: &str, args: Value) -> outrig::error::Result<McpToolResult> {
        self.received.lock().unwrap().push((name.to_string(), args));
        match self.responses.get(name) {
            Some(Ok(r)) => Ok(r.clone()),
            Some(Err(e)) => Err(OutrigError::Configuration(e.to_string())),
            None => Err(OutrigError::Configuration(format!(
                "FakeClient({}) has no response for {name:?}",
                self.name
            ))),
        }
    }
}

fn text_body(result: &rmcp::model::CallToolResult) -> String {
    let mut out = String::new();
    for content in &result.content {
        if let ContentBlock::Text(t) = content {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(&t.text);
        }
    }
    out
}

fn call(name: &str, args: Value) -> CallToolRequestParams {
    let arguments = match args {
        Value::Object(m) => Some(m),
        Value::Null => None,
        other => panic!("test args must be object or null, got {other:?}"),
    };
    let mut request = CallToolRequestParams::new(name.to_string());
    if let Some(arguments) = arguments {
        request = request.with_arguments(arguments);
    }
    request
}

#[tokio::test]
async fn list_tools_unions_namespaces_and_preserves_order() {
    let fs = Arc::new(
        FakeClient::new("fs")
            .with_tool("read_file")
            .with_tool("write_file"),
    );
    let git = Arc::new(FakeClient::new("git").with_tool("commit"));

    let proxy = ProxyServer::build(vec![fs, git]).await.unwrap();

    let listing = proxy.list_tools_inner();
    let names: Vec<&str> = listing.tools.iter().map(|t| t.name.as_ref()).collect();

    // Order: clients in input order, tools in each client's `list_tools` order.
    assert_eq!(
        names,
        vec!["fs__read_file", "fs__write_file", "git__commit"]
    );

    // Description and schema pass through unchanged.
    let read_file = &listing.tools[0];
    assert_eq!(read_file.description.as_deref(), Some("desc for read_file"));
    assert_eq!(
        Value::Object(read_file.input_schema.as_ref().clone()),
        json!({"type": "object"})
    );
}

#[tokio::test]
async fn call_tool_routes_to_correct_backend() {
    let fs = Arc::new(FakeClient::new("fs").with_tool("read_file").respond_ok(
        "read_file",
        "fs payload",
        false,
    ));
    let git = Arc::new(FakeClient::new("git").with_tool("commit").respond_ok(
        "commit",
        "git payload",
        false,
    ));
    let fs_for_assert = fs.clone();
    let git_for_assert = git.clone();

    let proxy = ProxyServer::build(vec![fs, git]).await.unwrap();

    let result = proxy
        .dispatch_call(call("git__commit", json!({"msg": "hi"})))
        .await;
    assert_eq!(result.is_error, Some(false));
    assert_eq!(text_body(&result), "git payload");

    // The backend tool name (not the public one) is what hits the client,
    // and the args object passes through untouched.
    let git_calls = git_for_assert.received.lock().unwrap();
    assert_eq!(git_calls.len(), 1);
    assert_eq!(git_calls[0].0, "commit");
    assert_eq!(git_calls[0].1, json!({"msg": "hi"}));

    // `fs` was not invoked.
    assert!(fs_for_assert.received.lock().unwrap().is_empty());
}

#[tokio::test]
async fn null_arguments_become_value_null() {
    let fs = Arc::new(
        FakeClient::new("fs")
            .with_tool("ping")
            .respond_ok("ping", "pong", false),
    );
    let fs_for_assert = fs.clone();

    let proxy = ProxyServer::build(vec![fs]).await.unwrap();

    // `arguments: None` on the request becomes `Value::Null` on the wire.
    let req = CallToolRequestParams::new("fs__ping".to_string());
    let result = proxy.dispatch_call(req).await;
    assert_eq!(result.is_error, Some(false));

    let calls = fs_for_assert.received.lock().unwrap();
    assert_eq!(calls[0].1, Value::Null);
}

#[tokio::test]
async fn backend_error_surfaces_as_call_tool_result_is_error() {
    let fs = Arc::new(
        FakeClient::new("fs")
            .with_tool("read_file")
            .respond_err("read_file", "permission denied"),
    );

    let proxy = ProxyServer::build(vec![fs]).await.unwrap();

    let result = proxy.dispatch_call(call("fs__read_file", json!({}))).await;
    assert_eq!(result.is_error, Some(true));
    let body = text_body(&result);
    // Body identifies the backing server by name and carries the upstream
    // error verbatim. The prefix lets an MCP client distinguish a proxy
    // failure from a backend's own `is_error=true` response.
    assert!(
        body.contains("outrig: backing server `fs`"),
        "body should identify the backing server, was {body:?}"
    );
    assert!(
        body.contains("permission denied"),
        "body should carry the backend error, was {body:?}"
    );
}

#[tokio::test]
async fn backend_semantic_error_propagates_unchanged() {
    // Backing client returns Ok(...) but with `is_error: true` -- this is the
    // "tool ran but reported failure" path (e.g. `read_file` on a missing
    // path). The flag and body must reach the caller verbatim.
    let fs = Arc::new(FakeClient::new("fs").with_tool("read_file").respond_ok(
        "read_file",
        "ENOENT: missing",
        true,
    ));

    let proxy = ProxyServer::build(vec![fs]).await.unwrap();

    let result = proxy.dispatch_call(call("fs__read_file", json!({}))).await;
    assert_eq!(result.is_error, Some(true));
    assert_eq!(text_body(&result), "ENOENT: missing");
}

#[tokio::test]
async fn unknown_tool_returns_is_error_not_protocol_error() {
    let fs = Arc::new(FakeClient::new("fs").with_tool("read_file"));
    let proxy = ProxyServer::build(vec![fs]).await.unwrap();

    let result = proxy.dispatch_call(call("fs__nope", json!({}))).await;
    assert_eq!(result.is_error, Some(true));
    assert!(text_body(&result).contains("unknown tool"));
}

fn build_err(result: Result<ProxyServer<Arc<FakeClient>>, OutrigError>) -> OutrigError {
    match result {
        Ok(_) => panic!("expected ProxyServer::build to fail"),
        Err(e) => e,
    }
}

#[tokio::test]
async fn duplicate_client_name_is_rejected() {
    let a = Arc::new(FakeClient::new("fs").with_tool("read_file"));
    let b = Arc::new(FakeClient::new("fs").with_tool("write_file"));

    let err = build_err(ProxyServer::build(vec![a, b]).await);
    let msg = err.to_string();
    assert!(
        msg.contains("duplicate backing-client name") && msg.contains("\"fs\""),
        "error was {msg:?}"
    );
}

#[tokio::test]
async fn public_name_collision_is_rejected() {
    // Two distinct (server, tool) pairs that sanitize to the same public
    // name. `sanitize_tool_name` replaces non-charset chars with `_`, so
    // ("fs/", "bar") and ("fs", "_bar") both collapse to `fs___bar`.
    let a = Arc::new(FakeClient::new("fs/").with_tool("bar"));
    let b = Arc::new(FakeClient::new("fs").with_tool("_bar"));

    let err = build_err(ProxyServer::build(vec![a, b]).await);
    let msg = err.to_string();
    assert!(
        msg.contains("public name") && msg.contains("fs___bar"),
        "error was {msg:?}"
    );
}

#[tokio::test]
async fn iter_public_names_matches_list_tools() {
    let fs = Arc::new(
        FakeClient::new("fs")
            .with_tool("read_file")
            .with_tool("write_file"),
    );
    let proxy = ProxyServer::build(vec![fs]).await.unwrap();

    let from_iter: Vec<&str> = proxy.iter_public_names().collect();
    let from_list: Vec<String> = proxy
        .list_tools_inner()
        .tools
        .into_iter()
        .map(|t| t.name.into_owned())
        .collect();

    assert_eq!(from_iter, vec!["fs__read_file", "fs__write_file"]);
    assert_eq!(from_iter, from_list);
}

#[tokio::test]
async fn per_server_counts_preserves_registration_order() {
    let fs = Arc::new(
        FakeClient::new("fs")
            .with_tool("read_file")
            .with_tool("write_file"),
    );
    let git = Arc::new(FakeClient::new("git").with_tool("commit"));
    let empty = Arc::new(FakeClient::new("empty"));

    let proxy = ProxyServer::build(vec![fs, git, empty]).await.unwrap();

    assert_eq!(
        proxy.per_server_counts(),
        vec![("fs", 2), ("git", 1), ("empty", 0)]
    );
}
