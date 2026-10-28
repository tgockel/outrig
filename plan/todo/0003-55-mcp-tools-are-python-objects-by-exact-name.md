# 0003-55 -- MCP tools are Python objects by exact name

## Context

After `0003-54` each ready MCP server is a service bound into `0003-44`'s table, and agent Python
already reaches it through the generic object `services.md` gives every service: a method per tool
whose exact name is an identifier, `call(name, arguments)` and `method(name)` for every tool by
exact name, the attribute convenience with its ambiguity error, `help()` from the schema, the
listing in `runtime.bindings`, `outrig.bindings[name]`, the schema check in Rust before dispatch,
the result bound and the events. A tool's result is the `McpToolResult` as JSON, which the generic
object hands over as dicts and lists. What a server has and a Rust service does not is still
missing: a result made of ordered content blocks with a structured payload, metadata and an error
flag; a listing that moves; descriptors with a title, annotations, icons and `_meta`. The content
types keep a result whole in Rust: `McpToolResult` with ordered blocks, `structured_content`,
`meta` and `is_error` (`crates/outrig/src/mcp_content.rs:129`), `McpContent::Other` for a kind this
build does not model (`:205`), `render_text` as the lossy view (`:175`), and `McpTool` as the
descriptor (`:46`). `plan/next/tool-descriptor-is-shaped-twice.md` records that `ToolHandle`
(`outrig_.rs:979`) copies `McpTool`'s fields by hand; the description is built from `McpTool` and
adds no third copy, and `ToolHandle` is untouched (A-08).

`plan/next/one-bad-tool-schema-aborts-the-tool-list.md` corrected its own premise: rmcp's typed
decode of `tools/list` fails the whole page for a non-object `inputSchema`, so tolerance "needs a
lenient decode of `tools/list`". This task absorbs the entry; the seam the design names is rmcp's
`ServerResult::CustomResult(Value)`, the union's last untagged variant, which the task confirms
first (`mcp-presenter.md`, "Unverified"). `plan/next/rmcp-list-result-spec-gaps.md` bears on scope:
whoever adds resources or prompts "inherits the cache-metadata gap", and this task adds neither.
The interpreter program moved off the `-c` argument in `0003-39`, so the code this task adds to the
interpreter competes with no cap.

The requirement is OR-MCP2 of `02-channels-and-mcp.md`: exact tool names, documentation, schemas
and metadata exposed; deterministic attribute conveniences beside an exact-name lookup and call that
handle hyphens, keywords, collisions and unusual schemas; ordered content, structured results, the
error indication and metadata preserved, text a derived view, unknown kinds identifiable; a
relisting that never silently retargets a resolved tool; a host result bound distinct from the
model's observation bound, failing clearly. `mcp-wrappers.md` ("What the object has to be") decided
the shape -- synchronous, results as data, distinguishable errors, names with an escape,
documentation from the schema -- and `services.md` builds most of it for every service; this task
builds what is MCP's, as `mcp-presenter.md` ("The Python surface", "Events") lays it out. The
validation's attachment note: the content types "already keep ordered blocks, structured content,
`_meta`, `isError` and unknown block kinds ... which is what OR-MCP2 asks for". This task records
in its `## Decisions` the end of the waiting `mcp-wrappers.md` describes, for presentation,
quoting the sentence the page quotes.

## Goal

Agent Python reaches each MCP service's tools through the generic service object and gets a call's
result as data -- blocks, structured content, metadata and the error flag -- through a presenter
kind OutRig ships, with the descriptors a server advertises, a relisting that reports changes
without moving a resolved tool, and a listing that survives one malformed descriptor.

## Deliverables

- **The description from `tools/list`.** `McpService::describe()` (`0003-54`) is the current
  listing as a `ServiceDescription`: each tool's exact `name`, `description`, `inputSchema` as
  `params`, `outputSchema` as `result`, and `title`, `annotations`, `icons` and `_meta` in
  `extras`, built from `McpTool` (`mcp_content.rs:46`); `presenter: "mcp"`; the generation is the
  listing's number. Names are kept exact: `read-file` is the method `read-file`, reached by
  `call("read-file", ...)` and `method("read-file")` from the generic object and by `read_file`
  through its convenience, as `services.md` fixes for every service; nothing here maps names.
- **The `mcp` presenter kind**, registered in `0003-44`'s registry: a subclass of the generic
  class, instantiated per kernel as every service's object is, adding `tools` -- the descriptors as
  the protocol has them, from `extras` and the schemas -- `tool(name)`, the exact-name lookup
  returning a `Tool` (the generic `method(name)` with its descriptor and `listing` beside it,
  callable by the name it was resolved with), `listing`, `stale`, `relist()` and `reconnect()`
  (`0003-56` fills the second). `repr` and `help()` add the placement kind and nothing of `env`,
  `headers` or a URL's userinfo.
- **Results as data.** The presenter wraps a call's JSON into a result class holding `content` --
  `Text`, `Image`, `Audio`, `Resource`, `ResourceLink` and `Other(kind, raw)` blocks, each with its
  annotations and `meta` -- `structured` as Python data or `None`, `meta`, `is_error`, and `text`,
  the derived view that renders as `render_text` does, documented lossy. A binary block gives its
  MIME type and its bytes, decoded from the wire's base64 on first use (fork 2). The wire carries
  the JSON `McpService::call` returns; the interpreter rebuilds the blocks.
- **The error flag** (fork 1): a result with `is_error` raises `ToolError`, a subclass of the
  `McpError` `0003-56` defines under `outrig.ServiceError`, carrying the result; the event records
  `outcome: returned` with `is_error: true`.
- **Relisting.** `relist()` runs `tools/list` again through the host -- the MCP table's request,
  answered like a call -- replaces the description under the next generation, which `0003-44`'s
  `service.described` line carries to every kernel, and returns `added`, `removed` and `changed`
  by exact name. A held `Tool` keeps its name and descriptor; one whose name was removed is
  `refused` before dispatch as unknown method by the service table; attribute names are recomputed
  by the generic class. A `tools/list_changed` notification sets `stale` and is an event; nothing
  relists on its own.
- **A tolerant listing.** Each page of `tools/list` is read through `send_request_with_option`,
  and one the typed decode refused is decoded tool by tool from `CustomResult`'s value, keeping
  what decodes and recording the rest by name and reason in the outcome and the events. Confirmed
  with a fake server before anything is built on it; if the seam does not hold, the task records
  the limit in `## Decisions` and the listing fails as today. A tool whose `inputSchema` decodes
  but does not compile in the validator is dispatched unchecked and never dropped (`services.md`,
  "The table").
- **The result bound's key** (fork 3): a top-level config key for the service policy's
  `result_max`, so `run-new` can set it; `tool-result-max` (`config/mod.rs:41`) is untouched and its
  doc says which bound it is.
- **The orientation.** The services paragraph `0003-44` adds to the primary's preamble
  (`crates/outrig/src/agent/orientation.rs:66`, `rest` at `:111`) gains, for an MCP service, that a
  result is blocks and structured data to filter in Python, and that a call's outcome may be
  uncertain and is never repeated by OutRig. A dict-constructed agent's paragraph is
  `facilities.md`'s (`0003-42`, `0003-47`).
- **Events**, integration audit, in `doc/reference/events.md`: the MCP fields on
  `service.call.received` and `service.call.ended` -- `tool`, `listing`, `state`, `failure`,
  `is_error`, `blocks`, `structured` -- with `state` filled as `response_received` for a result and
  as `0003-56` decides for a failure; `mcp.service.relisted` and `mcp.service.list_changed`.
- **A-08.** `McpToolResult`, `McpTool` and `ToolHandle` are read, never changed; the result class
  is the interpreter's.
- **Docs.** `doc/concepts/mcp-servers.md` gains the Python surface; the Python runtime concept page
  `0003-38` writes gains the MCP service beside the Rust service; `events.md` as above;
  `crates/outrig/CHANGELOG.md`.
- **`## Decisions` records** the quoted sentence and the end of the waiting, the forks, the
  `CustomResult` finding, and the spellings.

## Acceptance

Mock-driven: a fake MCP server on rmcp's server side over an in-process transport, behind a fake
placement from `0003-54`, in the host-session test mode; the Python side driven as the
interpreter's tests drive agent code.

- **Exact names.** The fake lists `read-file`, `class`, `read_file`, `a__b` and a tool whose
  `inputSchema` is `{"type": "object", "additionalProperties": true}` with no properties; each is
  callable by `svc.call(name, ...)` and by `svc.tool(name)(...)`, and the fake records each call
  under that exact name; `svc.class_` resolves `class`; `svc.read_file` raises the ambiguity error
  naming `read-file` and `read_file`; `svc.tool("missing")` raises unknown method and the fake sees
  nothing. These hold through the generic object; the test asserts them here because the names
  are a server's.
- **`help()` from the schema.** A tool with properties `path: string` and `limit: integer`,
  `required: ["path"]`, renders a signature with `path` positional-or-keyword and `limit` defaulted;
  the unusual-schema tool renders `(**arguments)`; both carry their descriptions; neither is
  dropped, and both are callable. `svc.tools` carries each tool's `title`, `annotations` and
  `_meta` as the fake sent them.
- **Results are data.** A result with a text block, an image block, an embedded text resource, a
  resource link and `structuredContent` arrives with the blocks in order and typed, `structured`
  as a dict, `meta` kept, `is_error` false, and `text` equal to `render_text`'s rendering; the
  image's bytes decode. A block of a kind rmcp knows and outrig does not, if the locked rmcp
  declares one, arrives as `Other(kind, raw)`; else the Rust side's `McpContent::Other` is checked
  to cross as one.
- **A large result is filtered in Python.** A 4 MiB `structuredContent` arrives whole; agent code
  filters it and prints a slice; the execution's recorded output is the bounded observation, not
  the result. With the policy's `result_max` at 1 MiB the same call raises the service mechanism's
  result-too-large error naming the limit and the size, with `state == "response_received"`, and
  `service.call.ended` records `bytes` and `outcome: returned`.
- **The error flag.** A result with `isError` true raises `ToolError` whose `result` holds the
  blocks and whose `state` is `response_received`; the fake saw one call; the event says
  `returned` with `is_error: true`.
- **Relisting.** The fake adds `new-tool` and removes `read_file`; `relist()` returns them in
  `added` and `removed`, `listing` is 2, `mcp.service.relisted` then `service.described` with
  generation 2 carry the changes; the `Tool` held for `read_file` raises unknown method before
  dispatch and the fake sees no call; a `Tool` held for `read-file` still calls `read-file`. A
  `tools/list_changed` notification sets `stale`, records `mcp.service.list_changed`, and changes
  the listing in nothing.
- **A tolerant listing.** The fake lists one tool with `"inputSchema": []` beside two sound ones;
  the service is `Ready` with two tools, and the outcome and `mcp.service.started` name the tool
  that did not decode and why. If the seam does not hold, this line is replaced by the recorded
  limit.
- **Nothing injected shows.** With a `${VAR}` in the fake's sidecar `env`, `repr(svc)`,
  `help(svc)`, every exception's text and every event's serialized form lack the value.
- **Attribution.** A call from a child's kernel records `agent` as the child and `execid` as its
  execution, with `kind: "mcp"` and `tool` on the event; the same call from a Rust test service
  records `kind: "rust"` with the same correlation fields (OR-A3-5).
- **The orientation** names each ready MCP service and says results are blocks and structured
  data, checked by reading the `model.instructions` event.
- `crates/outrig/public-api.txt` regenerated; `cargo test --workspace`,
  `cargo clippy --all-targets`, `cargo fmt --check` pass; the docs and `events.md` say what the
  code does, checked by reading.

## Design forks

1. **A result with `isError` -- Recommended: raise `ToolError` carrying the result.** A flag a
   model forgets to check turns a failure into a success; an exception cannot be missed, and
   `.result` keeps the content. The alternative returns the result and leaves the flag to the
   caller, which the requirement lists as its open choice.
2. **Binary blocks -- Recommended: bytes, decoded on first use**, with the base64 kept beside.
   The alternative hands over the base64 string, which every caller decodes by hand.
3. **A config key for the result bound -- Recommended: a top-level key** so `run-new` can set it,
   named for the service policy it sets and documented beside `mcp-call-timeout-secs`. The
   alternative leaves the CLI at the default.

The local argument check is not a fork here: `services.md`, "The table", decides it for every
service, and a tool whose schema does not compile is dispatched unchecked.

## Dependencies

- **Hard: `0003-44`.** The generic object, the presenter-kind registry, the result bound, the
  `service.described` line and the call events this task adds fields to.
- **Hard: `0003-54`.** `McpService`, the MCP table and its clients, the placements.
- **Hard: `0003-39`.** The interpreter program as a file; this task adds interpreter code.
- **Soft: `0003-42`.** `facilities.md`'s per-agent view; with it landed, the orientation paragraph
  follows the agent's dict.

## See also

- `plan/phase/0003-python/mcp-presenter.md` -- "The shape", "The Python surface", "Events".
- `plan/phase/0003-python/services.md` -- "The generated object", "The table".
- `plan/phase/0003-python/mcp-wrappers.md` -- "What the object has to be, whichever way it is
  built".
- `plan/phase/0003-python/discovery.md` -- "What goes in the preamble", the bounded `help()`.
- `plan/next/one-bad-tool-schema-aborts-the-tool-list.md` -- absorbed here;
  `plan/next/tool-descriptor-is-shaped-twice.md`, `plan/next/rmcp-list-result-spec-gaps.md`.
- `crates/outrig/src/mcp_content.rs`; `crates/outrig/src/agent/orientation.rs`;
  `crates/outrig/src/python/interpreter.py`.
