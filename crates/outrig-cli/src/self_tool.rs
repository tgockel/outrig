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
    ValidateConfigArgs, ValidateDockerfileArgs, ValidateImageTomlArgs,
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
                 [images.<name>] entries, returning parse and validation errors, \
                 and a warning for each key outrig would ignore. Checked as a \
                 repo config (.agents/outrig/config.toml), which may not carry \
                 the machine-level keys; pass scope = \"global\" for a fragment \
                 bound for ~/.outrig/config.toml. Run it on any config you write \
                 before telling the user it is ready."
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
            Self::ValidateConfig => json!({
                "type": "object",
                "properties": {
                    "toml": {
                        "type": "string",
                        "description": "Full text of the TOML to parse and validate."
                    },
                    "scope": {
                        "type": "string",
                        "enum": ["repo", "global"],
                        "description": "Which config file the fragment is for. \
                                        Defaults to \"repo\" \
                                        (.agents/outrig/config.toml), which may not set \
                                        session-root, model-cache-root, the [network] \
                                        policy keys, [repo].env-allow, or a provider \
                                        carrying an api-key."
                    }
                },
                "required": ["toml"],
                "additionalProperties": false
            }),
            Self::ValidateImageToml => json!({
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
                    encode(&validate::validate_dockerfile(&args.dockerfile))
                }
                Kind::ValidateConfig => {
                    let args: ValidateConfigArgs = parse_args(&args)?;
                    encode(&validate::validate_config(&args.toml, args.scope))
                }
                Kind::ValidateImageToml => {
                    let args: ValidateImageTomlArgs = parse_args(&args)?;
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
            .call(json!({ "toml": "[images.x]\ndockerfile = 5\n" }).to_string())
            .await
            .expect("validation runs");
        let parsed: Value = serde_json::from_str(&out).expect("json");
        assert_eq!(parsed["valid"], json!(false), "{out}");
    }

    /// An unknown key is what `outrig run` warns about and ignores: a warning
    /// naming it, on a fragment that is otherwise valid.
    #[tokio::test]
    async fn validate_config_warns_about_an_unknown_key() {
        let toml = "[images.x]\nimage-name = \"debian\"\nbogus-key = 1\n";
        let out = tool(Kind::ValidateConfig)
            .call(json!({ "toml": toml }).to_string())
            .await
            .expect("validation runs");
        let parsed: Value = serde_json::from_str(&out).expect("json");
        assert_eq!(parsed["valid"], json!(true), "{out}");
        assert_eq!(
            parsed["warnings"][0]["code"],
            json!("unknown_config_key"),
            "{out}"
        );
        assert_eq!(parsed["warnings"][0]["line"], json!(3), "{out}");
    }

    /// The default scope is the repo config, so a fragment carrying a key
    /// only the operator's own file may set is refused here rather than at
    /// `outrig run`, after the user installed what this tool approved.
    #[tokio::test]
    async fn validate_config_defaults_to_the_repo_rules() {
        let toml = "[network]\nmode = \"filter\"\nallow = [\"github.com:443\"]\n";
        let out = tool(Kind::ValidateConfig)
            .call(json!({ "toml": toml }).to_string())
            .await
            .expect("validation runs");
        let parsed: Value = serde_json::from_str(&out).expect("json");
        assert_eq!(parsed["valid"], json!(false), "{out}");
        assert_eq!(
            parsed["errors"][0]["code"],
            json!("repo_config_invalid"),
            "{out}"
        );

        let out = tool(Kind::ValidateConfig)
            .call(json!({ "toml": toml, "scope": "global" }).to_string())
            .await
            .expect("validation runs");
        let parsed: Value = serde_json::from_str(&out).expect("json");
        assert_eq!(parsed["valid"], json!(true), "{out}");
    }

    /// `validate_image_toml` has no scope to pick, and advertising one would
    /// invite an agent to send an argument its decoder refuses.
    #[tokio::test]
    async fn validate_image_toml_takes_no_scope() {
        let toml = "[image]\nref = \"x\"\n\n[mcp]\nfs = [\"mcp-server-filesystem\"]\n";
        tool(Kind::ValidateImageToml)
            .call(json!({ "toml": toml }).to_string())
            .await
            .expect("validation runs");
        assert!(
            tool(Kind::ValidateImageToml)
                .call(json!({ "toml": toml, "scope": "repo" }).to_string())
                .await
                .is_err(),
            "a scope it would ignore must not decode"
        );
        assert!(
            tool(Kind::ValidateImageToml).parameters()["properties"]
                .get("scope")
                .is_none(),
            "nor be advertised"
        );
    }

    /// These schemas are hand-written where `outrig mcp self` derives its own
    /// from the same arg structs, so the two front-ends can drift in what
    /// they advertise even while sharing a decoder -- which is exactly what a
    /// model reads to decide how to call a tool. Argument names and which are
    /// required are the part that has to agree; the prose does not.
    #[test]
    fn advertised_arguments_match_what_outrig_mcp_self_derives() {
        let derived = crate::mcp_self::server::SelfServer::tools();
        for kind in Kind::ALL {
            let served = derived
                .iter()
                .find(|t| t.name == kind.tool())
                .unwrap_or_else(|| panic!("{} is served", kind.tool()));
            let served = Value::Object((*served.input_schema).clone());
            let builtin = kind.parameters();

            let names = |schema: &Value| {
                let mut keys: Vec<String> = schema["properties"]
                    .as_object()
                    .map(|m| m.keys().cloned().collect())
                    .unwrap_or_default();
                keys.sort_unstable();
                keys
            };
            let required = |schema: &Value| {
                let mut keys: Vec<String> = schema
                    .get("required")
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_str())
                            .map(str::to_string)
                            .collect()
                    })
                    .unwrap_or_default();
                keys.sort_unstable();
                keys
            };
            assert_eq!(names(&builtin), names(&served), "{}", kind.tool());
            assert_eq!(required(&builtin), required(&served), "{}", kind.tool());
        }
    }

    #[tokio::test]
    async fn unknown_arguments_are_rejected() {
        let err = tool(Kind::GetDoc)
            .call(r#"{"page":"reference/config","extra":1}"#.to_string())
            .await;
        assert!(err.is_err(), "deny_unknown_fields must reject `extra`");
    }
}
