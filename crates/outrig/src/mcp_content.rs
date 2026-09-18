//! Outrig's own vocabulary for MCP tool content and tool descriptors, and the
//! four conversions between it and rmcp's.
//!
//! An MCP tool result is an ordered list of typed blocks -- text, images,
//! audio, embedded resources, links -- beside a structured payload and
//! protocol metadata. These types carry all of it. The single string a model
//! eventually sees is [`McpToolResult::render_text`], derived on demand, so
//! there is one source of truth rather than two that can disagree.
//!
//! # Why these are not rmcp's types
//!
//! rmcp types are permitted on outrig's public surface only where the item
//! exists to participate in rmcp's own machinery -- implementing an rmcp
//! trait, or being handed straight back to rmcp, as
//! [`crate::mcp_proxy::ProxyServer`]'s server impl does. Wherever a value
//! carries information outrig reports in its own right, the type is outrig's.
//! A tool result is the latter: it crosses outrig's API on the way to a
//! library consumer, so an rmcp major would otherwise be an outrig major for
//! every caller who ever touched one.
//!
//! The conversions below are therefore `pub(crate)` free functions rather
//! than `From` impls, which would put rmcp's types back on the surface.
//!
//! # Mirror rule
//!
//! Every rmcp field outrig forwards gets an outrig field of the same shape.
//! Free-form JSON -- `_meta`, schemas, `structuredContent` -- stays
//! `serde_json`. A foreign enum rmcp declares `#[non_exhaustive]` is mirrored
//! as its wire string, because such a mirror needs an escape hatch anyway:
//! hence [`McpRole`] is a real enum (rmcp's `Role` is declared exhaustive,
//! and the spec fixes it at two values) while [`McpIcon::theme`] is a
//! `String`.

use std::sync::Arc;

use rmcp::model::{
    Annotations, AudioContent, CallToolResult, ContentBlock, EmbeddedResource, Icon, IconTheme,
    ImageContent, JsonObject, MetaObject, Resource, ResourceContents, Role, TextContent, Tool,
    ToolAnnotations,
};
use serde_json::{Map, Value};

/// An MCP tool, as advertised by `tools/list`.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct McpTool {
    pub name: String,
    /// Human-readable display name, distinct from the programmatic [`name`].
    ///
    /// [`name`]: Self::name
    pub title: Option<String>,
    pub description: Option<String>,
    /// JSON Schema for the call's `arguments`. MCP requires an object; a
    /// server that sends anything else is rejected by
    /// [`ProxyServer::build`](crate::mcp_proxy::ProxyServer::build).
    pub input_schema: Value,
    /// JSON Schema for `structuredContent`, if the server declares one.
    pub output_schema: Option<Value>,
    pub annotations: Option<McpToolAnnotations>,
    pub icons: Option<Vec<McpIcon>>,
    /// Protocol-level `_meta`, forwarded verbatim.
    pub meta: Option<Map<String, Value>>,
}

impl McpTool {
    /// A tool named `name` taking `input_schema`. Assign the remaining fields
    /// on the result to add a description, schemas, or hints.
    pub fn new(name: impl Into<String>, input_schema: Value) -> Self {
        Self {
            name: name.into(),
            title: None,
            description: None,
            input_schema,
            output_schema: None,
            annotations: None,
            icons: None,
            meta: None,
        }
    }
}

/// Behavioral hints a server attaches to a tool. Every one is advisory and
/// unverified -- a client may use them to decide what to confirm with a user,
/// not to decide what a tool is allowed to do.
#[derive(Debug, Clone, Default, PartialEq)]
#[non_exhaustive]
pub struct McpToolAnnotations {
    pub title: Option<String>,
    pub read_only_hint: Option<bool>,
    pub destructive_hint: Option<bool>,
    pub idempotent_hint: Option<bool>,
    pub open_world_hint: Option<bool>,
}

/// An icon a client may display for a tool or a resource.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct McpIcon {
    pub src: String,
    pub mime_type: Option<String>,
    /// Rendered sizes the source provides, e.g. `["48x48", "96x96"]`.
    pub sizes: Option<Vec<String>>,
    /// The color scheme this icon is for, as its wire string (`"light"`,
    /// `"dark"`). A string rather than an enum because the protocol may add
    /// values; see the module's mirror rule.
    pub theme: Option<String>,
}

impl McpIcon {
    pub fn new(src: impl Into<String>) -> Self {
        Self {
            src: src.into(),
            mime_type: None,
            sizes: None,
            theme: None,
        }
    }
}

/// The result of an MCP `tools/call`.
///
/// `content` is the canonical payload: an ordered list of blocks, boundaries
/// and all. [`render_text`](Self::render_text) is the derived single-string
/// view -- what outrig's own agent loop sends a model, since `rig`'s tool
/// interface is `String`-shaped. A client reaching outrig through
/// `outrig mcp` receives the blocks themselves.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct McpToolResult {
    /// The server's content blocks, in the order it sent them.
    pub content: Vec<McpContent>,
    /// The server's `structuredContent`, if it sent one. Not rendered by
    /// [`render_text`](Self::render_text): a server that wants a model to see
    /// it is required by the spec to also serialize it into a text block.
    pub structured_content: Option<Value>,
    /// Result-level `_meta`, forwarded verbatim.
    pub meta: Option<Map<String, Value>>,
    /// The server's own `isError` flag. A transport failure is an `Err`
    /// instead; this means the tool ran and reported failure.
    pub is_error: bool,
}

impl McpToolResult {
    /// A successful call returning a single text block.
    pub fn ok(text: impl Into<String>) -> Self {
        Self::from_content(vec![McpContent::text(text)])
    }

    /// A call the server itself reported as failed, returning a single text
    /// block -- distinct from a transport error, which surfaces as an `Err`.
    pub fn error(text: impl Into<String>) -> Self {
        Self {
            is_error: true,
            ..Self::ok(text)
        }
    }

    /// A successful call returning `content`.
    pub fn from_content(content: Vec<McpContent>) -> Self {
        Self {
            content,
            structured_content: None,
            meta: None,
            is_error: false,
        }
    }

    /// The blocks rendered into one string, joined by newlines: text inline,
    /// everything else as a bracketed placeholder naming what was elided.
    ///
    /// This is a view, not the data. It exists because a language model's
    /// tool-result channel is a string, and it is lossy on purpose -- an
    /// image is not describable in it. Consumers that can do better should
    /// read [`content`](Self::content).
    pub fn render_text(&self) -> String {
        let mut out = String::new();
        for (i, block) in self.content.iter().enumerate() {
            if i > 0 {
                out.push('\n');
            }
            block.render_into(&mut out);
        }
        out
    }
}

/// One block of an MCP tool result.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum McpContent {
    Text(McpTextContent),
    /// Base64 image data. [`McpMediaContent`] is shared with `Audio` because
    /// the protocol gives the two identical shapes.
    Image(McpMediaContent),
    Audio(McpMediaContent),
    /// A resource's contents, embedded in the result.
    Resource(McpEmbeddedResource),
    /// A pointer to a resource the client may fetch separately.
    ResourceLink(McpResourceLink),
    /// A block kind this build of outrig does not model, kept as the JSON it
    /// arrived as so that it survives a round trip through the proxy intact.
    /// Reachable only when the MCP SDK has learned a block kind outrig has
    /// not: a kind the SDK itself does not know fails to decode before outrig
    /// ever sees it.
    Other {
        kind: String,
        raw: Value,
    },
}

impl McpContent {
    /// A text block carrying `text`.
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text(McpTextContent::new(text))
    }

    /// An image block carrying base64 `data` of type `mime_type`.
    pub fn image(data: impl Into<String>, mime_type: impl Into<String>) -> Self {
        Self::Image(McpMediaContent::new(data, mime_type))
    }

    /// An audio block carrying base64 `data` of type `mime_type`.
    pub fn audio(data: impl Into<String>, mime_type: impl Into<String>) -> Self {
        Self::Audio(McpMediaContent::new(data, mime_type))
    }

    fn render_into(&self, out: &mut String) {
        use std::fmt::Write as _;

        match self {
            Self::Text(t) => out.push_str(&t.text),
            Self::Image(m) => {
                let _ = write!(
                    out,
                    "[image: {}, {} base64 bytes]",
                    m.mime_type,
                    m.data.len()
                );
            }
            Self::Audio(m) => {
                let _ = write!(
                    out,
                    "[audio: {}, {} base64 bytes]",
                    m.mime_type,
                    m.data.len()
                );
            }
            Self::Resource(r) => match &r.resource {
                McpResourceContents::Text { text, .. } => out.push_str(text),
                McpResourceContents::Blob {
                    mime_type, blob, ..
                } => {
                    let mime = mime_type.as_deref().unwrap_or("application/octet-stream");
                    let _ = write!(out, "[blob: {mime}, {} base64 bytes]", blob.len());
                }
                McpResourceContents::Other { .. } => {
                    out.push_str("[unsupported resource contents]")
                }
            },
            Self::ResourceLink(link) => {
                let _ = write!(out, "[resource link: {}]", link.uri);
            }
            Self::Other { kind, .. } => {
                let _ = write!(out, "[unsupported content block: {kind}]");
            }
        }
    }
}

/// A text block.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct McpTextContent {
    pub text: String,
    pub annotations: Option<McpAnnotations>,
    pub meta: Option<Map<String, Value>>,
}

impl McpTextContent {
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            annotations: None,
            meta: None,
        }
    }
}

/// An image or audio block: base64 `data` plus its MIME type.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct McpMediaContent {
    /// Base64-encoded payload, exactly as the server sent it -- not decoded,
    /// so `data.len()` is the encoded length.
    pub data: String,
    pub mime_type: String,
    pub annotations: Option<McpAnnotations>,
    pub meta: Option<Map<String, Value>>,
}

impl McpMediaContent {
    pub fn new(data: impl Into<String>, mime_type: impl Into<String>) -> Self {
        Self {
            data: data.into(),
            mime_type: mime_type.into(),
            annotations: None,
            meta: None,
        }
    }
}

/// A resource's contents, embedded directly in a tool result.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct McpEmbeddedResource {
    pub resource: McpResourceContents,
    pub annotations: Option<McpAnnotations>,
    pub meta: Option<Map<String, Value>>,
}

impl McpEmbeddedResource {
    pub fn new(resource: McpResourceContents) -> Self {
        Self {
            resource,
            annotations: None,
            meta: None,
        }
    }
}

/// The contents of an embedded resource: text, or base64 bytes.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum McpResourceContents {
    Text {
        uri: String,
        mime_type: Option<String>,
        text: String,
        meta: Option<Map<String, Value>>,
    },
    Blob {
        uri: String,
        mime_type: Option<String>,
        /// Base64-encoded, like [`McpMediaContent::data`].
        blob: String,
        meta: Option<Map<String, Value>>,
    },
    /// Contents in a shape this build does not model, kept as JSON so they
    /// survive a round trip. See [`McpContent::Other`].
    Other { raw: Value },
}

impl McpResourceContents {
    /// Text contents for `uri`, with no MIME type declared.
    pub fn text(uri: impl Into<String>, text: impl Into<String>) -> Self {
        Self::Text {
            uri: uri.into(),
            mime_type: None,
            text: text.into(),
            meta: None,
        }
    }

    /// Base64 contents for `uri`, with no MIME type declared.
    pub fn blob(uri: impl Into<String>, blob: impl Into<String>) -> Self {
        Self::Blob {
            uri: uri.into(),
            mime_type: None,
            blob: blob.into(),
            meta: None,
        }
    }
}

/// A pointer to a resource, for a client that wants to fetch it itself.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct McpResourceLink {
    pub uri: String,
    /// The programmatic name of the resource.
    pub name: String,
    pub title: Option<String>,
    pub description: Option<String>,
    pub mime_type: Option<String>,
    /// Size of the raw contents in bytes, before base64, if the server knows.
    pub size: Option<u64>,
    pub icons: Option<Vec<McpIcon>>,
    pub annotations: Option<McpAnnotations>,
    pub meta: Option<Map<String, Value>>,
}

impl McpResourceLink {
    pub fn new(uri: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            uri: uri.into(),
            name: name.into(),
            title: None,
            description: None,
            mime_type: None,
            size: None,
            icons: None,
            annotations: None,
            meta: None,
        }
    }
}

/// Display hints a server attaches to a block or a resource.
#[derive(Debug, Clone, Default, PartialEq)]
#[non_exhaustive]
pub struct McpAnnotations {
    /// Who the block is for. Absent means everyone.
    pub audience: Option<Vec<McpRole>>,
    /// How important the block is, from `0.0` to `1.0`.
    pub priority: Option<f32>,
    /// RFC 3339 timestamp of the underlying data's last change.
    pub last_modified: Option<String>,
}

/// A conversation participant. Exhaustive rather than `#[non_exhaustive]`:
/// MCP fixes the set at these two, and the SDK declares its own that way.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum McpRole {
    User,
    Assistant,
}

// ---------------------------------------------------------------------------
// Conversions. One function per leg, each pure, so each is exercisable
// without a live server: a fake that can only reach the proxy's two legs
// would let a round trip pass while the client's two discarded the data
// first, which is the failure these replace.
// ---------------------------------------------------------------------------

/// `tools/list` entry, upstream server -> outrig.
pub(crate) fn tool_from_rmcp(tool: Tool) -> McpTool {
    let input_schema = tool.schema_as_json_value();
    let output_schema = tool
        .output_schema
        .as_deref()
        .map(|s| Value::Object(s.clone()));
    McpTool {
        name: tool.name.into_owned(),
        title: tool.title,
        // An empty description is the same as none, and saying so here keeps
        // every consumer from having to.
        description: tool
            .description
            .and_then(|d| (!d.is_empty()).then(|| d.into_owned())),
        input_schema,
        output_schema,
        annotations: tool.annotations.map(tool_annotations_from_rmcp),
        icons: tool
            .icons
            .map(|icons| icons.into_iter().map(icon_from_rmcp).collect()),
        meta: tool.meta.map(meta_from_rmcp),
    }
}

/// `tools/list` entry, outrig -> downstream client.
///
/// `public_name` replaces the upstream name, because the proxy advertises
/// namespaced names. `input_schema` comes in separately as the object form
/// [`ProxyServer::build`](crate::mcp_proxy::ProxyServer::build) validated and
/// interned once, rather than being re-derived from
/// [`McpTool::input_schema`] on every `tools/list`.
pub(crate) fn tool_to_rmcp(
    public_name: String,
    tool: &McpTool,
    input_schema: Arc<JsonObject>,
) -> Tool {
    let mut out = Tool::new_with_raw(
        public_name,
        tool.description.clone().map(Into::into),
        input_schema,
    );
    out.title = tool.title.clone();
    out.output_schema = tool.output_schema.as_ref().and_then(|s| match s {
        Value::Object(map) => Some(Arc::new(map.clone())),
        _ => None,
    });
    out.annotations = tool.annotations.as_ref().map(tool_annotations_to_rmcp);
    out.icons = tool
        .icons
        .as_ref()
        .map(|icons| icons.iter().map(icon_to_rmcp).collect());
    out.meta = tool.meta.clone().map(MetaObject::from);
    out
}

/// `tools/call` result, upstream server -> outrig.
pub(crate) fn result_from_rmcp(result: CallToolResult) -> McpToolResult {
    McpToolResult {
        content: result.content.into_iter().map(content_from_rmcp).collect(),
        structured_content: result.structured_content,
        meta: result.meta.map(meta_from_rmcp),
        is_error: result.is_error.unwrap_or(false),
    }
}

/// `tools/call` result, outrig -> downstream client.
///
/// `resultType` is deliberately not carried through: `CallToolResult`'s
/// constructors answer `complete`, and the other values (`task`,
/// `input_required`) promise follow-up methods the proxy does not implement.
pub(crate) fn result_to_rmcp(result: McpToolResult) -> CallToolResult {
    let blocks = result.content.into_iter().map(content_to_rmcp).collect();
    let mut out = if result.is_error {
        CallToolResult::error(blocks)
    } else {
        CallToolResult::success(blocks)
    };
    out.structured_content = result.structured_content;
    out.meta = result.meta.map(MetaObject::from);
    out
}

fn content_from_rmcp(block: ContentBlock) -> McpContent {
    match block {
        ContentBlock::Text(t) => McpContent::Text(McpTextContent {
            text: t.text,
            annotations: t.annotations.map(annotations_from_rmcp),
            meta: t.meta.map(meta_from_rmcp),
        }),
        ContentBlock::Image(i) => McpContent::Image(McpMediaContent {
            data: i.data,
            mime_type: i.mime_type,
            annotations: i.annotations.map(annotations_from_rmcp),
            meta: i.meta.map(meta_from_rmcp),
        }),
        ContentBlock::Audio(a) => McpContent::Audio(McpMediaContent {
            data: a.data,
            mime_type: a.mime_type,
            annotations: a.annotations.map(annotations_from_rmcp),
            meta: a.meta.map(meta_from_rmcp),
        }),
        ContentBlock::Resource(r) => McpContent::Resource(McpEmbeddedResource {
            resource: resource_contents_from_rmcp(r.resource),
            annotations: r.annotations.map(annotations_from_rmcp),
            meta: r.meta.map(meta_from_rmcp),
        }),
        ContentBlock::ResourceLink(link) => McpContent::ResourceLink(McpResourceLink {
            uri: link.uri,
            name: link.name,
            title: link.title,
            description: link.description,
            mime_type: link.mime_type,
            size: link.size,
            icons: link
                .icons
                .map(|icons| icons.into_iter().map(icon_from_rmcp).collect()),
            annotations: link.annotations.map(annotations_from_rmcp),
            meta: link.meta.map(meta_from_rmcp),
        }),
        other => opaque(&other),
    }
}

fn content_to_rmcp(content: McpContent) -> ContentBlock {
    match content {
        McpContent::Text(t) => {
            let mut out = TextContent::new(t.text);
            out.annotations = t.annotations.map(annotations_to_rmcp);
            out.meta = t.meta.map(MetaObject::from);
            ContentBlock::Text(out)
        }
        McpContent::Image(m) => {
            let mut out = ImageContent::new(m.data, m.mime_type);
            out.annotations = m.annotations.map(annotations_to_rmcp);
            out.meta = m.meta.map(MetaObject::from);
            ContentBlock::Image(out)
        }
        McpContent::Audio(m) => {
            let mut out = AudioContent::new(m.data, m.mime_type);
            out.annotations = m.annotations.map(annotations_to_rmcp);
            out.meta = m.meta.map(MetaObject::from);
            ContentBlock::Audio(out)
        }
        McpContent::Resource(r) => {
            let mut out = EmbeddedResource::new(resource_contents_to_rmcp(r.resource));
            out.annotations = r.annotations.map(annotations_to_rmcp);
            out.meta = r.meta.map(MetaObject::from);
            ContentBlock::Resource(out)
        }
        McpContent::ResourceLink(link) => {
            let mut out = Resource::new(link.uri, link.name);
            out.title = link.title;
            out.description = link.description;
            out.mime_type = link.mime_type;
            out.size = link.size;
            out.icons = link
                .icons
                .as_ref()
                .map(|icons| icons.iter().map(icon_to_rmcp).collect());
            out.annotations = link.annotations.map(annotations_to_rmcp);
            out.meta = link.meta.map(MetaObject::from);
            ContentBlock::ResourceLink(out)
        }
        // The JSON came from serializing a block the SDK produced, so it
        // decodes again. Dropping the whole result because one block will not
        // is worse than forwarding the placeholder a reader would have seen.
        McpContent::Other { kind, raw } => serde_json::from_value(raw)
            .unwrap_or_else(|_| ContentBlock::text(format!("[unsupported content block: {kind}]"))),
    }
}

/// Capture a block kind this build does not model as the JSON it arrived as.
fn opaque(block: &ContentBlock) -> McpContent {
    let raw = serde_json::to_value(block).unwrap_or(Value::Null);
    let kind = raw
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_string();
    McpContent::Other { kind, raw }
}

fn resource_contents_from_rmcp(contents: ResourceContents) -> McpResourceContents {
    match contents {
        ResourceContents::TextResourceContents {
            uri,
            mime_type,
            text,
            meta,
        } => McpResourceContents::Text {
            uri,
            mime_type,
            text,
            meta: meta.map(meta_from_rmcp),
        },
        ResourceContents::BlobResourceContents {
            uri,
            mime_type,
            blob,
            meta,
        } => McpResourceContents::Blob {
            uri,
            mime_type,
            blob,
            meta: meta.map(meta_from_rmcp),
        },
        other => McpResourceContents::Other {
            raw: serde_json::to_value(&other).unwrap_or(Value::Null),
        },
    }
}

fn resource_contents_to_rmcp(contents: McpResourceContents) -> ResourceContents {
    match contents {
        McpResourceContents::Text {
            uri,
            mime_type,
            text,
            meta,
        } => ResourceContents::TextResourceContents {
            uri,
            mime_type,
            text,
            meta: meta.map(MetaObject::from),
        },
        McpResourceContents::Blob {
            uri,
            mime_type,
            blob,
            meta,
        } => ResourceContents::BlobResourceContents {
            uri,
            mime_type,
            blob,
            meta: meta.map(MetaObject::from),
        },
        // As in `content_to_rmcp`: a value that will not decode becomes an
        // empty text resource rather than taking the result down with it.
        McpResourceContents::Other { raw } => {
            serde_json::from_value(raw).unwrap_or(ResourceContents::TextResourceContents {
                uri: String::new(),
                mime_type: None,
                text: "[unsupported resource contents]".to_string(),
                meta: None,
            })
        }
    }
}

/// `_meta` in, the mirror of [`MetaObject::from`] on the way back out.
fn meta_from_rmcp(meta: MetaObject) -> Map<String, Value> {
    meta.0
}

fn annotations_from_rmcp(annotations: Annotations) -> McpAnnotations {
    McpAnnotations {
        audience: annotations
            .audience
            .map(|roles| roles.into_iter().map(role_from_rmcp).collect()),
        priority: annotations.priority,
        last_modified: annotations.last_modified,
    }
}

fn annotations_to_rmcp(annotations: McpAnnotations) -> Annotations {
    let mut out = Annotations::default();
    out.audience = annotations
        .audience
        .map(|roles| roles.into_iter().map(role_to_rmcp).collect());
    out.priority = annotations.priority;
    out.last_modified = annotations.last_modified;
    out
}

fn role_from_rmcp(role: Role) -> McpRole {
    match role {
        Role::User => McpRole::User,
        Role::Assistant => McpRole::Assistant,
    }
}

fn role_to_rmcp(role: McpRole) -> Role {
    match role {
        McpRole::User => Role::User,
        McpRole::Assistant => Role::Assistant,
    }
}

fn tool_annotations_from_rmcp(annotations: ToolAnnotations) -> McpToolAnnotations {
    McpToolAnnotations {
        title: annotations.title,
        read_only_hint: annotations.read_only_hint,
        destructive_hint: annotations.destructive_hint,
        idempotent_hint: annotations.idempotent_hint,
        open_world_hint: annotations.open_world_hint,
    }
}

fn tool_annotations_to_rmcp(annotations: &McpToolAnnotations) -> ToolAnnotations {
    let mut out = ToolAnnotations::default();
    out.title = annotations.title.clone();
    out.read_only_hint = annotations.read_only_hint;
    out.destructive_hint = annotations.destructive_hint;
    out.idempotent_hint = annotations.idempotent_hint;
    out.open_world_hint = annotations.open_world_hint;
    out
}

fn icon_from_rmcp(icon: Icon) -> McpIcon {
    McpIcon {
        src: icon.src,
        mime_type: icon.mime_type,
        sizes: icon.sizes,
        theme: icon.theme.as_ref().and_then(theme_to_wire),
    }
}

fn icon_to_rmcp(icon: &McpIcon) -> Icon {
    let mut out = Icon::new(icon.src.clone());
    out.mime_type = icon.mime_type.clone();
    out.sizes = icon.sizes.clone();
    // A theme string the SDK cannot name is dropped rather than guessed at;
    // it is decoration, and the icon itself still renders.
    out.theme = icon
        .theme
        .as_ref()
        .and_then(|t| serde_json::from_value(Value::String(t.clone())).ok());
    out
}

/// The wire spelling of a theme, via serde so a value added to the SDK after
/// this was written still round-trips.
fn theme_to_wire(theme: &IconTheme) -> Option<String> {
    match serde_json::to_value(theme) {
        Ok(Value::String(s)) => Some(s),
        _ => None,
    }
}

#[cfg(test)]
#[path = "mcp_content_tests.rs"]
pub(crate) mod mcp_content_tests;
