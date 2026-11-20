//! The `mcp` presenter: an MCP service's tools reached by exact name in agent Python, its
//! results as content blocks, its listing relisted and announced, and a malformed tool left out.
//! Each server is a fake that answers with the JSON a test sets, placed in no container.

use std::sync::Arc;
use std::time::Duration;

use rmcp::model::CallToolResult;
use serde_json::{Value, json};

use super::bindings_tests::{output, start, stop, taken};
use super::event::{ServiceKind, Subscription};
use super::mcp_placement::fake::{Behavior, Toolbox};
use super::mcp_tests::{Fakes, of_service, outcome, placed_under, policy, position, sidecar};
use super::services_tests::{VAR, builder, of_kind, quick};
use super::{
    CallContext, McpServiceOutcome, McpServiceSpec, MethodDescription, Service, ServiceDescription,
    ServiceError, ServiceErrorCategory, ServiceFuture, ServicePolicy, Session,
};
use crate::mcp_content::{McpContent, McpToolResult, result_from_rmcp, result_to_wire};

/// A session whose MCP service `fs` -- a sidecar of its own, with `env` -- is the server
/// `toolbox` answers as, its calls under `calls`; its events subscribed to, `err` defined in its
/// primary, and the fakes that placed it.
async fn toolboxed(
    toolbox: &Arc<Toolbox>,
    env: &[(&str, &str)],
    calls: ServicePolicy,
) -> (Session, Subscription, Fakes) {
    let fakes = Fakes::default();
    fakes
        .of("mcp-fs")
        .script("fs", [Behavior::Toolbox(Arc::clone(toolbox))]);
    let builder = builder().mcp("fs", McpServiceSpec::sidecar(sidecar(env)));
    let starts = policy(Duration::from_secs(5), Duration::from_secs(10));
    let (session, events) = placed_under(&fakes, builder, calls, starts).await;
    (session, events, fakes)
}

/// Calls of a few seconds, at most.
fn calls() -> ServicePolicy {
    quick(Duration::from_secs(5))
}

/// A tool named `name`, taking what `schema` describes, as a server lists one.
fn tool(name: &str, schema: Value) -> Value {
    json!({"name": name, "description": format!("The {name} tool."), "inputSchema": schema})
}

/// `{text: string}`, which the fake echoes.
fn text() -> Value {
    json!({"type": "object", "properties": {"text": {"type": "string"}}})
}

/// Tools whose names are not identifiers, are keywords, or meet one attribute are each called by
/// their exact name, through `call` and through `tool`, and the server is asked for each under
/// that name; the attribute convenience resolves what it can and refuses what is ambiguous; an
/// unknown tool is refused with nothing sent. Each signature and docstring is the schema's --
/// `(**arguments)` for one taking anything -- and the descriptors are the server's own.
#[tokio::test]
async fn tools_are_reached_by_exact_name() {
    let path = json!({"type": "object", "properties": {"path": {"type": "string"}}});
    let mut read = tool(
        "read-file",
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Where it is."},
                "limit": {"type": "integer"},
            },
            "required": ["path"],
        }),
    );
    read["title"] = json!("Read File");
    read["annotations"] = json!({"readOnlyHint": true});
    read["_meta"] = json!({"tag": "r"});
    let toolbox = Toolbox::new(json!([
        read,
        tool("class", path.clone()),
        tool("read_file", path.clone()),
        tool("a__b", path),
        tool(
            "anything",
            json!({"type": "object", "additionalProperties": true})
        ),
    ]));
    let (session, _events, _fakes) = toolboxed(&toolbox, &[], calls()).await;
    let said = output(
        &session,
        r#"
import inspect
names = ['read-file', 'class', 'read_file', 'a__b', 'anything']
for name in names:
    fs.call(name, {'path': 'c'})
for name in names:
    fs.tool(name)(path='t')
fs.class_(path='k')
try:
    fs.read_file
except AttributeError as e:
    print('ambiguous:', "'read-file'" in str(e) and "'read_file'" in str(e))
try:
    fs.tool('missing')
except AttributeError as e:
    print('missing:', e)
print(inspect.signature(fs.method('read-file')), inspect.signature(fs.anything))
print('Where it is.' in fs.method('read-file').__doc__, 'The anything tool.' in fs.anything.__doc__)
d = fs.tools['read-file']
print(d['title'], d['annotations'], d['_meta'], sorted(fs.tools))
"#,
    )
    .await;
    assert_eq!(
        said,
        "ambiguous: True\n\
         missing: MCP service 'fs' has no tool 'missing': its `tools` lists those it has\n\
         (path: str, *, limit: int = ...) (**arguments)\n\
         True True\n\
         Read File {'readOnlyHint': True} {'tag': 'r'} \
         ['a__b', 'anything', 'class', 'read-file', 'read_file']\n"
    );
    let names = ["read-file", "class", "read_file", "a__b", "anything"];
    assert_eq!(
        toolbox.calls(),
        [&names[..], &names[..], &["class"]].concat()
    );
    let arguments = toolbox.arguments();
    assert_eq!(
        (&arguments[4], &arguments[9]),
        (&json!({"path": "c"}), &json!({"path": "t"})),
        "what `**arguments` takes reaches the server"
    );
    stop(session).await;
}

/// A result's blocks arrive in order and typed, its structured content as data, its `_meta`
/// kept, and its `text` the rendering the host makes of the same result; an image's bytes decode.
/// The call's events name the tool and the listing, and its end what the result held.
#[tokio::test]
async fn results_are_data() {
    let mixed = json!({
        "content": [
            {"type": "text", "text": "first"},
            {"type": "image", "data": "aGVsbG8=", "mimeType": "image/png"},
            {"type": "resource", "resource": {
                "uri": "file:///a.txt", "mimeType": "text/plain", "text": "embedded",
            }},
            {"type": "resource_link", "uri": "file:///b.txt", "name": "b"},
        ],
        "structuredContent": {"rows": [1, 2]},
        "_meta": {"m": 1},
    });
    let toolbox = Toolbox::new(json!([tool("mixed", json!({"type": "object"}))]));
    toolbox.answer("mixed", mixed.clone());
    let (session, mut events, _fakes) = toolboxed(&toolbox, &[], calls()).await;
    let said = output(
        &session,
        r#"
import json
r = fs.mixed()
print([type(b).__name__ for b in r.content], [b.kind for b in r.content])
print(r.structured, r.meta, r.is_error, r.content[1].data, r.content[2].uri, r.content[3].name)
print(json.dumps(r.text))
"#,
    )
    .await;
    let rendered =
        result_from_rmcp(serde_json::from_value::<CallToolResult>(mixed).expect("a tool result"))
            .render_text();
    assert_eq!(
        said,
        format!(
            "['McpText', 'McpImage', 'McpResource', 'McpResourceLink'] \
             ['text', 'image', 'resource', 'resource_link']\n\
             {{'rows': [1, 2]}} {{'m': 1}} False b'hello' file:///a.txt b\n\
             {}\n",
            serde_json::to_string(&rendered).expect("encodes")
        )
    );
    stop(session).await;
    let events = taken(&mut events);
    let received = &of_kind(&events, "service.call.received")[0];
    assert_eq!(
        (&received["kind"], &received["tool"], &received["listing"]),
        (&json!("mcp"), &json!("mixed"), &json!(1))
    );
    let ended = &of_kind(&events, "service.call.ended")[0];
    assert_eq!(ended["outcome"], "returned");
    assert_eq!(ended["state"], "response_received");
    assert_eq!(ended["is_error"], false);
    assert_eq!(ended["structured"], true);
    assert_eq!(
        ended["blocks"],
        json!({"image": 1, "resource": 1, "resource_link": 1, "text": 1})
    );
}

/// A service of the `mcp` kind whose one method, `video`, returns a block of a kind this build
/// does not model, beside a text block: what the MCP service hands over for such a block,
/// through the same wire form.
struct Unmodeled;

impl Service for Unmodeled {
    fn describe(&self) -> ServiceDescription {
        ServiceDescription::new("Blocks this build does not model.")
            .of_kind(ServiceKind::Mcp)
            .with_method(MethodDescription::new(
                "video",
                "",
                json!({"type": "object"}),
            ))
    }

    fn call(&self, _method: &str, _args: Value, _cx: CallContext) -> ServiceFuture<'_> {
        Box::pin(async {
            result_to_wire(unmodeled())
                .map_err(|e| ServiceError::new(ServiceErrorCategory::Failed, e.to_string()))
        })
    }
}

fn unmodeled() -> McpToolResult {
    McpToolResult::from_content(vec![
        McpContent::text("before"),
        McpContent::Other {
            kind: "video".to_string(),
            raw: json!({"type": "video", "data": "dmlk"}),
        },
    ])
}

/// rmcp 3.4.1 declares no block kind outrig does not model, so no server's result can carry
/// one; a block the host holds as `McpContent::Other` crosses as `McpOther`, its kind and its
/// JSON intact, and renders as the host renders it.
#[tokio::test]
async fn a_block_this_build_does_not_model_crosses_as_one() {
    let builder = builder().serve("vid", Arc::new(Unmodeled) as Arc<dyn Service>);
    let session = start(builder).await;
    let said = output(
        &session,
        "import json\nr = vid.video()\no = r.content[1]\n\
         print(type(o).__name__, o.kind, o.raw, json.dumps(r.text))",
    )
    .await;
    assert_eq!(
        said,
        format!(
            "McpOther video {{'type': 'video', 'data': 'dmlk'}} {}\n",
            serde_json::to_string(&unmodeled().render_text()).expect("encodes")
        )
    );
    stop(session).await;
}

/// A large structured result arrives whole, for agent code to filter, and what is recorded of
/// the execution is what it printed. Past the policy's bound the same call raises the service
/// mechanism's error, naming the limit, with the server's answer as its state, and the event
/// records that it returned.
#[tokio::test]
async fn a_large_result_is_filtered_in_python() {
    let rows: Vec<String> = (0..4096)
        .map(|i| format!("{i:04}{}", "x".repeat(1020)))
        .collect();
    let big = json!({"content": [], "structuredContent": {"rows": rows}});
    let toolbox = Toolbox::new(json!([tool("big", json!({"type": "object"}))]));
    toolbox.answer("big", big);
    let (session, mut events, _fakes) = toolboxed(&toolbox, &[], calls()).await;
    assert_eq!(
        output(
            &session,
            "r = fs.big()\nprint(len(r.structured['rows']), r.structured['rows'][7][:8])"
        )
        .await,
        "4096 0007xxxx\n"
    );
    stop(session).await;
    let events = taken(&mut events);
    let ended = &of_kind(&events, "service.call.ended")[0];
    assert!(
        ended["bytes"].as_u64().unwrap_or_default() > 4 << 20,
        "{ended}"
    );
    assert_eq!(ended["body"], Value::Null);

    let tight = ServicePolicy {
        result_max: 1 << 20,
        ..calls()
    };
    let (session, mut events, _fakes) = toolboxed(&toolbox, &[], tight).await;
    assert_eq!(
        output(
            &session,
            "try:\n    fs.big()\nexcept outrig.McpError as e:\n    \
             print(e.outcome, e.category, e.state, '1048576' in str(e), type(e).__name__)"
        )
        .await,
        "returned result_too_large response_received True McpError\n"
    );
    stop(session).await;
    let events = taken(&mut events);
    let ended = &of_kind(&events, "service.call.ended")[0];
    assert_eq!(
        (&ended["outcome"], &ended["reason"], &ended["state"]),
        (
            &json!("returned"),
            &json!("result_too_large"),
            &json!("response_received")
        )
    );
    assert!(
        ended["bytes"].as_u64().unwrap_or_default() > 1 << 20,
        "{ended}"
    );
}

/// A result the server marks as an error raises `ToolError`, the result in it, the server
/// asked once; the event says the call returned, and that the result was an error.
#[tokio::test]
async fn an_error_result_raises_tool_error() {
    let toolbox = Toolbox::new(json!([tool("fails", json!({"type": "object"}))]));
    toolbox.answer(
        "fails",
        json!({"content": [{"type": "text", "text": "no such file"}], "isError": true}),
    );
    let (session, mut events, _fakes) = toolboxed(&toolbox, &[], calls()).await;
    assert_eq!(
        output(
            &session,
            r#"
try:
    fs.fails()
except outrig.ToolError as e:
    print(e.outcome, e.state, e.result.is_error, [b.text for b in e.result.content])
    print(isinstance(e, outrig.McpError), isinstance(e, outrig.ServiceError), e.call is not None)
    print(e)
"#
        )
        .await,
        "returned response_received True ['no such file']\n\
         True True True\n\
         fs.fails (returned): the tool reported an error: no such file\n"
    );
    assert_eq!(toolbox.calls(), ["fails"]);
    stop(session).await;
    let events = taken(&mut events);
    let ended = &of_kind(&events, "service.call.ended")[0];
    assert_eq!(
        (&ended["outcome"], &ended["is_error"], &ended["state"]),
        (
            &json!("returned"),
            &json!(true),
            &json!("response_received")
        )
    );
}

/// A relist takes the server's new listing as the next generation and says what changed; a tool
/// already held keeps the name it was resolved by, so one whose tool was removed is refused
/// before the server is asked, and one whose tool remains still calls it. The server's own
/// announcement of a change marks the service stale and is evented, and changes nothing until
/// a relist.
#[tokio::test]
async fn relisting_never_retargets() {
    let toolbox = Toolbox::new(json!([
        tool("read-file", text()),
        tool("read_file", text()),
        tool("class", text()),
    ]));
    let (session, mut events, _fakes) = toolboxed(&toolbox, &[], calls()).await;
    assert_eq!(
        output(
            &session,
            "held_under = fs.tool('read_file')\nheld_dash = fs.tool('read-file')\n\
             print(fs.listing, fs.stale)"
        )
        .await,
        "1 False\n"
    );
    toolbox.list(json!([
        tool("read-file", text()),
        tool("class", text()),
        tool("new-tool", text()),
    ]));
    assert_eq!(
        output(
            &session,
            r#"
r = fs.relist()
print(r.added, r.removed, r.changed, r.undecoded, r.listing, fs.listing, fs.stale)
err(held_under, text='x')
print(held_dash(text='y').content[0].text, held_dash.listing, held_under.listing)
print(fs.read_file(text='z').content[0].text, fs.new_tool(text='n').content[0].text)
"#
        )
        .await,
        "('new-tool',) ('read_file',) () () 2 2 False\n\
         refused unknown_method unknown_method None\n\
         y 1 1\n\
         z n\n"
    );
    assert_eq!(toolbox.calls(), ["read-file", "read-file", "new-tool"]);

    toolbox.list_changed().await;
    let mut stale = String::new();
    for _ in 0..50 {
        stale = output(&session, "print(fs.stale, fs.listing)").await;
        if stale == "True 2\n" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(
        stale, "True 2\n",
        "a change announced marks the service stale"
    );
    assert_eq!(
        output(&session, "r = fs.relist()\nprint(r, fs.stale, fs.listing)").await,
        "McpRelisted(added=(), removed=(), changed=(), undecoded=(), listing=2) False 2\n"
    );
    stop(session).await;
    let events = taken(&mut events);
    let relisted = of_service(&events, "mcp.service.relisted", "fs");
    assert_eq!(
        relisted,
        [
            json!({"service": "fs", "agent": "primary", "listing": 2, "undecoded": []}),
            json!({"service": "fs", "agent": "primary", "listing": 2, "undecoded": []}),
        ]
    );
    let described = of_service(&events, "service.described", "fs");
    assert_eq!(described.len(), 1, "an unchanged listing describes nothing");
    assert_eq!(described[0]["generation"], 2);
    assert_eq!(
        described[0]["changes"],
        json!({"added": ["new-tool"], "removed": ["read_file"], "changed": []})
    );
    assert!(
        position(&events, "mcp.service.relisted", "fs")
            < position(&events, "service.described", "fs")
    );
    assert_eq!(
        of_service(&events, "mcp.service.list.changed", "fs"),
        [json!({"service": "fs"})]
    );
}

/// A change the server announces while its first listing is answered -- before the session
/// binds the service and listens for changes -- is heard once it does: the service is stale,
/// and the change evented once.
#[tokio::test]
async fn a_change_announced_before_bind_is_heard() {
    let toolbox = Toolbox::new(json!([tool("read-file", text())]));
    toolbox.announce_while_listing();
    let (session, mut events, _fakes) = toolboxed(&toolbox, &[], calls()).await;
    let mut stale = String::new();
    for _ in 0..50 {
        stale = output(&session, "print(fs.stale, fs.listing)").await;
        if stale == "True 1\n" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(
        stale, "True 1\n",
        "the change announced before bind is heard"
    );
    stop(session).await;
    assert_eq!(
        of_service(&taken(&mut events), "mcp.service.list.changed", "fs"),
        [json!({"service": "fs"})]
    );
}

/// A tool whose `inputSchema` is not an object costs the service that tool alone: it starts
/// ready with the others, and its outcome and `mcp.service.started` name the one left out and
/// why.
#[tokio::test]
async fn a_malformed_tool_costs_only_itself() {
    let toolbox = Toolbox::new(json!([
        tool("good1", json!({"type": "object"})),
        {"name": "bad", "inputSchema": []},
        tool("good2", json!({"type": "object"})),
    ]));
    let (session, mut events, _fakes) = toolboxed(&toolbox, &[], calls()).await;
    let McpServiceOutcome::Ready {
        tools, undecoded, ..
    } = outcome(&session, "fs")
    else {
        panic!("{:?}", outcome(&session, "fs"));
    };
    assert_eq!(tools, 2);
    assert_eq!(undecoded.len(), 1, "{undecoded:?}");
    assert_eq!(undecoded[0].name.as_deref(), Some("bad"));
    assert!(
        undecoded[0].reason.contains("invalid type"),
        "{undecoded:?}"
    );
    assert_eq!(
        output(&session, "print(sorted(fs.tools))").await,
        "['good1', 'good2']\n"
    );
    stop(session).await;
    let events = taken(&mut events);
    let started = &of_service(&events, "mcp.service.started", "fs")[0];
    assert_eq!(started["tools"], 2);
    assert_eq!(started["undecoded"][0]["name"], "bad");
    assert!(
        started["undecoded"][0]["reason"]
            .as_str()
            .is_some_and(|reason| reason.contains("invalid type")),
        "{started}"
    );
}

/// A credential the server's environment is given reaches the server, and nothing agent code
/// or an owner reads: not the object's `repr` or `help`, not any exception, not any event.
#[tokio::test]
async fn nothing_injected_shows() {
    let toolbox = Toolbox::new(json!([
        tool("echo", text()),
        tool("fails", json!({"type": "object"})),
    ]));
    toolbox.answer(
        "fails",
        json!({"content": [{"type": "text", "text": "it failed"}], "isError": true}),
    );
    let reference = format!("${{{VAR}}}");
    let (session, mut events, fakes) =
        toolboxed(&toolbox, &[("TOKEN", reference.as_str())], calls()).await;
    assert!(
        fakes
            .of("mcp-fs")
            .envs()
            .iter()
            .any(|(_, env)| env.get("TOKEN").map(String::as_str) == Some("sk-test")),
        "the server was given the value"
    );
    let said = output(
        &session,
        r#"
said = [repr(fs)]
for f in (lambda: fs.tool('missing'), lambda: fs.call('missing', {}), fs.fails,
          lambda: fs.echo(text=1), lambda: fs.call('echo', {'text': 1}, timeout=10**9)):
    try:
        f()
    except Exception as e:
        said.append(f'{type(e).__name__}: {e}')
print('\n'.join(said))
help(fs)
"#,
    )
    .await;
    assert!(
        said.starts_with("<outrig MCP service 'fs' (sidecar):"),
        "{said}"
    );
    assert!(said.contains("ToolError: fs.fails (returned)"), "{said}");
    assert!(said.contains("McpError: fs.missing (refused"), "{said}");
    assert!(!said.contains("sk-test"), "{said}");
    stop(session).await;
    let events = taken(&mut events);
    let all = serde_json::to_string(
        &events
            .iter()
            .map(|event| serde_json::to_value(&event.payload).expect("encodes"))
            .collect::<Vec<_>>(),
    )
    .expect("encodes");
    assert!(!all.contains("sk-test"), "{all}");
}
