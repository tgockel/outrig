# Python execution and agent communication in OutRig

## Overview

This design gives each OutRig agent a persistent Python execution environment inside its existing container. The LLM submits Python code, the application executes it, and bounded execution results return to the LLM. Python variables persist independently of model conversation history.

Agents communicate through named channels with message contracts described by serializable Python dataclasses. Each agent has its own interpreter and a user channel. A parent can create heterogeneous subagents and connect each child through multiple channels with different contracts.

Python code can await ordinary asynchronous operations. A runtime helper, `runtime.wait()`, additionally allows pending channel input to interrupt the wait without cancelling the operation. Background results and exceptions remain in their Python objects until code awaits them; they do not independently trigger model invocations.

The proposed deployment uses statically linked CPython payloads for Linux x86-64 and AArch64, supplied with OutRig and mounted read-only into the container. Python executes neither on the host nor in a separate sidecar.

This is an architecture specification, not a description of an implemented OutRig subsystem. API names illustrate the proposed interface. Platform validation is incomplete; the final section distinguishes tested behavior from remaining acceptance tests.

## 1. Goals and constraints

### Goals

The execution environment must support:

- Ordinary Python syntax and standard-library idioms, including `dataclasses`, `pathlib`, `open()`, and `asyncio`.
- Persistent objects and dynamically composed message types.
- Reflection through help text, signatures, annotations, and variable inspection.
- Exceptions as the mechanism for execution and operation failures.
- Asynchronous interaction with user input, other agents, network operations, and services.
- Bounded observations so execution cannot accidentally exhaust model context.
- Independent Python state for each agent.

### OutRig constraints

OutRig's security boundary is the Podman container. The operator configures the environment, mounts, network policy, and available runtime services. Arbitrary execution inside that environment is permitted; arbitrary execution on the host is not.

The Python subsystem must not require Python, a shell, a compiler, or user-management tools in the selected image. Existing OutRig bootstrap requirements still apply: notably, a `sleep` accepting `infinity` and the documented identity-bootstrap prerequisites, including existing passwd/group files and writable `/etc`.

OutRig does not download or install interpreter dependencies at session runtime. Executables supplied to a container must be Linux ELF files matching its architecture. An unavailable runtime must produce a precise diagnostic rather than silently substitute a different interpreter.

The agent cannot ask the host to create additional containers, add mounts, change images, or broaden network permissions. Creating a subagent adds execution within the environment already granted by the operator.

### Scope exclusions

The orchestration interpreter does not need to load third-party native extensions or support `pip`. A separate Python installation supplied by the image can execute applications that require such packages.

Detailed credential management, user-interface layout, and representing MCP servers as Python objects are separate work. The architecture must leave those integrations possible.

## 2. Terminology

### Agent

An agent consists of its application-managed model interaction and its associated Python VM. Their lifetimes are coupled. An agent does not continue operating after losing its VM.

A sleeping agent still has its VM, globals, and channels. Sleeping does not require a foreground Python execution suspended indefinitely on input.

### Turn

A **turn** is the continuous application–LLM interaction managed by OutRig's `RigAgent::run_turn`. It can include multiple model requests, text messages, tool calls, and execution results.

A user prompt can initiate a turn, but another event can initiate one too. A turn is not synonymous with one user message, one model response, or one Python execution.

The application controls the turn's lifetime. There is no LLM-callable `finish()` operation that terminates the turn.

### Model invocation

A **model invocation** is one request to an endpoint such as `/chat/completions` or `/responses` and its generated response, potentially streamed.

Generation stopping means that invocation has stopped producing output. It does not establish that the user's task, Python work, or OutRig turn is finished.

### Python execution

A **Python execution** is one submitted piece of source evaluated in the persistent session. It can complete synchronously, suspend on `await`, or exit with an exception.

Only one foreground Python execution runs at a time for an agent. Background asyncio tasks can remain active across foreground executions.

### Channel

A **channel** connects two endpoints and carries typed messages. Each endpoint has a name local to its agent. Its directional message contracts are established by the channel's creator.

## 3. Process and state organization

Each agent has a separate CPython process inside the session's primary container. Each process owns its:

- Global variables and module namespace.
- Asyncio event loop.
- Foreground execution.
- Background tasks and futures.
- Channel endpoints and receive queues.

Parents and children do not share a REPL namespace or Python object references. Communication crosses process boundaries as serialized messages.

Separate processes simplify state separation and lifecycle management. They are not a security boundary between agents sharing a container, user identity, and filesystem. Agents can affect shared resources to the extent the environment permits.

Python file, subprocess, and network operations use the primary container's filesystem and network namespace. No host-side Python evaluation is involved.

Python state survives foreground execution completion and OutRig turn completion. Model conversation history can be discarded between turns without losing Python state. Later model invocations discover retained variables through an inventory and inspect selected values through execution.

The persistent global namespace belongs to a module registered in `sys.modules`. This preserves module-dependent Python behavior, including dataclass introspection, rather than treating globals as an unrelated dictionary.

## 4. The application–LLM interface

### Model input

The application supplies the model with instructions describing the execution interface, available runtime objects, and how to discover their APIs. It also supplies bounded observations such as:

- Local channel names and their message contracts.
- Global variable names and type descriptions.
- Notifications of pending input.
- Output or an exception from the preceding execution.

For example:

```text
Python execution supports top-level await.
Global variables persist between executions.
Use help() to inspect runtime interfaces.

Channels:
    user
    work
    control

Globals:
    agent       AgentRuntime
    downloads   Future

Pending input:
    user
```

A global inventory does not automatically display every value. Message bodies remain queued until Python receives them. Notification of input therefore need not copy the message body directly into model context.

### Model output

The LLM can emit text messages and request Python execution. The proposed harness operation is:

```text
python_execute(source=...)
```

It carries Python source to the agent's interpreter and does not require an MCP server.

The application executes the code and returns bounded output or an exception through the execution-result exchange. It does not infer what arbitrary Python data means or automatically generate a domain-specific report.

For example, if code stores ETF histories without printing or otherwise emitting a summary, the result indicates completion without output. The application does not inspect the histories and invent a financial summary.

### Idle agents and incoming messages

When an agent has no foreground Python execution, input arrival causes the application to notify the LLM. The LLM can submit Python to receive and process the message.

An idle agent does not need to keep a Python execution awaiting input. If a model invocation is already in progress, additional notifications are queued rather than starting concurrent foreground interactions for the same agent.

During active Python execution, messages are queued. They do not inject exceptions into arbitrary code. Responsiveness during an operation is provided by `runtime.wait()` when the generated code chooses to use it.

### User output

The application displays LLM text intended for the user. Python can also send messages through the user channel.

Sending output is not a lifecycle operation: it does not cancel work, destroy the VM, or end the OutRig turn. The application decides whether to continue its interaction loop or sleep awaiting input.

### Model-generation limits

Code-generating model invocations need an explicit output-token ceiling, either configured or obtained from verified model-specific information. An empty or reasoning-only response executes nothing. A truncated code response must not be executed as if complete.

The application reports unusable responses and applies a bounded retry policy. It does not interpret hidden reasoning as executable source or continue retrying indefinitely.

## 5. Tracking execution completion

The runtime tracks completion of the entire submitted execution. It does not recognize a particular `await` by inspecting source or infer completion from printed output.

For source containing top-level await, the asynchronous path is conceptually:

```python
code = compile(
    submitted_source,
    "<agent>",
    "exec",
    flags=ast.PyCF_ALLOW_TOP_LEVEL_AWAIT,
)

coroutine = eval(code, session_globals, session_globals)
task = asyncio.create_task(coroutine)
task.add_done_callback(report_execution_finished)
```

Source without top-level await can complete synchronously and is handled separately.

When the tracked execution returns or exits with an uncaught exception, the runtime reports the execution identifier, status, and bounded output to the application. The application returns this result to the LLM within its interaction loop.

For an execution containing `await asyncio.gather(...)`:

1. The foreground coroutine suspends at the await.
2. Asyncio runs the outstanding operations.
3. Gather completes or raises.
4. The foreground coroutine resumes and executes any remaining statements.
5. Return or an uncaught exception ends the execution.
6. The runtime sends an explicit completion report.

Background tasks that the execution did not await can continue after that report.

## 6. Asynchronous waiting and message notification

### Ordinary awaits

Generated code can use asyncio without the runtime helper:

```python
downloads = asyncio.gather(*download_tasks)
histories = await downloads
```

Incoming messages remain queued during this await. If code neither receives messages nor finishes, the messages remain pending unless the operator separately stops execution.

Network operations use container sockets; socket readiness resumes the relevant asyncio operations. Timers use the event loop. External runtime services deliver responses through the application connection and resolve corresponding futures. These sources all support Python await semantics without requiring a host-side generic network proxy.

### The `runtime.wait()` contract

The helper provides a common interruptible wait:

```python
histories = await runtime.wait(downloads, "downloads")
```

Its interface is:

```text
async runtime.wait(operation, label=None)

Await an operation while checking incoming channels.

Return the operation's result when it completes.
Raise the operation's exception if it fails.

If input is pending, raise MessageAvailable and identify
its channel by the agent's local channel name.

Do not consume the message.
Do not cancel the operation because input became available.

Use the optional label for status and exception reporting.
```

The label is descriptive text. It neither creates a variable nor needs to match a variable name.

The helper checks all incoming channels. It does not initially require a channel-selection argument. A notification can identify several ready channels together, within output limits.

Pending input takes priority when both input and operation completion are ready. The operation retains its result or exception for a later await.

### Effect of incoming input

If input arrives during the wait, the helper raises within the submitted execution. This is not arbitrary exception injection by the host.

An uncaught exception ends the execution and produces an observation such as:

```text
MessageAvailable: input pending on "control" while waiting for "downloads".
The operation was not cancelled.
```

The application invokes the LLM with that observation. Subsequent Python can receive the input:

```python
message = await runtime.channels["control"].receive()
print(message)
```

After interpreting it, the model can submit:

```python
histories = await runtime.wait(downloads, "downloads")
```

This is a new execution awaiting the retained operation. The earlier coroutine does not resume after its stack has unwound.

Unread input causes a subsequent `runtime.wait()` to raise again immediately. Receiving the message removes it from the queue. Generated code can also catch `MessageAvailable` and handle the input without ending execution or requesting another model invocation.

### Status events

The helper can emit structured status events:

```text
Waiting: downloads
Completed: downloads
Failed: downloads
Input available on control while waiting: downloads
```

The application can display waiting status without repeatedly calling the LLM. Any status included in model context is bounded.

The helper reports generic execution facts. It cannot determine which ETFs succeeded, whether their date ranges are comparable, or which fields are relevant to the user. Generated code must produce that information explicitly.

## 7. Background results and failures

Operations remain accessible through ordinary Python references:

```python
downloads = asyncio.gather(*download_tasks)
```

If `runtime.wait(downloads)` is interrupted by input, `downloads` remains available in the persistent namespace. A later model invocation can discover it in the variable inventory.

No separate model-facing operation registry is required. Code retains the references it wants to use later.

A background operation's completion or failure does not independently trigger a model invocation. Its result or exception remains stored in the operation. A later await retrieves it:

```python
histories = await runtime.wait(downloads)
```

If the operation failed, this raises its stored exception, subject to pending channel notifications taking priority.

The runtime does not infer whether a result still matters. If generated code suppresses an exception or never retrieves the operation's result, that is the code's behavior. Asyncio's incidental diagnostics are not repurposed as an automatic model scheduling mechanism; captured diagnostics remain subject to output limits.

Channel-failure notifications are separate from background-operation failures and follow the channel semantics below.

## 8. Channel creation and ownership

### Multiple channels per relationship

An agent holds a collection of named endpoints. A parent and child can have multiple channels with different purposes and contracts:

| Channel | Parent sends | Child sends |
|---|---|---|
| `work` | `AnalyzeETF` | `ETFResult` |
| `control` | `ChangeScope` | `ScopeAcknowledged` |
| `progress` | No application messages | `ProgressUpdate` |

The agent discovers these endpoints through reflection. Notifications use its local names.

An inbox, if used internally to aggregate notifications, does not have one global accepted-message schema. Contracts belong to individual channel directions.

### Creator responsibility

The creator specifies the subagent's initial configuration and the communication contracts. It constructs channels and supplies the appropriate endpoints to the participants.

The child discovers the endpoints and message types supplied to it. It does not independently define what its creator expects by registering an agent-wide receive type.

Subagent creation and channel creation are distinct operations conceptually. An eventual convenience API can perform both. Exact constructor signatures are not specified here.

The initial communication API supports parent–child relationships, including additional parent–child channels after construction. Direct sibling or other agent-to-agent connections and endpoint transfer are deferred. Multiple endpoints per agent allow those relationships to be added later without changing the channel model.

### User channel

Every agent, including every subagent, has a named `user` channel supplied by the application.

The user can address a subagent directly without its parent relaying the message. The application preserves source and destination agent identity for routing and attribution. Conversation selection and presentation are UI concerns, not channel semantics.

The application defines the user channel's message types. For example, an illustrative outbound type could be used as follows:

```python
await runtime.channels["user"].send(
    UserText("The downloads are complete.")
)
```

The concrete user-message classes remain an API specification detail. Delivery does not terminate a turn.

## 9. Typed messages

Channel creators describe message bodies with simple serializable dataclasses:

```python
from dataclasses import dataclass

@dataclass
class AnalyzeETF:
    symbol: str
    start_date: str
    end_date: str

@dataclass
class ETFResult:
    symbol: str
    annualized_return: float
```

A direction can accept a declared union of message types. Different channels can use unrelated contracts, allowing a parent to create heterogeneous subagents.

The initial data subset comprises strings, booleans, integers, finite floating-point numbers, `None`, typed lists, string-keyed dictionaries, nested supported dataclasses, optional types, and declared unions. Channel construction validates the supported subset; unsupported declarations raise exceptions.

A dataclass describes data, not remotely executable behavior. Serialization does not transfer arbitrary class definitions, constructors, `__post_init__` hooks, or object identity. Decoding must not execute user-defined initialization hooks. Each VM constructs or binds its local representation from the agreed schema.

Only serialized values and contract/type identifiers cross the process connection. Pickle and executable deserialization are not used.

Routing and sender metadata are supplied by the application, not trusted from fields inside the body. A delivery can expose that metadata separately from its typed body.

Contracts are immutable for the channel's lifetime. A changed contract requires a new channel; queued messages are not reinterpreted under a changed schema.

Each endpoint exposes its send and receive types through help text, signatures, and annotations. Message size, nesting, and queue lengths are bounded. Validation and overload failures are exceptions.

## 10. Channel delivery, closure, and failure

The delivery contract is:

- Ordering is preserved within each channel.
- There is no global ordering across channels.
- Successful send means accepted for delivery, not processed by the recipient.
- Receive removes a message from its queue.
- Notification does not consume messages.
- Closure and overload are observable; queues cannot grow without bound.

The runtime operates on a single machine. It does not promise transparent recovery or exactly-once application processing across VM crashes. A receive can complete immediately before its VM fails.

If an interpreter crashes, the application detects the failure and marks its connected endpoints failed:

- Future sends to the failed endpoint raise.
- Attempts to receive from it raise rather than wait indefinitely.
- `runtime.wait()` delivers a one-time channel-failure notification to the surviving endpoint's agent.
- Later waits do not repeatedly report the same failure merely because the endpoint remains failed.
- Explicit use of that failed endpoint continues to raise.

The surviving agent decides how to respond. Reporting channel failure does not require cancelling the unrelated operation supplied to `runtime.wait()`.

## 11. Reflection and variable control

Ordinary discovery is supported:

```python
help(runtime.wait)
help(runtime.channels["work"])
dir(runtime)
```

A capability description identifies available runtime services and unavailable features with their reasons. Discovery does not grant authority; the host validates service requests against the session's configured capabilities.

The harness has a separate kernel-control interface to:

- List global names and type descriptions with pagination.
- Preview selected values within a budget.
- Assign supported serialized values.
- Evaluate an assignment inside the container when a richer value is needed.
- Delete a binding.

The host does not evaluate Python. The interpreter performs these operations.

### Safe-point edits

Edits are serialized between foreground executions or at explicit cooperative pauses. The harness does not concurrently alter a running Python frame.

Rebinding a global does not rewrite aliases, coroutine locals, or earlier side effects. Unmanaged Python threads must cooperate before an edit can be guaranteed safe; otherwise the harness defers or rejects the edit.

A wedged interpreter cannot be edited reliably through this mechanism. The UI can retain the last successful inventory, but recovery requires interruption or termination.

### Inspection safety and bounds

Automatic inventories do not call arbitrary `repr()`, properties, descriptors, or iterators. Those can execute code, block, or generate excessive output.

Supported built-in values receive bounded previews; other objects receive opaque descriptions. Explicitly requesting a custom representation is ordinary Python execution, with ordinary execution limits and failure handling.

Names and type descriptions themselves have size limits. Listing globals must not become an unbounded context dump.

## 12. Output handling

Every execution-derived path into model context is bounded, including stdout, stderr, expression display, previews, exceptions, tracebacks, inventories, status events, and service responses.

For example:

```python
print("a" * 500000)
```

returns a prefix and a truncation marker rather than 500,000 characters.

Proposed initial defaults are 1,000 characters per displayed item and 16 KiB of aggregate observations per model activation. The need for limits is fixed; exact defaults require implementation testing.

Limits apply before constructing the model request. Many small writes share the aggregate budget. UTF-8 decoding is streaming and byte-bounded, with reserved space for truncation markers.

Replacing `print()` is insufficient. An in-container supervisor captures stdout/stderr file descriptors, including raw writes and inherited subprocess output. The host also applies a final bound so modifying the in-container collector cannot create an unbounded request.

Once the display budget is exhausted, additional bytes are drained and discarded rather than accumulated in memory or an unlimited log. Sustained output flooding is subject to an execution limit. User output and control messages use separate framing, and both are bounded.

These limits do not truncate Python values in memory. Resource limits separately constrain allocations and processes.

## 13. Example: analyzing ETF returns

Assume a data library provides `find_etfs(criteria)` and `download_history(symbol)`. These functions are illustrative application APIs, not OutRig built-ins.

### Receive the request

The user asks for ETFs matching specified criteria and a comparison of their historical returns. The application queues the message on `user` and notifies the LLM.

The LLM submits:

```python
request = await runtime.channels["user"].receive()
print(request)
```

The bounded output supplies the request to the model.

### Find matching ETFs

The LLM can emit a text explanation of what it plans to do, then submit code:

```python
matching_etfs = await find_etfs(criteria)
print(matching_etfs)
```

Here `criteria` is constructed by generated code from the received request. The matching list remains in Python; its printed representation is bounded.

### Download histories

The LLM submits:

```python
import asyncio

async def download_one(symbol):
    history = await download_history(symbol)
    return symbol, history

downloads = asyncio.gather(
    *(download_one(symbol) for symbol in matching_etfs)
)

histories = dict(await runtime.wait(downloads, "ETF histories"))

for symbol, history in histories.items():
    if history:
        print(
            f"{symbol}: {len(history)} records, "
            f"{history[0].date} through {history[-1].date}"
        )
    else:
        print(f"{symbol}: no records")
```

The application captures the explicit summary output. It does not decide which fields of the histories to report.

If a download failure propagates through gather, the await raises it. Remaining gather children follow ordinary asyncio semantics; one failure does not imply the runtime cancelled all remaining work.

### Handle new input during the wait

Suppose the user requests a different comparison period while the downloads are pending:

1. The application queues the new message on `user`.
2. `runtime.wait()` raises `MessageAvailable`, identifying that channel.
3. If uncaught, the execution ends before assigning `histories` or printing the summary.
4. The downloads continue and remain referenced by `downloads`.
5. The application invokes the LLM with the exception observation.
6. The LLM submits code to receive the message.
7. The LLM decides whether to reuse the downloads, cancel them explicitly, or start different work.

If the downloads are still useful, a subsequent execution submits:

```python
histories = dict(await runtime.wait(downloads, "ETF histories"))
```

If the operation failed meanwhile, awaiting it raises its stored exception. No background-failure model invocation occurred.

### Calculate and explain

The LLM submits Python to compute comparable returns, check date coverage, and produce a bounded table. The application returns the resulting output.

The LLM emits an explanation for the user. The application displays it and controls whether the interaction continues or sleeps awaiting input.

This sequence can occur within one OutRig turn containing several model invocations and Python executions.

## 14. CPython packaging and startup

### Interpreter choice

Use CPython to provide the language and standard-library behavior the model is expected to know. External native-extension loading is a separate capability and is not required for the orchestration runtime.

A restricted host-side interpreter is unsuitable: it either exposes host resources or disables operating-system facilities that ordinary standard-library modules depend on. Executing CPython inside the container avoids using language restrictions as the host-security boundary.

### Release payloads

Release construction produces Linux x86-64 and AArch64 payloads containing:

- A static musl CPython ELF with required native standard-library modules linked in.
- Matching standard-library files.
- The kernel and a small supervisor executable.
- Architecture, version, integrity, and license information.

OutRig embeds verified payloads. At session creation it materializes the selected payload under its session directory and bind-mounts only that subtree read-only into the container. The parent host session directory, configuration, logs, and secrets are not exposed by that mount.

Archive extraction must be manifest-driven and path-confined. Hashes and executable permissions are checked. Executable files are ELF; Python sources are interpreter input rather than a dependence on image-provided script launchers.

The supervisor is executed directly through Podman with the mapped session identity and workspace cwd. Interpreter initialization supplies explicit runtime paths and ignores accidental host/image `PYTHONPATH`, user site packages, and unrelated Python configuration. The bundled library remains available while container files and installed programs retain their ordinary paths.

Nothing is fetched or installed during session execution. Developer builds without a payload report that Python is unavailable, including architecture, build-time reason, and remedy. They do not silently select an arbitrary Python from the image. Other available OutRig capabilities can continue.

### Existing binary-injection precedent

OutRig's `outrig-enter` helper already uses an embedded binary materialized into a session directory and mounted read-only. This design uses that placement mechanism, but not its build recipe.

`outrig-enter` is a dependency-free Rust compilation. CPython and its native dependencies require a separate release-build stage. Cargo packaging consumes prepared artifacts instead of assembling a C toolchain and interpreter recursively inside `build.rs`.

Release CI requires both payloads so a missing artifact cannot produce a nominally successful release.

### Architecture selection

Select by the container image's architecture, not merely the host architecture. Validate the ELF class and `e_machine` of every supplied executable against its manifest and selected image.

A foreign-architecture image may use an existing operator-configured emulator. OutRig does not install one. Missing support produces an architecture-specific diagnostic, not a misleading missing-library error.

### TLS and additional Python stacks

TLS trust uses image-provided trust selected by the operator or an explicitly supplied build-time CA bundle. Missing trust raises a certificate/trust error; it never disables certificate verification.

Third-party native extensions and `pip` are not supported in this orchestration interpreter. An image-provided Python can execute applications needing them through subprocesses, files, or streams without sharing the orchestration heap.

## 15. Containment, failures, and lifecycle

### Operator authority

Python is selected as an operator-controlled runtime capability. It must be visible in the effective environment description, including when the image itself contains no interpreter.

Inside the selected environment, Python has no command allowlist. Outside it, the application connection exposes only authorized runtime services. It provides no arbitrary host Python, host file access, generic host network proxy, or container-runtime socket.

Subagent creation starts another Python process under existing grants. It does not create a sidecar, install tools, add mounts, or alter network policy.

### Limits and supervision

The implementation needs host-enforced deadlines, bounded protocol messages and output, and operator-controlled memory and process limits. Primary-container cgroups constrain aggregate interpreter, subprocess, and subagent resource use; they do not provide per-agent isolation by themselves.

Synchronous code or native calls can block an event loop. `runtime.wait()` is cooperative and cannot interrupt code that never reaches it. A host watchdog and separate operator-stop action remain necessary.

Termination first attempts cooperative interruption. If the execution or its descendants cannot be stopped reliably, terminating the primary container is the final containment action. Killing the `podman exec` client alone does not stop its in-container command.

Stopping the primary affects all agents and tools sharing it. The application must identify that consequence rather than present it as an isolated Python restart. Required limits that cannot be enforced must produce a diagnostic and disable the affected execution capability rather than imply containment that is absent.

### Interpreter and agent failure

An interpreter crash ends its agent. The application marks its connected channels failed and notifies surviving agents according to the channel-failure contract.

Other agent processes can continue if the containing environment remains operational. There is no transparent arbitrary-heap restoration and no automatic replay of executed code. File writes and external side effects may already have happened.

Creating another VM creates another agent instance. It does not silently preserve the failed instance's object identity or channel state.

Ending an OutRig turn does not by itself destroy the VM or channels. Explicit agent removal and ownership-based cleanup require application lifecycle handling separate from turn completion.

### Diagnostics

Diagnostics identify the agent or execution, reason, and affected resources. Distinguish ordinary Python exceptions, channel failure, bridge failure, deadlines, output limits, and VM exit.

Report an out-of-memory cause only when runtime or cgroup evidence supports it. A signal-derived exit status alone is not proof of OOM.

## 16. Credentials and MCP integration

Credentials not needed inside the container remain outside it. Host provider keys do not enter interpreter environment variables, channel bootstrap descriptions, automatic inventories, or model observations.

Unrestricted Python that can read a credential can also print or encode it. Output truncation and redaction cannot guarantee that an arbitrary readable secret never reaches model context. The architecture therefore minimizes unnecessary credential placement and does not claim that output filtering solves credential isolation.

Automatic environment dumps, arbitrary object representations, traceback locals, and unlimited raw-output logs introduce unnecessary disclosure paths and are excluded. Detailed credential-bearing service design remains separate work.

MCP services can later appear as Python objects whose methods return awaitables. Such adapters must preserve the same channel/service authority and output bounds and need not expose their credentials in Python representations.

OutRig documentation currently describes MCP tool calls as the principal way agents act. With this subsystem, Python becomes the primary execution interface and MCP becomes an optional integration surface. Documentation must describe that change explicitly. The security claim remains container isolation, not that every action is an MCP call.

## 17. Evidence, cost, and validation

### Static CPython probe

A packaging probe used this upstream python-build-standalone artifact:

```text
cpython-3.13.15+20260901-x86_64-unknown-linux-musl-noopt+static-full.tar.zst
```

Observed SHA-256:

```text
68606ae38cb3f4db0d0fdb75b16dde78888428161e8f82430915d405b5ea96de
```

This identifies the tested bytes; it is not independent provenance verification.

ELF inspection found an x86-64 ELF64 executable with no `PT_INTERP` and no dynamic section. Upstream metadata reported static CRT, static libpython, and built-in extension loading only.

The probe exercised dataclasses, pathlib, `open()`, help generation, signatures, CSV/IO, importlib, LZMA, synchronous and asynchronous subprocesses using the bundled interpreter, asyncio TCP, timers and queues, top-level await with persistent globals, and certificate-verified HTTPS using an explicit CA file. It imported `random` without functionally testing that module.

These tests ran in an x86-64 environment, not in a minimal Podman container. Podman integration and native AArch64 execution were not tested.

**Since settled by the prototype (x86-64 only).** The same artifact was run inside Podman
against two images and works in both:

```sh
podman run --rm -v "$CACHE":/outrig/python:ro alpine:latest \
  /outrig/python/bin/python3 -I -c \
  'import dataclasses, pathlib, asyncio, csv, ssl, subprocess, random, importlib, lzma; print("ok")'

podman run --rm -v "$CACHE":/outrig/python:ro gcr.io/distroless/static-debian12 \
  /outrig/python/bin/python3 -I -c 'print(1)'
```

The distroless image has no shell and no libc of its own, so printing `1` there is what
establishes the ELF is genuinely self-contained rather than quietly borrowing the image's musl.
`podman exec -i` was separately confirmed to carry raw bytes (`00 01 02 ff fe`) unmodified,
which is what the NDJSON protocol rides on.

Native AArch64 remains untested and out of the prototype's scope.

### Size and startup measurements

| Item | Bytes |
|---|---:|
| Downloaded full static archive | 36,737,124 |
| Unstripped interpreter executable | 47,980,200 |
| Stripped interpreter executable | 22,510,344 |
| Reduced runtime regular files | 32,755,843 |
| Compressed reduced runtime | 11,959,419 |

The reduced tree is a sizing experiment, not a final release manifest. A release must retain licenses and required data and include the supervisor and kernel.

A two-architecture embedded release is estimated to add approximately 25–35 MB compressed, pending measurements of the final payloads. One imported-runtime startup measured about 75 ms and 15,120 KiB maximum RSS; this is not a full-agent performance guarantee.

Static variants must be selected explicitly. Current default musl builds can depend on an image-provided musl runtime. The upstream distinction is documented in [python-build-standalone's behavior notes](https://gregoryszorc.com/docs/python-build-standalone/main/quirks.html).

### Required acceptance tests

Before release, CI must validate:

1. Native execution on x86-64 and AArch64, not merely cross-compilation.
2. Minimal supported images, Alpine, and glibc images without image-provided Python or shell dependencies.
3. ELF architecture, baseline CPU compatibility, and absence of external dynamic runtime dependencies.
4. Functional standard-library behavior, including async subprocesses, DNS, verified TLS, dataclasses, and reflection.
5. OutRig startup, network-policy enforcement, cgroups, supervisor transport, and lifecycle integration.
6. `runtime.wait()` races, queued messages, retained operations, and one-time channel-failure notification.
7. Dataclass contracts across separate VMs, including validation and bounded decoding.
8. Output limits under raw writes, floods, malformed control messages, and hostile representations.
9. Safe-point edits, interpreter crashes, cancellation, and descendant processes.
10. Missing-payload builds, unsupported architectures, missing trust data, and precise degraded-capability diagnostics.

Release engineering also owns pinned interpreter/native dependencies, provenance checks, licenses, SBOM information, and security updates. Downloads occur during release construction, not session execution.

### Deployment invalidation condition

The static-payload deployment is invalid if static musl CPython cannot provide the required ordinary-Python and asyncio behavior on either supported architecture without image-provided runtime libraries.

The x86-64 probe provides partial support. Native AArch64 and minimal-container acceptance tests remain necessary before the deployment can be considered validated.

## 18. Implementation details not fixed by this specification

The architecture defines observable execution and messaging behavior without fixing every implementation choice. Remaining details include:

- Exact subagent and channel construction signatures.
- Concrete application-defined user-message classes.
- Wire encoding and internal transport framing.
- Final output, queue, memory, process, and timing defaults.
- UI presentation and selection of subagent conversations.
- Explicit removal and descendant-cleanup controls.
- Future sibling channels, endpoint transfer, and MCP adapters.

These details must preserve the stated properties: persistent per-agent Python state, application-controlled turns, creator-defined channel contracts, non-cancelling message-aware waits, explicit retrieval of background results, and bounded observations.

## 19. As built

A prototype of this architecture now drives `outrig run`. This section records what was
actually built, what was deferred, and -- more importantly -- the places where the
implementation contradicts what is written above. The sections before this one are the design
as proposed; this one is what happened when it met a real provider.

### Built

Static musl CPython 3.13.15 bind-mounted read-only at `/outrig/python`; one kernel process per
session over `podman exec -i`, speaking NDJSON; a persistent session module in `sys.modules`;
top-level await with explicit completion reporting; fd-level bounded output capture; a bounded
globals inventory; one `user` channel with `receive`/`send`; `runtime.wait()` with the
contract in section 6; and `python_execute` as the agent's only tool.

### Deferred, not attempted

Subagents and multi-channel relationships; serializable dataclass contracts across VMs; the
kernel-control variable-edit interface (section 11); AArch64; ELF architecture validation;
manifest-driven, hash-checked extraction; safe-point edits; cgroups and operator resource
limits; the one-time channel-failure notification (section 10); credential-bearing service
design; MCP servers as Python objects.

### Where the implementation diverges

**The runtime object is `runtime`, not `agent`.** Renamed throughout this document.
`agent.wait()` read as a lifecycle operation on the agent itself, which is precisely what
section 4 says it is not.

**The payload is fetched, not embedded.** Section 14 specifies embedded payloads materialized
per session under the session directory. The prototype uses `scripts/fetch-python-payload.sh`
into a host cache directory, bind-mounted directly. No `build.rs` download, nothing in the
binary, and no per-session copy of ~173 MB. The consequence is that SELinux `,Z` would relabel
a shared cache directory; on a relabeling host the spec's per-session placement is the answer.

**MCP tools are removed entirely, not made optional.** Section 16 anticipates MCP becoming "an
optional integration surface". The prototype gives the model exactly one tool. The servers
still start -- their sidecars, network policy, and teardown are unchanged -- and the startup
banner says `not offered to the agent` so the two lines do not contradict each other.

**Conversation history is discarded every turn, always.** Section 3 says it *can* be discarded.
The prototype does it unconditionally, which makes the per-turn observation block the only
thing carrying state forward.

**An empty completion ends the turn instead of being retried.** This directly contradicts
section 4: "The application reports unusable responses and applies a bounded retry policy."
That rule was written for a model whose final text was its only voice. Once the model speaks
through `runtime.channels["user"]`, a turn that sends and then stops is complete, and an empty
final message is the ordinary way to say so.

The correction is narrower than it first appears, and the difference is the provider arm:

| Arm | Reads its finish reason | An empty completion means |
|---|---|---|
| Anthropic (native) | yes -- a clean `end_turn` is handled upstream | a real fault; retry it |
| OpenAI-compatible   | no -- discarded before OutRig sees it      | ambiguous |

So the narrowing is opted into per arm rather than applied to the error, which carries the
same wording from both. On an arm that cannot tell a stop from a truncation, the prototype
reads it as a stop -- and pays for that by no longer naming a genuine truncation there, guarded
only by the `max-tokens` the request carries.

**A wedged interpreter is recoverable without killing the container.** Section 15 offers
cooperative interruption and then "terminating the primary container is the final containment
action". Cooperative interruption is not enough on its own: synchronous Python blocks the event
loop, and with it every path that could report the problem -- including the
`call_soon_threadsafe` queue, which is what a wedged loop is not draining. The kernel therefore
handles an
`interrupt` message **on its reader thread**, raising SIGINT into the running execution, which
comes back as an ordinary error result. Two alternatives were rejected with evidence:
`ctypes.pythonapi` is `None` in a statically linked build, and `_thread.interrupt_main()`
killed the process in 9 of 10 trials.

The host only interrupts a kernel that has gone quiet. A healthy kernel answers an inventory
while its foreground execution runs, so a long build or download keeps its slot indefinitely;
interrupting a healthy loop is a no-op in any case. Terminating the container remains the
answer for an interpreter parked in a C call Python cannot break into.

**Output is bounded per execution, and background output is billed separately.** Section 12
proposes 1,000 characters per displayed item and 16 KiB per model activation. Capture happens
at the file descriptor, which has no notion of an "item", so the per-item cap applies only
where the kernel itself formats a value -- an echoed `repr`, an inventory entry. The aggregate
is 16 KiB per execution.

Section 12 does not anticipate output arriving *between* executions. It does in practice, from
background tasks, and billing it to whichever execution ran next let a chatty task evict the
result the model actually asked for -- silently, with nothing to say it had been displaced.
Between-execution output is now kept in a separate 2 KiB tail and reported as a labelled,
byte-counted preamble.

**`runtime.wait()` takes a Future, not a bare coroutine.** Section 7's promise that an
interrupted operation can be awaited again holds for anything `asyncio.ensure_future` returns
unchanged -- a `gather`, a `Task`. A bare coroutine gets wrapped, leaving the caller's name
bound to something that cannot be awaited twice. The system prompt says to bind
`asyncio.gather(...)` or `asyncio.ensure_future(...)`.

**The model has two ways to reach the user, with different jobs.** Section 4 notes both exist.
In practice the model narrates in its own text whether or not it is told to, so the split is
stated rather than fought: its own text is running commentary, and a `send()` on the `user`
channel is a deliberate message -- a result, an answer, or a notification from work that
finished after the model stopped writing. A sent message is labelled `[outrig] agent message:`
on stderr with the body on stdout, so a redirect still captures only what the agent said.

### Details this specification left open, and how they were settled

Section 18 lists what it does not fix. The prototype's answers:

- **Wire encoding and framing**: NDJSON, one object per line, over the `podman exec` stdio.
  The kernel dups the real stdout aside for the protocol and points fds 1 and 2 at a capture
  pipe, so generated code cannot forge a frame no matter what it prints.
- **User-message classes**: one `UserText` carrying a string, both directions.
- **Output, queue, and timing defaults**: 16 KiB per execution, 2 KiB of background output,
  1,000 characters per echoed value, 200 inventory entries, a 30-second liveness check and a
  10-second interrupt grace.
- **Kernel delivery**: the kernel source is passed as the interpreter's `-c` argument rather
  than mounted, so it rebuilds with the binary. The cost is `<string>` in a traceback raised
  inside the kernel itself; model-visible tracebacks name `<execution>` explicitly.

## Repository references

The implementation must remain consistent with these existing OutRig definitions and constraints:

- `doc/README.md`: application placement and the container-based execution model.
- `SECURITY.md`: container isolation, operator-granted access, and arbitrary execution inside the container.
- `doc/concepts/containers.md`: container startup requirements, mapped execution identity, mounts, and process lifecycle behavior.
- `doc/reference/config.md`: capability configuration and diagnostics for features unavailable in a particular build.
- `crates/outrig/build.rs` and `crates/outrig/src/container/enter/mod.rs`: the existing build, embedding, and materialization mechanism for `outrig-enter`.
- `crates/outrig-cli/src/mcp_self/docs/concepts/mcp-trust-model.md`: fixed environment grants and the rule that agents cannot start additional sidecars.
- `plan/next/enter-arch-mismatch.md`: architecture validation requirements for supplied executables.
- `plan/next/openai-arm-sends-no-ceiling.md`: model output-token limits and textless completions.

The Python APIs described in this document are additions to that repository behavior, not interfaces assumed to exist already.
