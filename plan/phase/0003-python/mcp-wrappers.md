# MCP wrappers

Not in this phase. Hosted objects (`hosted-objects.md`) now give the sandbox a way to reach
objects on the other side of a boundary, but neither route to an MCP client fits what this phase
builds, for a reason worth stating up front rather than discovering later: the only MCP client
that can run is OutRig's own, written in Rust, and presenting a Rust object to agent Python is
deferred to `plan/next/rust-object-as-python-object.md`. That entry is this page's route.

The goal: an MCP server presented to the model as a Python object, its tools as methods. AI
tooling is built around MCP, and the alternative -- telling the model that a whole category of
integration is unreachable -- is worse than wrapping it.

## The finding that decides the shape

[mcp2py](https://github.com/MaximeRivest/mcp2py) is the obvious candidate. It cannot run in the
sandbox, and neither can anything else built on the official Python MCP SDK.

mcp2py is itself pure Python. Its dependencies are not. It requires `mcp`, the official SDK,
which requires `pydantic`, which requires `pydantic-core` -- compiled Rust. It also requires
`litellm` unconditionally (for a feature its own comment marks as a later phase), which brings
`tiktoken`, `tokenizers`, `aiohttp`, and `fastuuid`, all compiled.

Dropping `litellm` does not rescue it. The `mcp` SDK alone reaches `pydantic-core`, so **any**
approach that puts the official Python MCP SDK inside this interpreter is blocked. The
interpreter is a static CPython that cannot load third-party extension modules at all, so the
existence of musl wheels for those packages does not help -- there is no mechanism to load one.

That is not a defect in mcp2py. It is a statement about where MCP client code can run.

## Which gives the architecture

MCP clients live on the other side of the boundary. The sandbox gets proxies.

An earlier draft of this page answered "where" with a sidecar carrying a normal CPython with pip,
which is what would have made a Python MCP client possible. That premise is gone.
`security.md` now places trusted objects on the host, and a hosted library runs in the same
embedded static CPython the container uses (`hosted-objects.md`), so the host side cannot load
`pydantic-core` either. A second process in the primary container would have the same limit.
The Python MCP SDK therefore runs nowhere OutRig starts a Python.

```text
   UNTRUSTED                          TRUSTED                      ELSEWHERE
   +------------------+               +----------------------+     +-------------+
   | runtime.mcp.fs   |               | MCP client           |     | MCP server  |
   |   .read_file() --+--- proxy -----+-> tools, credentials-+-----+> (sidecar)  |
   +------------------+               +----------------------+     +-------------+
```

mcp2py's credential handling would also have required trusted-side placement, had it been able to
load; with mcp2py out, the point is moot. It reads provider keys from the environment
(`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, `MCP_TOKEN`) and caches OAuth tokens in plaintext at
`~/.config/mcp2py/tokens.json`, so running it in the sandbox would have put those credentials in
the process the model controls.

## OutRig's own client is the route

**Decided in planning (2026-09-30): the route is the MCP client OutRig already has, in Rust.**
`crates/outrig/src/mcp.rs` has a working client that connects over `podman exec` stdio and has
been in use since 0.1; `crates/outrig/src/mcp_proxy.rs` already aggregates several servers behind
one surface; and `crates/outrig-cli/src/rig_tool.rs` already turns a discovered tool into
something an agent can call.

So the trusted side needs no Python MCP client. It exposes the clients OutRig already has, and the
sandbox cannot tell the difference. That means:

- no new dependency, and no exposure to a project with 39 commits and no release in ten months;
- one MCP implementation in the codebase rather than two that can disagree about a spec;
- tool discovery, name sanitization, and result handling reusing code that exists and is tested;
- and correspondingly, writing the object-presentation layer rather than getting it for free.

mcp2py would have brought real work already done: generated type hints and docstrings from tool
descriptions, resources as module attributes, prompts as template functions, OAuth with PKCE, and
IDE stubs. Which of those this route reproduces is a separate question from the route itself.

mcp2py is out for the reason above: its SDK cannot load in the embedded Python on either side of
the boundary. What the route still needs is a way to present a Rust object to agent Python with
the same policy and events as a hosted Python object, and that mechanism is deferred past this
phase to `plan/next/rust-object-as-python-object.md`. Until it exists, `run-new` starts no MCP
server.

## mcp2py, recorded honestly

Assessed at 0.6.0: MIT, on PyPI, 254 stars, **39 commits total**, single primary author, all
six releases published inside one two-week window in late 2025, and no commit in roughly ten
months. The README carries no status, limitations, or roadmap section and presents the project
as production-ready. The commit history does not support that claim. The last activity was an
outside contributor's fix to an SSE `Accept` header, which is to say interop bugs were still
being found when work stopped.

It supports both stdio and HTTP/SSE transports, and the API is small:

```python
from mcp2py import load
fstools = load("npx -y @modelcontextprotocol/server-filesystem /home")
fstools.list_directory("/home")
```

None of this made it unusable. It is recorded because it was the obvious candidate, and the
reason it is not used is the compiled dependency above, not its maintenance record.

## What the object has to be, whichever way it is built

The choice above is about where the client lives. These are true however the object is
presented, and they are the part an agent actually experiences.

**Awaitable, and not merely wrapped.** A call waits on something remote. If that blocks the
agent's event loop it stops channel delivery, which is what lets a user redirect an agent
mid-wait -- most of what makes long-running work tolerable here. Hosted Python objects accept a
blocking call and offer `asyncio.to_thread` (`hosted-objects.md`); a Rust-presented object has no
library forcing that choice on it, so its calls should be awaitable from the start.

**Results stay data.** A tool that returns a structured result must arrive as a structure the
agent can filter in Python, not as prose it has to parse back. This is the whole economic argument
of the phase applied to integrations: a large result should be processed in the interpreter with
only the interesting part observed. Flattening it to text at the boundary throws that away and
cannot be recovered.

**Errors are distinguishable.** At least: the arguments were invalid, the call was not permitted,
the remote tool failed, the transport failed, and -- separately from all of them -- the outcome is
unknown. That last one is not a nicety. A write whose response was lost must not be retried
blindly, and an agent cannot make that judgment if every failure arrives as the same exception.

**Timeouts and retries are declared, not assumed.** Whether a call is safe to repeat is a
property of the tool, not of whether it looks like a read: a repeat can spend quota, cost money,
or return something different. So it belongs with the tool's declaration rather than in a global
policy or an inference from the verb.

**Names are handled, with an escape.** MCP tool names pass `^[a-zA-Z0-9_-]{1,64}$`, which admits
things that are not Python identifiers and things that are Python keywords. Attribute access needs
a documented mapping and an exact-name lookup for whatever the mapping mangles. The existing
sanitizer in the Rust tool surface is prior art for the collision half of this.

**Documentation comes from the schema.** A tool carries a description and a parameter schema, and
turning those into a signature and a docstring is what makes `discovery.md`'s `help()` answer
something useful instead of `(*args, **kwargs)`.

## Open questions

- Everything in `plan/next/rust-object-as-python-object.md`, which this page now depends on.
- How an MCP tool's schema becomes a Python signature the model can read with `help()`.
- What happens to the tool-name sanitization that exists for the current adapter. Python
  attribute names have different rules than the `^[a-zA-Z0-9_-]{1,64}$` the tool surface uses.
- Whether MCP resources and prompts are presented at all, or only tools.
- How this interacts with the trust model in `doc/concepts/mcp-trust-model.md`, which currently
  describes MCP tool calls as the way an agent acts. That document needs rewriting either way;
  this phase demotes MCP to one integration surface among several.

## Unverified

The `jsonschema` to `rpds-py` compiled dependency was not independently confirmed. It does not
change the conclusion -- `pydantic-core` alone is decisive -- but do not cite it as established.
