# MCP wrappers

Deferred out of the first milestone, and blocked on `security.md` and `pyro-remote-objects.md`
together, for a reason worth stating up front rather than discovering later.

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

MCP clients live on the other side of the boundary. The sandbox gets proxies. Which makes the
placement question in `security.md` this document's prerequisite too: a sidecar with its own
image can carry a normal CPython with pip, and that is what makes a Python MCP client possible
at all. A second process in the primary container inherits the same static interpreter and the
same inability to load compiled extensions, so it would not help.

```text
   UNTRUSTED                          TRUSTED                      ELSEWHERE
   +------------------+               +----------------------+     +-------------+
   | runtime.mcp.fs   |               | MCP client           |     | MCP server  |
   |   .read_file() --+--- proxy -----+-> tools, credentials-+-----+> (sidecar)  |
   +------------------+               +----------------------+     +-------------+
```

This composes with the isolation work rather than merely coexisting with it, and it solves a
second problem as a side effect. mcp2py reads provider keys from the environment
(`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, `MCP_TOKEN`) and caches OAuth tokens in plaintext at
`~/.config/mcp2py/tokens.json`. Running it in the sandbox would pull exactly the credentials
this design is trying to isolate into the process the model controls. Trusted-side placement
keeps them out.

## The alternative that may remove the dependency

OutRig already speaks MCP, in Rust. `crates/outrig/src/mcp.rs` has a working client that
connects over `podman exec` stdio and has been in use since 0.1; `crates/outrig/src/mcp_proxy.rs`
already aggregates several servers behind one surface; and `crates/outrig-cli/src/rig_tool.rs`
already turns a discovered tool into something an agent can call.

So the trusted side does not obviously need a Python MCP client at all. It could expose the
clients OutRig already has, and the sandbox would never know the difference. That would mean:

- no new dependency, and no exposure to a project with 39 commits and no release in ten months;
- one MCP implementation in the codebase rather than two that can disagree about a spec;
- tool discovery, name sanitization, and result handling reusing code that exists and is tested;
- and correspondingly, writing the object-presentation layer rather than getting it for free.

Against that, mcp2py brings real work already done: generated type hints and docstrings from
tool descriptions, resources as module attributes, prompts as template functions, OAuth with
PKCE, and IDE stubs. Whether any of it is wanted here is a separate question from whether it is
good.

**This is not decided.** Both directions are live and the choice should be made when the
isolation boundary exists and its shape is known, since the exposed surface is what either
approach has to be built against.

## mcp2py, recorded honestly

Assessed at 0.6.0: MIT, on PyPI, 254 stars, **39 commits total**, single primary author, all
six releases landing inside one two-week window in late 2025, and no commit in roughly ten
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

None of this makes it unusable. It makes it a dependency to adopt with open eyes, on the
trusted side, pinned.

## What the object has to be, whichever way it is built

The choice above is about where the client lives. These are true either way, and they are the part
an agent actually experiences.

**Awaitable, and not merely wrapped.** A call crosses a socket and waits on something remote. If
that blocks the agent's event loop it stops channel delivery, which is what lets a user redirect
an agent mid-wait -- most of what makes long-running work tolerable here.
`pyro-remote-objects.md` records the same constraint from the transport side.

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

- Whether to use mcp2py at all, against exposing OutRig's existing Rust clients.
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
