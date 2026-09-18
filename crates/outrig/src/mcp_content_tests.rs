//! Per-leg tests for the four conversions in [`super`].
//!
//! Each leg is driven directly, against a hand-built value, so none can pass
//! on another's behalf: a fidelity suite that only reached the proxy would
//! stay green while [`crate::mcp::McpClient`] discarded the data one step
//! earlier, which is how the reduction these replace went unnoticed.

use std::sync::Arc;

use rmcp::model::{
    Annotations, AudioContent, CallToolResult, ContentBlock, EmbeddedResource, Icon, IconTheme,
    ImageContent, JsonObject, MetaObject, Resource, ResourceContents, ResultType, Role,
    TextContent, Tool, ToolAnnotations,
};
use serde_json::{Map, Value, json};

use super::*;

/// A distinguishable `_meta` payload, so a test can tell which one survived.
fn meta(tag: &str) -> Map<String, Value> {
    let mut map = Map::new();
    map.insert("outrig.test/tag".to_string(), json!(tag));
    map
}

fn rmcp_meta(tag: &str) -> MetaObject {
    MetaObject::from(meta(tag))
}

fn rmcp_annotations() -> Annotations {
    let mut out = Annotations::default();
    out.audience = Some(vec![Role::Assistant, Role::User]);
    out.priority = Some(0.5);
    out.last_modified = Some("2026-09-18T00:00:00Z".to_string());
    out
}

fn rmcp_icon() -> Icon {
    Icon::new("https://example.invalid/icon.png")
        .with_mime_type("image/png")
        .with_sizes(vec!["48x48".to_string(), "96x96".to_string()])
        .with_theme(IconTheme::Dark)
}

/// One result carrying every block kind the protocol has, plus structured
/// content and `_meta`. `pub(crate)` because the proxy's dispatch tests drive
/// the same value through the other two legs.
pub(crate) fn rmcp_mixed_result() -> CallToolResult {
    let mut blob = EmbeddedResource::new(ResourceContents::BlobResourceContents {
        uri: "file:///report.pdf".to_string(),
        mime_type: Some("application/pdf".to_string()),
        blob: "YmxvYg==".to_string(),
        meta: Some(rmcp_meta("blob-resource")),
    });
    blob.annotations = Some(rmcp_annotations());

    let link = Resource::new("file:///linked.rs", "linked")
        .with_title("Linked")
        .with_description("a resource the client may fetch itself")
        .with_mime_type("text/x-rust")
        .with_size(4096)
        .with_icons(vec![rmcp_icon()])
        .with_meta(rmcp_meta("resource-link"));

    let mut out = CallToolResult::success(vec![
        ContentBlock::Text(
            TextContent::new("first")
                .with_annotations(rmcp_annotations())
                .with_meta(rmcp_meta("text")),
        ),
        ContentBlock::Image(ImageContent::new("aW1n", "image/png").with_meta(rmcp_meta("image"))),
        ContentBlock::Audio(AudioContent::new("YXVk", "audio/wav")),
        ContentBlock::Resource(EmbeddedResource::new(
            ResourceContents::TextResourceContents {
                uri: "file:///notes.txt".to_string(),
                mime_type: Some("text/plain".to_string()),
                text: "notes".to_string(),
                meta: None,
            },
        )),
        ContentBlock::Resource(blob),
        ContentBlock::ResourceLink(link),
    ]);
    out.structured_content = Some(json!({"rows": 3, "truncated": false}));
    out.meta = Some(rmcp_meta("result"));
    out
}

/// The rendering of [`rmcp_mixed_result`]: every placeholder the reduced
/// projection used to produce, unchanged, so no transcript shifts.
pub(crate) const MIXED_RENDERING: &str = "first\n\
     [image: image/png, 4 base64 bytes]\n\
     [audio: audio/wav, 4 base64 bytes]\n\
     notes\n\
     [blob: application/pdf, 8 base64 bytes]\n\
     [resource link: file:///linked.rs]";

/// A tool descriptor using every field `tools/list` can carry.
pub(crate) fn rmcp_rich_tool() -> Tool {
    let mut annotations = ToolAnnotations::default();
    annotations.title = Some("Read a file".to_string());
    annotations.read_only_hint = Some(true);
    annotations.destructive_hint = Some(false);
    annotations.idempotent_hint = Some(true);
    annotations.open_world_hint = Some(false);

    let mut out = Tool::new(
        "read_file",
        "read one file",
        Arc::new(rich_input_schema_object()),
    );
    out.title = Some("Read File".to_string());
    out.output_schema = Some(Arc::new(
        json!({"type": "object", "properties": {"text": {"type": "string"}}})
            .as_object()
            .unwrap()
            .clone(),
    ));
    out.annotations = Some(annotations);
    out.icons = Some(vec![rmcp_icon()]);
    out.meta = Some(rmcp_meta("tool"));
    out
}

fn rich_input_schema_object() -> JsonObject {
    json!({"type": "object", "properties": {"path": {"type": "string"}}})
        .as_object()
        .unwrap()
        .clone()
}

// ---------------------------------------------------------------------------
// Leg 1: upstream results in.
// ---------------------------------------------------------------------------

#[test]
fn result_from_rmcp_keeps_every_block_with_its_boundaries() {
    let out = result_from_rmcp(rmcp_mixed_result());

    assert_eq!(out.content.len(), 6, "one outrig block per rmcp block");
    assert!(!out.is_error);

    let McpContent::Text(text) = &out.content[0] else {
        panic!("block 0 should be text, got {:?}", out.content[0]);
    };
    assert_eq!(text.text, "first");
    assert_eq!(text.meta.as_ref(), Some(&meta("text")));
    let annotations = text.annotations.as_ref().expect("text annotations");
    assert_eq!(
        annotations.audience.as_deref(),
        Some([McpRole::Assistant, McpRole::User].as_slice())
    );
    assert_eq!(annotations.priority, Some(0.5));
    assert_eq!(
        annotations.last_modified.as_deref(),
        Some("2026-09-18T00:00:00Z")
    );

    let McpContent::Image(image) = &out.content[1] else {
        panic!("block 1 should be an image, got {:?}", out.content[1]);
    };
    assert_eq!(image.data, "aW1n");
    assert_eq!(image.mime_type, "image/png");
    assert_eq!(image.meta.as_ref(), Some(&meta("image")));

    let McpContent::Audio(audio) = &out.content[2] else {
        panic!("block 2 should be audio, got {:?}", out.content[2]);
    };
    assert_eq!(audio.data, "YXVk");
    assert_eq!(audio.mime_type, "audio/wav");

    let McpContent::Resource(embedded) = &out.content[3] else {
        panic!("block 3 should be a resource, got {:?}", out.content[3]);
    };
    let McpResourceContents::Text {
        uri,
        mime_type,
        text,
        ..
    } = &embedded.resource
    else {
        panic!("block 3 should carry text contents, got {embedded:?}");
    };
    assert_eq!(uri, "file:///notes.txt");
    assert_eq!(mime_type.as_deref(), Some("text/plain"));
    assert_eq!(text, "notes");

    let McpContent::Resource(embedded) = &out.content[4] else {
        panic!("block 4 should be a resource, got {:?}", out.content[4]);
    };
    let McpResourceContents::Blob {
        uri,
        mime_type,
        blob,
        meta: blob_meta,
    } = &embedded.resource
    else {
        panic!("block 4 should carry blob contents, got {embedded:?}");
    };
    assert_eq!(uri, "file:///report.pdf");
    assert_eq!(mime_type.as_deref(), Some("application/pdf"));
    assert_eq!(blob, "YmxvYg==");
    assert_eq!(blob_meta.as_ref(), Some(&meta("blob-resource")));
    assert!(
        embedded.annotations.is_some(),
        "annotations on the embedding block are distinct from those on its contents"
    );

    let McpContent::ResourceLink(link) = &out.content[5] else {
        panic!(
            "block 5 should be a resource link, got {:?}",
            out.content[5]
        );
    };
    assert_eq!(link.uri, "file:///linked.rs");
    assert_eq!(link.name, "linked");
    assert_eq!(link.title.as_deref(), Some("Linked"));
    assert_eq!(link.mime_type.as_deref(), Some("text/x-rust"));
    assert_eq!(link.size, Some(4096));
    let icons = link.icons.as_ref().expect("resource link icons");
    assert_eq!(icons[0].src, "https://example.invalid/icon.png");
    assert_eq!(icons[0].theme.as_deref(), Some("dark"));
    assert_eq!(
        icons[0].sizes.as_deref(),
        Some(["48x48".to_string(), "96x96".to_string()].as_slice())
    );

    assert_eq!(
        out.structured_content,
        Some(json!({"rows": 3, "truncated": false}))
    );
    assert_eq!(out.meta.as_ref(), Some(&meta("result")));
}

#[test]
fn is_error_mirrors_the_servers_flag() {
    let mut reported = CallToolResult::error(vec![ContentBlock::text("nope")]);
    reported.meta = None;
    assert!(result_from_rmcp(reported).is_error);

    // The spec's default: an absent `isError` is not an error.
    let mut absent = CallToolResult::success(vec![ContentBlock::text("fine")]);
    absent.is_error = None;
    assert!(!result_from_rmcp(absent).is_error);
}

// ---------------------------------------------------------------------------
// Leg 2: proxied results out.
// ---------------------------------------------------------------------------

#[test]
fn result_round_trips_through_both_legs_unchanged() {
    let original = rmcp_mixed_result();
    let round = result_to_rmcp(result_from_rmcp(original.clone()));

    assert_eq!(
        serde_json::to_value(&round).unwrap(),
        serde_json::to_value(&original).unwrap(),
        "a result must survive client-in then proxy-out with its wire form intact"
    );
}

#[test]
fn result_to_rmcp_preserves_structured_content_and_meta() {
    let mut result = McpToolResult::from_content(vec![McpContent::text("body")]);
    result.structured_content = Some(json!({"n": 1}));
    result.meta = Some(meta("outbound"));

    let out = result_to_rmcp(result);

    assert_eq!(out.structured_content, Some(json!({"n": 1})));
    assert_eq!(out.meta.map(|m| m.0), Some(meta("outbound")));
}

#[test]
fn result_type_is_normalized_to_complete() {
    // `task` promises `tasks/*` follow-up methods the proxy does not
    // implement, so it is answered as a finished result rather than relayed.
    let mut task = rmcp_mixed_result();
    task.result_type = Some(ResultType::TASK);

    let out = result_to_rmcp(result_from_rmcp(task));

    assert_eq!(out.result_type, Some(ResultType::COMPLETE));
}

// ---------------------------------------------------------------------------
// Legs 3 and 4: tool descriptors, both directions.
// ---------------------------------------------------------------------------

#[test]
fn tool_from_rmcp_keeps_the_whole_descriptor() {
    let out = tool_from_rmcp(rmcp_rich_tool());

    assert_eq!(out.name, "read_file");
    assert_eq!(out.title.as_deref(), Some("Read File"));
    assert_eq!(out.description.as_deref(), Some("read one file"));
    assert_eq!(
        out.input_schema,
        json!({"type": "object", "properties": {"path": {"type": "string"}}})
    );
    assert_eq!(
        out.output_schema,
        Some(json!({"type": "object", "properties": {"text": {"type": "string"}}}))
    );
    let annotations = out.annotations.as_ref().expect("tool annotations");
    assert_eq!(annotations.title.as_deref(), Some("Read a file"));
    assert_eq!(annotations.read_only_hint, Some(true));
    assert_eq!(annotations.destructive_hint, Some(false));
    assert_eq!(annotations.idempotent_hint, Some(true));
    assert_eq!(annotations.open_world_hint, Some(false));
    assert_eq!(
        out.icons.as_ref().expect("icons")[0].theme.as_deref(),
        Some("dark")
    );
    assert_eq!(out.meta.as_ref(), Some(&meta("tool")));
}

#[test]
fn tool_round_trips_through_both_legs_unchanged() {
    let original = rmcp_rich_tool();
    let round = tool_to_rmcp(
        original.name.to_string(),
        &tool_from_rmcp(original.clone()),
        Arc::new(rich_input_schema_object()),
    );

    assert_eq!(
        serde_json::to_value(&round).unwrap(),
        serde_json::to_value(&original).unwrap(),
    );
}

#[test]
fn tool_to_rmcp_renames_to_the_public_name_only() {
    let tool = tool_from_rmcp(rmcp_rich_tool());

    let out = tool_to_rmcp(
        "fs__read_file".to_string(),
        &tool,
        Arc::new(rich_input_schema_object()),
    );

    assert_eq!(out.name, "fs__read_file");
    assert_eq!(out.title.as_deref(), Some("Read File"));
    assert_eq!(out.description.as_deref(), Some("read one file"));
}

#[test]
fn an_empty_description_becomes_none() {
    let mut tool = Tool::new("t", "", Arc::new(rich_input_schema_object()));
    tool.title = None;

    assert_eq!(tool_from_rmcp(tool).description, None);
}

// ---------------------------------------------------------------------------
// Block kinds this build does not model.
// ---------------------------------------------------------------------------

#[test]
fn an_unmodeled_block_is_captured_as_the_json_it_arrived_as() {
    // No current SDK variant reaches the wildcard, so the capture is driven
    // directly. What it must produce is the block's own wire form, which is
    // what makes the return leg able to rebuild it.
    let captured = opaque(&ContentBlock::text("hello"));

    let McpContent::Other { kind, raw } = &captured else {
        panic!("expected an opaque block, got {captured:?}");
    };
    assert_eq!(kind, "text");
    assert_eq!(raw, &json!({"type": "text", "text": "hello"}));
}

#[test]
fn an_unmodeled_block_rebuilds_itself_on_the_way_out() {
    let captured = opaque(&ContentBlock::text("hello"));

    let out = content_to_rmcp(captured);

    assert_eq!(
        serde_json::to_value(&out).unwrap(),
        serde_json::to_value(ContentBlock::text("hello")).unwrap()
    );
}

#[test]
fn a_block_the_sdk_cannot_rebuild_degrades_to_its_placeholder() {
    let block = McpContent::Other {
        kind: "video".to_string(),
        raw: json!({"type": "video", "data": "dmlk"}),
    };
    assert_eq!(
        McpToolResult::from_content(vec![block.clone()]).render_text(),
        "[unsupported content block: video]"
    );

    let out = content_to_rmcp(block);

    let ContentBlock::Text(text) = &out else {
        panic!("expected the placeholder as text, got {out:?}");
    };
    assert_eq!(text.text, "[unsupported content block: video]");
}

#[test]
fn unmodeled_resource_contents_degrade_the_same_way() {
    let contents = McpResourceContents::Other {
        raw: json!({"uri": "file:///x", "shape": "unknown"}),
    };
    assert_eq!(
        McpToolResult::from_content(vec![McpContent::Resource(McpEmbeddedResource::new(
            contents.clone()
        ))])
        .render_text(),
        "[unsupported resource contents]"
    );

    let ResourceContents::TextResourceContents { text, .. } = resource_contents_to_rmcp(contents)
    else {
        panic!("expected the placeholder as text contents");
    };
    assert_eq!(text, "[unsupported resource contents]");
}

// ---------------------------------------------------------------------------
// The derived view.
// ---------------------------------------------------------------------------

#[test]
fn render_text_is_unchanged_for_text_only_results() {
    assert_eq!(McpToolResult::ok("hello").render_text(), "hello");
    assert_eq!(McpToolResult::error("boom").render_text(), "boom");
    assert_eq!(
        McpToolResult::from_content(vec![McpContent::text("one"), McpContent::text("two")])
            .render_text(),
        "one\ntwo"
    );
    assert_eq!(McpToolResult::from_content(vec![]).render_text(), "");
}

#[test]
fn render_text_pins_the_mixed_case() {
    assert_eq!(
        result_from_rmcp(rmcp_mixed_result()).render_text(),
        MIXED_RENDERING
    );
}

#[test]
fn a_blob_without_a_mime_type_renders_as_octet_stream() {
    let result = McpToolResult::from_content(vec![McpContent::Resource(McpEmbeddedResource::new(
        McpResourceContents::blob("file:///x.bin", "YmxvYg=="),
    ))]);

    assert_eq!(
        result.render_text(),
        "[blob: application/octet-stream, 8 base64 bytes]"
    );
}

#[test]
fn structured_content_is_not_rendered() {
    let mut result = McpToolResult::ok("visible");
    result.structured_content = Some(json!({"hidden": true}));

    assert_eq!(result.render_text(), "visible");
}

// ---------------------------------------------------------------------------
// Decoration whose wire value the SDK cannot name.
// ---------------------------------------------------------------------------

#[test]
fn an_unnameable_icon_theme_is_dropped_rather_than_guessed() {
    let mut icon = McpIcon::new("https://example.invalid/i.png");
    icon.theme = Some("sepia".to_string());

    let out = icon_to_rmcp(&icon);

    assert_eq!(out.src, "https://example.invalid/i.png");
    assert_eq!(out.theme, None);
}
