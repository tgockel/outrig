# Present a Rust service to agent Python as an object

## Context

Phase 0003 hosts Python objects only. A binding's factory runs in a host process of the embedded
CPython, and agent Python reaches the object through RPyC frames on the interpreter pipe
(`plan/phase/0003-python/hosted-objects.md`). A service written in Rust has no Python object to
host. The phase 0003 design brief (now in `plan/phase/0003-python/embedding.md`) listed ways to give
it one and chose none, and the maintainer deferred all of them past 0.3: a Rust service ships a
pure-Python client, and a binding hosts that client like any library. That costs a second client per
service and one more process between the agent and the service.

This is also how an MCP server would become a Python object. The Python MCP SDK requires
`pydantic-core`, which is compiled and cannot load in the embedded CPython on either side of the
boundary (`plan/phase/0003-python/mcp-wrappers.md`), while OutRig's Rust MCP client
(`crates/outrig/src/mcp.rs`, `mcp_proxy.rs`) already works.

What would decide whether this adapter is worth building is `0003-17`'s service-shaped
measurement: RPC count, process memory and tail latency for representative list, update and
question operations, made through a hosted Python client across concurrent sessions. If a Rust
service behind such a client meets its numbers, the second client and the extra process are a
cost and not a reason to build this. If per-call overhead or the memory of blocked calls
dominates, this adapter is the step after the ticket pattern
(`plan/phase/0003-python/potential/ticket-based-service-waits.md`). The protocol a Rust service
could answer directly, with no hosted client and no RPyC, is
`plan/phase/0003-python/potential/custom-hosted-object-protocol.md`.

## Shape

An embedder implements a trait and binds an instance by name. The spellings are illustrative, and
the real trait needs a boxed future in place of `async fn` to be usable as `dyn`:

```rust
trait Service: Send + Sync {
    fn describe(&self) -> ServiceDescription;
    async fn call(&self, method: &str, args: serde_json::Value, cx: CallContext)
        -> Result<serde_json::Value, ServiceError>;
}
builder.bind("task", Arc::new(TaskService::new(..)));
```

- `describe` lists methods with argument and result schemas and their documentation. Every
  kernel gets an object generated from it, with a signature and docstring per method, so `help()`
  answers from the schema, and `runtime.bindings` lists it beside hosted objects.
- A call carries its arguments by value, as JSON, in a message kind of its own on the interpreter
  pipe. Rust checks them against the schema and calls `call`. No binding process or RPyC is used.
- The boundary is a hosted object's: `[[policy.rules]]` (`0003-23`) match the binding, the
  described type, the method and the operation; `evaluate` sends the call to the evaluator
  (`0003-24`); each call is evented as in `0003-22`; admission and the shutdown report cover it
  (`plan/phase/0003-python/lifecycle.md`).
- Calls are awaitable with the same spelling as a hosted object's, so the two read alike
  (`mcp-wrappers.md`). `CallContext` carries the call id, the binding and a cancellation signal,
  so unlike an RPyC call this one can be told to stop.

Open: whether a result may contain further service objects or only values; whether `describe` is
fixed at start or may change, as an MCP server's tool list can; the error kinds `mcp-wrappers.md`
lists; how an MCP tool's schema and name become a Python method.

## Acceptance

- Agent code calls a bound service's method and gets Python data back. Arguments outside the
  schema fail before `call` runs.
- A `deny` rule on a method never invokes `call`; an `escalate` rule waits for `/approve`.
- An MCP server's tools are methods of a bound object, served by OutRig's Rust client.
- Shutdown signals cancellation to running calls and reports each one's outcome.
