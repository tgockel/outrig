//! outrig's self-documentation tools, exposed to the model as `outrig__<tool>`.
//!
//! The same eight tools [`outrig mcp self`](crate::mcp_self::server) serves to
//! an external authoring client, reaching an in-session agent through the
//! built-in path instead of an MCP server. They are pure host-side functions
//! over embedded data -- no container, no workspace, no network -- so there is
//! nothing to install in an image, and [`McpServerSpec`] has no host placement
//! to run them under: every form of it execs inside a container.
//!
//! Offered only when the session fell through to the built-in default
//! image-config, which is exactly the user who has not written a config yet
//! and stands to gain most from outrig being able to explain itself. A
//! configured repo's tool list is unchanged.
//!
//! [`McpServerSpec`]: outrig::config::McpServerSpec

use rig::tool::{ToolDyn, ToolError};
use rig::wasm_compat::WasmBoxedFuture;
use serde_json::{Value, json};

use crate::builtin_tool::{fail, name_of, parse_args};
use crate::mcp_self::args::{
    GET_CONFIG_SCHEMA, GET_DOC, GetDocArgs, LIST_BASE_IMAGES, LIST_DOCS,
    LIST_MCP_SERVER_SUGGESTIONS, VALIDATE_CONFIG, VALIDATE_DOCKERFILE, VALIDATE_IMAGE_TOML,
    ValidateConfigArgs, ValidateDockerfileArgs,
};
use crate::mcp_self::{docs, schema, suggestions, validate};
use crate::session_tool::SessionTool;

fn encode<T: serde::Serialize>(value: &T) -> Result<String, ToolError> {
    serde_json::to_string(value).map_err(ToolError::JsonError)
}

/// Which of the eight this instance is. One enum rather than eight structs:
/// the tools are all read-only and stateless, so keeping their names,
/// descriptions, and schemas in one table is what makes them easy to hold
/// beside each other.
#[derive(Clone, Copy)]
enum Kind {
    ListDocs,
    GetDoc,
    GetConfigSchema,
    ListBaseImages,
    ListMcpServers,
    ValidateDockerfile,
    ValidateConfig,
    ValidateImageToml,
}

impl Kind {
    /// Advertised order. Docs first: an agent that reads before it validates
    /// gives better answers than one that guesses and checks.
    const ALL: [Kind; 8] = [
        Self::ListDocs,
        Self::GetDoc,
        Self::GetConfigSchema,
        Self::ListBaseImages,
        Self::ListMcpServers,
        Self::ValidateDockerfile,
        Self::ValidateConfig,
        Self::ValidateImageToml,
    ];

    /// The unprefixed tool name, from the same constants `outrig mcp self`
    /// serves -- so the two surfaces cannot drift in what they are called.
    fn tool(self) -> &'static str {
        match self {
            Self::ListDocs => LIST_DOCS,
            Self::GetDoc => GET_DOC,
            Self::GetConfigSchema => GET_CONFIG_SCHEMA,
            Self::ListBaseImages => LIST_BASE_IMAGES,
            Self::ListMcpServers => LIST_MCP_SERVER_SUGGESTIONS,
            Self::ValidateDockerfile => VALIDATE_DOCKERFILE,
            Self::ValidateConfig => VALIDATE_CONFIG,
            Self::ValidateImageToml => VALIDATE_IMAGE_TOML,
        }
    }

    fn description(self) -> &'static str {
        match self {
            Self::ListDocs => {
                "List outrig's own documentation pages with one-line summaries. \
                 Start here when the user asks how outrig works, how to configure \
                 it, or what a config key means -- the pages are the shipped docs \
                 for this exact build, so they beat recalling it."
            }
            Self::GetDoc => {
                "Return the full markdown of one outrig documentation page. Pass a \
                 `page` from outrig__list_docs."
            }
            Self::GetConfigSchema => {
                "Return the JSON Schema for outrig's config.toml, plus where the \
                 repo and global config files live and which OCI labels an image \
                 can carry. Use it before writing or editing a config."
            }
            Self::ListBaseImages => {
                "List the base images outrig suggests for an agent container, with \
                 a one-line note on each. Suggestions, not a registry."
            }
            Self::ListMcpServers => {
                "List the MCP servers outrig knows how to install into an image, \
                 with their packages and run commands, plus guidance on shell \
                 servers. Suggestions, not a registry."
            }
            Self::ValidateDockerfile => {
                "Check a proposed agent-container Dockerfile against outrig's \
                 conventions and return advisory warnings. Run it on any \
                 Dockerfile you write before telling the user it is ready."
            }
            Self::ValidateConfig => {
                "Parse and validate a config.toml fragment containing \
                 [images.<name>] entries, returning parse and validation errors. \
                 Run it on any config you write before telling the user it is ready."
            }
            Self::ValidateImageToml => {
                "Parse and validate the complete contents of a standalone \
                 image.toml (the file `outrig image init` writes)."
            }
        }
    }

    fn parameters(self) -> Value {
        match self {
            Self::GetDoc => json!({
                "type": "object",
                "properties": {
                    "page": {
                        "type": "string",
                        "description": "Page identifier from outrig__list_docs, \
                                        e.g. \"reference/config\"."
                    }
                },
                "required": ["page"],
                "additionalProperties": false
            }),
            Self::ValidateDockerfile => json!({
                "type": "object",
                "properties": {
                    "dockerfile": {
                        "type": "string",
                        "description": "Full text of the Dockerfile to check."
                    }
                },
                "required": ["dockerfile"],
                "additionalProperties": false
            }),
            Self::ValidateConfig | Self::ValidateImageToml => json!({
                "type": "object",
                "properties": {
                    "toml": {
                        "type": "string",
                        "description": "Full text of the TOML to parse and validate."
                    }
                },
                "required": ["toml"],
                "additionalProperties": false
            }),
            Self::ListDocs
            | Self::GetConfigSchema
            | Self::ListBaseImages
            | Self::ListMcpServers => json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
        }
    }
}

/// One of outrig's self-documentation tools.
#[derive(Clone)]
pub struct SelfTool {
    kind: Kind,
}

impl ToolDyn for SelfTool {
    fn name(&self) -> String {
        name_of(self.kind.tool())
    }

    fn description(&self) -> String {
        self.kind.description().to_string()
    }

    fn parameters(&self) -> Value {
        self.kind.parameters()
    }

    fn call<'a>(&'a self, args: String) -> WasmBoxedFuture<'a, Result<String, ToolError>> {
        Box::pin(async move {
            match self.kind {
                Kind::ListDocs => encode(&docs::list_docs()),
                Kind::GetDoc => {
                    let args: GetDocArgs = parse_args(&args)?;
                    match docs::get_doc(&args.page) {
                        Some(doc) => encode(&doc),
                        None => fail(format!(
                            "unknown doc page: {}; call outrig__list_docs for the set",
                            args.page
                        )),
                    }
                }
                Kind::GetConfigSchema => encode(&schema::get_config_schema()),
                Kind::ListBaseImages => encode(&suggestions::list_base_images()),
                Kind::ListMcpServers => encode(&suggestions::list_mcp_server_suggestions()),
                Kind::ValidateDockerfile => {
                    let args: ValidateDockerfileArgs = parse_args(&args)?;
                    // Probed here rather than at registration: it costs a
                    // ~1.4s `podman info`, this is the one tool of the eight
                    // that reads it, and a session that only reads docs should
                    // not pay for advice it never asks for. The probe caches in
                    // a `OnceCell`, so at most one call pays it.
                    let bootstrap = validate::UserBootstrap::for_this_host().await;
                    encode(&validate::validate_dockerfile(&args.dockerfile, bootstrap))
                }
                Kind::ValidateConfig => {
                    let args: ValidateConfigArgs = parse_args(&args)?;
                    encode(&validate::validate_config(&args.toml))
                }
                Kind::ValidateImageToml => {
                    let args: ValidateConfigArgs = parse_args(&args)?;
                    encode(&validate::validate_image_toml(&args.toml))
                }
            }
        })
    }
}

/// Every self-documentation tool, in advertised order.
pub fn self_tools() -> Vec<SessionTool> {
    Kind::ALL
        .into_iter()
        .map(|kind| SessionTool::new(SelfTool { kind }))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool(kind: Kind) -> SelfTool {
        SelfTool { kind }
    }

    /// These must look exactly like MCP tools to the model, which means going
    /// through the same `<server>__<tool>` sanitizer the subagent tools use.
    #[test]
    fn names_use_the_reserved_prefix() {
        assert_eq!(tool(Kind::ListDocs).name(), "outrig__list_docs");
        assert_eq!(
            tool(Kind::ListMcpServers).name(),
            "outrig__list_mcp_server_suggestions"
        );
        assert_eq!(
            tool(Kind::ValidateImageToml).name(),
            "outrig__validate_image_toml"
        );
    }

    /// The two front-ends over `mcp_self` must offer the same set. They share
    /// the name constants, so a rename cannot split them -- but adding a tool
    /// to one and forgetting the other still can, and that is what this pins.
    #[test]
    fn the_same_tools_are_offered_as_outrig_mcp_self_serves() {
        let sorted = |mut names: Vec<String>| {
            names.sort_unstable();
            names
        };
        let builtin = sorted(Kind::ALL.iter().map(|k| k.tool().to_string()).collect());
        let served = sorted(
            crate::mcp_self::server::SelfServer::tools()
                .iter()
                .map(|t| t.name.to_string())
                .collect(),
        );
        assert_eq!(builtin, served);
        assert_eq!(
            builtin.len(),
            Kind::ALL.len(),
            "duplicate name in Kind::ALL"
        );
    }

    /// A schema that advertises a required argument the decoder does not take
    /// (or vice versa) is the drift this pins.
    #[tokio::test]
    async fn argless_tools_accept_empty_arguments() {
        for kind in [
            Kind::ListDocs,
            Kind::GetConfigSchema,
            Kind::ListBaseImages,
            Kind::ListMcpServers,
        ] {
            let out = tool(kind).call(String::new()).await;
            assert!(out.is_ok(), "{}: {:?}", kind.tool(), out.err());
        }
    }

    #[tokio::test]
    async fn get_doc_reports_an_unknown_page_rather_than_failing_opaquely() {
        let err = tool(Kind::GetDoc)
            .call(r#"{"page":"nope"}"#.to_string())
            .await
            .expect_err("unknown page");
        let text = err.to_string();
        assert!(text.contains("nope"), "{text}");
        assert!(text.contains("outrig__list_docs"), "{text}");
    }

    /// Every page `list_docs` advertises must actually be fetchable, or the
    /// agent's first move after listing is a dead end.
    #[tokio::test]
    async fn every_listed_page_is_retrievable() {
        let listing = tool(Kind::ListDocs)
            .call(String::new())
            .await
            .expect("list");
        let listing: Value = serde_json::from_str(&listing).expect("json");
        let entries = listing["docs"].as_array().expect("docs array");
        assert!(!entries.is_empty(), "no doc pages are embedded");

        for entry in entries {
            let id = entry["page"].as_str().expect("page id");
            tool(Kind::GetDoc)
                .call(json!({ "page": id }).to_string())
                .await
                .unwrap_or_else(|e| panic!("{id} is listed but not retrievable: {e}"));
        }
    }

    #[tokio::test]
    async fn validate_config_reports_a_bad_fragment() {
        let out = tool(Kind::ValidateConfig)
            .call(json!({ "toml": "[images.x]\nbogus-key = 1\n" }).to_string())
            .await
            .expect("validation runs");
        let parsed: Value = serde_json::from_str(&out).expect("json");
        assert_eq!(parsed["valid"], json!(false), "{out}");
    }

    #[tokio::test]
    async fn unknown_arguments_are_rejected() {
        let err = tool(Kind::GetDoc)
            .call(r#"{"page":"reference/config","extra":1}"#.to_string())
            .await;
        assert!(err.is_err(), "deny_unknown_fields must reject `extra`");
    }
}
