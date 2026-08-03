use std::sync::Arc;

use rmcp::ErrorData as McpError;
use rmcp::ServerHandler;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation,
    JsonObject, ListToolsResult, PaginatedRequestParams, ServerCapabilities, ServerInfo, Tool,
    ToolAnnotations,
};
use rmcp::service::{RequestContext, RoleServer};
use schemars::JsonSchema;
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio_util::sync::CancellationToken;

use crate::error::{OutrigError, Result};
use crate::mcp_self::args::{
    EmptyArgs, GET_CONFIG_SCHEMA, GET_DOC, GetDocArgs, LIST_BASE_IMAGES, LIST_DOCS,
    LIST_MCP_SERVER_SUGGESTIONS, VALIDATE_CONFIG, VALIDATE_DOCKERFILE, VALIDATE_IMAGE_TOML,
    ValidateConfigArgs, ValidateDockerfileArgs,
};
use crate::mcp_self::{docs, schema, suggestions, validate};

#[derive(Debug, Clone, Default)]
pub struct SelfServer;

pub async fn serve_stdio() -> Result<i32> {
    let ct = CancellationToken::new();
    let server = SelfServer;
    let service = rmcp::service::serve_server_with_ct(server, rmcp::transport::stdio(), ct).await?;
    eprintln!("[outrig] mcp self server ready");
    match service.waiting().await {
        Ok(reason) => {
            tracing::debug!(target: "outrig::mcp_self", "rmcp service exited: {reason:?}");
            Ok(0)
        }
        Err(err) => {
            Err(OutrigError::Configuration(format!("mcp self server task failed: {err}")).into())
        }
    }
}

impl SelfServer {
    pub(crate) fn tools() -> Vec<Tool> {
        vec![
            tool::<EmptyArgs>(
                LIST_DOCS,
                "List Docs",
                "List embedded OutRig documentation pages with one-line summaries.",
            ),
            tool::<GetDocArgs>(
                GET_DOC,
                "Get Doc",
                "Return the markdown for one embedded documentation page.",
            ),
            tool::<EmptyArgs>(
                GET_CONFIG_SCHEMA,
                "Get Config Schema",
                "Return JSON Schema plus path and image-label hints.",
            ),
            tool::<EmptyArgs>(
                LIST_BASE_IMAGES,
                "List Base Image Suggestions",
                "List base-image suggestions used by outrig image add.",
            ),
            tool::<EmptyArgs>(
                LIST_MCP_SERVER_SUGGESTIONS,
                "List MCP Server Suggestions",
                "List MCP server suggestions and shell guidance for OutRig images.",
            ),
            tool::<ValidateDockerfileArgs>(
                VALIDATE_DOCKERFILE,
                "Validate Dockerfile",
                "Return advisory warnings for a proposed OutRig image Dockerfile.",
            ),
            tool::<ValidateConfigArgs>(
                VALIDATE_CONFIG,
                "Validate Config",
                "Parse and validate a TOML fragment containing [images.<name>] entries.",
            ),
            tool::<ValidateConfigArgs>(
                VALIDATE_IMAGE_TOML,
                "Validate Image TOML",
                "Parse and validate complete standalone image.toml content.",
            ),
        ]
    }

    fn dispatch(
        &self,
        request: CallToolRequestParams,
    ) -> std::result::Result<CallToolResult, McpError> {
        match request.name.as_ref() {
            LIST_DOCS => json_result(docs::list_docs()),
            GET_DOC => {
                let args: GetDocArgs = match parse_args(request.arguments) {
                    Ok(args) => args,
                    Err(result) => return Ok(result),
                };
                match docs::get_doc(&args.page) {
                    Some(doc) => json_result(doc),
                    None => Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                        "unknown doc page: {}",
                        args.page
                    ))])),
                }
            }
            GET_CONFIG_SCHEMA => json_result(schema::get_config_schema()),
            LIST_BASE_IMAGES => json_result(suggestions::list_base_images()),
            LIST_MCP_SERVER_SUGGESTIONS => json_result(suggestions::list_mcp_server_suggestions()),
            VALIDATE_DOCKERFILE => {
                let args: ValidateDockerfileArgs = match parse_args(request.arguments) {
                    Ok(args) => args,
                    Err(result) => return Ok(result),
                };
                json_result(validate::validate_dockerfile(&args.dockerfile))
            }
            VALIDATE_CONFIG => {
                let args: ValidateConfigArgs = match parse_args(request.arguments) {
                    Ok(args) => args,
                    Err(result) => return Ok(result),
                };
                json_result(validate::validate_config(&args.toml))
            }
            VALIDATE_IMAGE_TOML => {
                let args: ValidateConfigArgs = match parse_args(request.arguments) {
                    Ok(args) => args,
                    Err(result) => return Ok(result),
                };
                json_result(validate::validate_image_toml(&args.toml))
            }
            other => Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                "unknown tool: {other}"
            ))])),
        }
    }
}

impl ServerHandler for SelfServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                "outrig-self",
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(
                "Read OutRig docs and schema, then validate proposed image artifacts. \
                 This server never writes files or builds images. If your client permits normal \
                 repo edits, write the validated artifacts directly; otherwise return exact file \
                 contents and paths for the user to install. Do not stage files in /tmp and ask \
                 for an opaque copy into .agents/outrig.",
            )
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> std::result::Result<ListToolsResult, McpError> {
        Ok(ListToolsResult::with_all_items(Self::tools()))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _ctx: RequestContext<RoleServer>,
    ) -> std::result::Result<CallToolResponse, McpError> {
        self.dispatch(request).map(Into::into)
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        Self::tools()
            .into_iter()
            .find(|tool| tool.name.as_ref() == name)
    }
}

fn tool<T: JsonSchema>(name: &'static str, title: &'static str, description: &'static str) -> Tool {
    Tool::new(name, description, input_schema::<T>())
        .with_title(title)
        .with_annotations(read_only_annotations(title))
}

fn read_only_annotations(title: &'static str) -> ToolAnnotations {
    ToolAnnotations::with_title(title)
        .read_only(true)
        .open_world(false)
}

fn input_schema<T: JsonSchema>() -> Arc<JsonObject> {
    let value = serde_json::to_value(schemars::schema_for!(T)).expect("schema serializes");
    match value {
        serde_json::Value::Object(map) => Arc::new(map),
        _ => unreachable!("schemars root schema serializes as an object"),
    }
}

#[allow(clippy::result_large_err)]
fn parse_args<T: DeserializeOwned>(
    arguments: Option<JsonObject>,
) -> std::result::Result<T, CallToolResult> {
    serde_json::from_value(serde_json::Value::Object(arguments.unwrap_or_default())).map_err(
        |err| {
            CallToolResult::error(vec![ContentBlock::text(format!(
                "invalid arguments: {err}"
            ))])
        },
    )
}

fn json_result<T: Serialize>(value: T) -> std::result::Result<CallToolResult, McpError> {
    Ok(CallToolResult::success(vec![ContentBlock::json(value)?]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::model::ContentBlock;
    use serde_json::json;

    fn call(name: &str, args: serde_json::Value) -> CallToolResult {
        let arguments = match args {
            serde_json::Value::Object(map) => Some(map),
            serde_json::Value::Null => None,
            other => panic!("test args must be object or null, got {other:?}"),
        };
        let mut request = CallToolRequestParams::new(name.to_string());
        if let Some(arguments) = arguments {
            request = request.with_arguments(arguments);
        }
        SelfServer.dispatch(request).expect("dispatch")
    }

    fn text(result: &CallToolResult) -> &str {
        match &result.content[0] {
            ContentBlock::Text(t) => &t.text,
            other => panic!("expected text content, got {other:?}"),
        }
    }

    #[test]
    fn lists_expected_tool_set() {
        let tools: Vec<String> = SelfServer::tools()
            .iter()
            .map(|tool| tool.name.as_ref().to_string())
            .collect();
        assert_eq!(
            tools,
            vec![
                LIST_DOCS,
                GET_DOC,
                GET_CONFIG_SCHEMA,
                LIST_BASE_IMAGES,
                LIST_MCP_SERVER_SUGGESTIONS,
                VALIDATE_DOCKERFILE,
                VALIDATE_CONFIG,
                VALIDATE_IMAGE_TOML,
            ],
        );
    }

    #[test]
    fn tool_list_is_read_only_and_closed_world() {
        for tool in SelfServer::tools() {
            let annotations = tool
                .annotations
                .as_ref()
                .unwrap_or_else(|| panic!("{} should have annotations", tool.name));
            assert_eq!(annotations.read_only_hint, Some(true));
            assert_eq!(annotations.open_world_hint, Some(false));
            assert!(annotations.title.is_some());
        }
    }

    #[test]
    fn get_doc_dispatch_returns_json_text() {
        let result = call(GET_DOC, json!({"page": "concepts/containers"}));
        assert_eq!(result.is_error, Some(false));
        assert!(text(&result).contains("# Containers"));
    }

    #[test]
    fn invalid_arguments_are_tool_errors() {
        let result = call(
            GET_DOC,
            json!({"page": "concepts/containers", "extra": true}),
        );
        assert_eq!(result.is_error, Some(true));
        assert!(text(&result).contains("invalid arguments"));
    }
}
