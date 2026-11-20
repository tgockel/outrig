"""MCP services: an MCP server the session places, reached in an agent's kernel as an object of a
class generated from the server's tool listing, a method per tool by the tool's exact name.

The object is an `outrig.McpService`, an `outrig.Service` whose calls return an `outrig.McpResult`
-- the server's content blocks in order, its structured content, its `_meta` and whether it marked
the result an error -- rather than JSON data. A result the server marked as an error raises
`outrig.ToolError`, and a call that returns no result `outrig.McpError`.
"""

import base64
import collections
import functools
import threading
import types

from . import _services
from ._services import Service, ServiceError, _decoded, _failure, _line

# The interpreter's, bound as it loads the package: `_relist(obj)` asks the host to list again the
# tools of the MCP service `obj` stands for, from the calling kernel, and returns the host's answer,
# a line with `ok` or with `error`. On the host it stays `None`.
_relist = None

# How many times the host said each service's server announced a changed listing, and how many of
# those the latest relist covers, by service: a service is stale while the first is ahead. Counts,
# kept as the highest seen, so a relist's answer and an announcement read alike in either order.
_heard = {}
_relisted = {}
_counted = threading.Lock()

# How much of a result's text an exception's message carries, in characters.
_TOLD_MAX = 1000


def _list_changed(name, heard):
    """The host said the server of the MCP service `name` announced a changed listing, the
    `heard`th time: the reader thread's, as the line comes."""
    with _counted:
        _heard[name] = max(_heard.get(name, 0), heard)


class McpError(ServiceError):
    """A call of an MCP service's tool, or a relist, that gave no result: an
    `outrig.ServiceError` whose `tool` is the tool's exact name -- its `method` -- and whose `state`
    says where in the call it ended, as far as OutRig knows: `"response_received"`, the server
    answered, or `None`.
    """

    __module__ = "outrig"

    def __init__(self, message, *, state=None, **fields):
        super().__init__(message, **fields)
        self.state = state

    @property
    def tool(self):
        """The tool's exact name, as `method` has it."""
        return self.method

    def _fields(self):
        return {**super()._fields(), "state": self.state}


class ToolError(McpError):
    """A tool's result the server marked as an error: the tool ran and said it failed, which is a
    response, effects and all. `result` is the whole `outrig.McpResult` -- its `content` usually
    says what went wrong -- `outcome` is `returned`, and `state` is `"response_received"`.
    Nothing calls the tool again for you.
    """

    __module__ = "outrig"

    def __init__(self, message, *, result=None, **fields):
        super().__init__(message, **fields)
        self.result = result

    def _fields(self):
        return {**super()._fields(), "result": self.result}


class McpBlock:
    """One block of an MCP tool's result, of the `kind` the server gave it -- `text`, `image`,
    `audio`, `resource`, `resource_link`, or another this build does not model -- with its
    `annotations` and `meta` (its `_meta`), each `None` when absent. `raw` is the block as it
    crossed, JSON data.
    """

    __module__ = "outrig"

    def __init__(self, raw):
        self.raw = raw

    @property
    def kind(self):
        return self.raw.get("type")

    @property
    def annotations(self):
        return self.raw.get("annotations")

    @property
    def meta(self):
        return self.raw.get("_meta")

    def _rendered(self):
        return f"[unsupported content block: {self.kind}]"


class McpText(McpBlock):
    """A text block: `text`."""

    __module__ = "outrig"

    @property
    def text(self):
        return self.raw.get("text")

    def _rendered(self):
        return self.text or ""

    def __repr__(self):
        return f"<outrig.McpText: {len(self.text or '')} characters>"


class _Media(McpBlock):
    """A block of base64 bytes of a `mime_type`: `data`, the bytes, decoded on first use, and
    `base64`, as they crossed."""

    @property
    def mime_type(self):
        return self.raw.get("mimeType")

    @property
    def base64(self):
        return self.raw.get("data")

    @functools.cached_property
    def data(self):
        return base64.b64decode(self.base64 or "")

    def _rendered(self):
        return f"[{self.kind}: {self.mime_type}, {_size(self.base64)} base64 bytes]"

    def __repr__(self):
        return f"<outrig.{type(self).__name__}: {self.mime_type}, {_size(self.base64)} base64 bytes>"


class McpImage(_Media):
    """An image block: its `mime_type`, and its bytes as `data`, decoded from `base64` on first
    use."""

    __module__ = "outrig"


class McpAudio(_Media):
    """An audio block: its `mime_type`, and its bytes as `data`, decoded from `base64` on first
    use."""

    __module__ = "outrig"


class McpResource(McpBlock):
    """A resource embedded in the result: its `uri`, its `mime_type`, and its contents -- `text`,
    or bytes as `blob`, decoded from `base64` on first use; whichever it is not is `None`.
    `contents` is the contents as they crossed, which a shape this build does not model is only.
    """

    __module__ = "outrig"

    @property
    def contents(self):
        return self.raw.get("resource") or {}

    @property
    def uri(self):
        return self.contents.get("uri")

    @property
    def mime_type(self):
        return self.contents.get("mimeType")

    @property
    def text(self):
        text = self.contents.get("text")
        return text if isinstance(text, str) else None

    @property
    def base64(self):
        blob = self.contents.get("blob")
        return blob if isinstance(blob, str) and self.text is None else None

    @functools.cached_property
    def blob(self):
        return None if self.base64 is None else base64.b64decode(self.base64)

    def _rendered(self):
        if self.text is not None:
            return self.text
        if self.base64 is not None:
            mime = self.mime_type or "application/octet-stream"
            return f"[blob: {mime}, {_size(self.base64)} base64 bytes]"
        return "[unsupported resource contents]"

    def __repr__(self):
        return f"<outrig.McpResource: {self.uri}>"


class McpResourceLink(McpBlock):
    """A link to a resource the server can give, which the result does not hold: its `uri`,
    `name`, `title`, `description`, `mime_type`, `size` in bytes and `icons`, each `None` when
    absent."""

    __module__ = "outrig"

    uri = property(lambda self: self.raw.get("uri"))
    name = property(lambda self: self.raw.get("name"))
    title = property(lambda self: self.raw.get("title"))
    description = property(lambda self: self.raw.get("description"))
    mime_type = property(lambda self: self.raw.get("mimeType"))
    size = property(lambda self: self.raw.get("size"))
    icons = property(lambda self: self.raw.get("icons"))

    def _rendered(self):
        return f"[resource link: {self.uri}]"

    def __repr__(self):
        return f"<outrig.McpResourceLink: {self.uri}>"


class McpOther(McpBlock):
    """A block of a kind this build does not model: its `kind`, and `raw`, the block as it crossed.
    """

    __module__ = "outrig"

    @property
    def kind(self):
        kind = self.raw.get("type") if isinstance(self.raw, dict) else None
        return kind if isinstance(kind, str) else "unknown"

    @property
    def annotations(self):
        return self.raw.get("annotations") if isinstance(self.raw, dict) else None

    @property
    def meta(self):
        return self.raw.get("_meta") if isinstance(self.raw, dict) else None

    def __repr__(self):
        return f"<outrig.McpOther: {self.kind}>"


_BLOCKS = {
    "text": McpText,
    "image": McpImage,
    "audio": McpAudio,
    "resource": McpResource,
    "resource_link": McpResourceLink,
}


def _block(raw):
    """The block `raw`, one element of a result's `content`, is."""
    if not isinstance(raw, dict):
        return McpOther(raw)
    return _BLOCKS.get(raw.get("type"), McpOther)(raw)


def _size(text):
    """How long `text` is in bytes, as the host counts a base64 payload: as many as its
    characters, for base64, which is ASCII, with no copy made to count them."""
    text = text or ""
    return len(text) if text.isascii() else len(text.encode("utf-8", "surrogatepass"))


class McpResult:
    """What an MCP tool returned: `content`, its blocks in the server's order -- each an
    `outrig.McpText`, `McpImage`, `McpAudio`, `McpResource`, `McpResourceLink`, or `McpOther` for
    a kind this build does not model, each with its `kind` -- `structured`, its structured content
    as Python data or `None`, `meta`, its `_meta` or `None`, and `is_error`, whether the server
    marked it an error (a call raises `outrig.ToolError` then, carrying it).

    `text` is a view, not the data: the blocks as one string, a block's text as it is and every
    other block a bracketed placeholder. It drops what is not text, on purpose. Filter `content`
    or `structured` in Python and print what you need: a large result is better kept here than
    printed whole.
    """

    __module__ = "outrig"
    __slots__ = ("content", "structured", "meta", "is_error", "_text")

    def __init__(self, raw):
        raw = raw if isinstance(raw, dict) else {}
        content = raw.get("content")
        self.content = tuple(_block(block) for block in (content if isinstance(content, list)
                                                         else ()))
        self.structured = raw.get("structuredContent")
        self.meta = raw.get("_meta")
        self.is_error = raw.get("isError") is True

    @property
    def text(self):
        """The blocks as one string, joined by newlines: lossy, a view for reading."""
        try:
            return self._text
        except AttributeError:
            self._text = "\n".join(block._rendered() for block in self.content)
            return self._text

    def __repr__(self):
        kinds = collections.Counter(block.kind for block in self.content)
        parts = [", ".join(f"{count} {kind}" for kind, count in kinds.items()) or "no content"]
        if self.structured is not None:
            parts.append(f"structured {type(self.structured).__name__}")
        if self.is_error:
            parts.append("is_error")
        return f"<outrig.McpResult: {'; '.join(parts)}>"


class McpRelisted(collections.namedtuple(
        "McpRelisted", ("added", "removed", "changed", "undecoded", "listing"))):
    """What a relist found: the tools `added`, `removed` and `changed`, each by exact name; the
    tools its server listed that did not decode, as `(name, reason)`; and the `listing` the
    service is described at now."""

    __module__ = "outrig"
    __slots__ = ()


class McpTool:
    """A tool of an MCP service, resolved by its exact name: `name`, the `descriptor` its server
    listed it with -- `inputSchema`, and `title`, `description`, `outputSchema`, `annotations`,
    `icons` and `_meta` where it gave them -- and the `listing` it was resolved from. Calling it
    calls the tool by that name: one a later listing removed is refused, and is never another
    tool. `method` is the service's method it calls, whose `help()` describes its arguments.
    """

    __module__ = "outrig"
    __slots__ = ("name", "descriptor", "listing", "method")

    def __init__(self, name, descriptor, listing, method):
        self.name = name
        self.descriptor = descriptor
        self.listing = listing
        self.method = method

    def __call__(self, *args, **kwargs):
        return self.method(*args, **kwargs)

    def __repr__(self):
        return f"<outrig.McpTool {self.name!r} of listing {self.listing}>"


class McpService(Service):
    """One of the session's MCP services: an MCP server the host runs, each tool of which is a
    method of this object, a call to the host by value that blocks until it answers.

    A call returns an `outrig.McpResult`: the server's content blocks in order, its structured
    content, its `_meta`, and whether it marked the result an error -- in which case the call
    raises `outrig.ToolError`, carrying it. A call that returns no result raises
    `outrig.McpError`. OutRig never calls a tool again for you, whatever happened to the first
    call.

    Every tool is reachable by its exact name: `call(name, arguments)` calls it, `tool(name)`
    gives it with its descriptor, and `tools` lists the descriptors as the server gave them.
    `relist()` asks the server for its tools again; `stale` says whether the server has said its
    listing changed since, and `listing` which listing the methods come from.
    """

    __module__ = "outrig"
    __slots__ = ()

    # What each generated class says of its server: set as it is made.
    _tools = types.MappingProxyType({})
    _placement = None

    @classmethod
    def _namespace(cls, entry):
        tools = {}
        for method in entry["methods"]:
            name = method.get("name")
            if not isinstance(name, str) or name in tools:
                continue
            extras = method.get("extras") or {}
            descriptor = {"name": name}
            if "title" in extras:
                descriptor["title"] = extras["title"]
            if method.get("description"):
                descriptor["description"] = method["description"]
            descriptor["inputSchema"] = method.get("params")
            if method.get("result") is not None:
                descriptor["outputSchema"] = method["result"]
            for key in ("annotations", "icons", "_meta"):
                if key in extras:
                    descriptor[key] = extras[key]
            tools[name] = types.MappingProxyType(descriptor)
        placement = (entry.get("extras") or {}).get("placement")
        return {"_tools": types.MappingProxyType(tools), "_placement": placement}

    @classmethod
    def _about(cls, entry):
        placement = (entry.get("extras") or {}).get("placement")
        where = f", placed as a {placement.replace('_', ' ')}" if placement else ""
        return ("An MCP server the host runs" + where + ": each tool is a method, a call to the "
                "host, by value, which blocks until it answers and returns an outrig.McpResult -- "
                "the content blocks in order, structured content, `_meta` and `is_error`. "
                "`call(name, arguments)` calls any tool by its exact name, `tool(name)` gives one "
                "with its descriptor, `tools` lists the descriptors, and `relist()` lists them "
                "again. A result the server marks as an error raises outrig.ToolError, and a call "
                "that returns no result raises outrig.McpError.")

    @property
    def tools(self):
        """Each tool's descriptor as the server listed it, by exact name, in its order."""
        return type(self)._tools

    def tool(self, name):
        """The tool named exactly `name`, as an `outrig.McpTool`: callable by that name, with its
        descriptor and the listing it was resolved from."""
        cls = type(self)
        if name not in cls._tools:
            raise AttributeError(f"MCP service {cls._service!r} has no tool {name!r}: its "
                                 f"`tools` lists those it has")
        return McpTool(name, cls._tools[name], cls._generation, self.method(name))

    @property
    def listing(self):
        """Which listing of the server's tools the methods come from: 1, and one more for each
        relist that changed them."""
        return type(self)._generation

    @property
    def stale(self):
        """Whether the server has said its tool listing changed since it was last listed. Nothing
        relists for it: `relist()` does."""
        name = type(self)._service
        with _counted:
            return _heard.get(name, 0) > _relisted.get(name, 0)

    def relist(self):
        """Ask the server for its tools again, and take what it lists as this service's methods:
        a tool added is a method from now on, one removed is refused when called, and a method or
        `outrig.McpTool` already held still calls the tool it named. Returns an
        `outrig.McpRelisted`: what was added, removed and changed, and what did not decode.
        Raises `outrig.McpError` when the tools could not be listed."""
        name = type(self)._service
        answer = _relist(self)
        error = answer.get("error")
        if error is not None:
            raise _failure(McpError, name, None, error)
        ok = answer["ok"]
        with _counted:
            _relisted[name] = max(_relisted.get(name, 0), ok.get("relisted", 0))
        undecoded = tuple((each.get("name"), each.get("reason")) for each in ok["undecoded"])
        return McpRelisted(tuple(ok["added"]), tuple(ok["removed"]), tuple(ok["changed"]),
                           undecoded, ok["listing"])

    def _answered(self, method, answer):
        name = type(self)._service
        error = answer.get("error")
        if error is not None:
            # A call that returned reached its server, which answered with a result too large to
            # hand over; no other failure says yet where it ended.
            state = "response_received" if error.get("outcome") == "returned" else None
            raise _failure(McpError, name, method, error, state=state)
        result = McpResult(_decoded(name, method, answer["ok"]))
        if result.is_error:
            text = result.text
            if len(text) > _TOLD_MAX:
                text = text[:_TOLD_MAX] + f"... [{len(text) - _TOLD_MAX} more characters]"
            raise ToolError(f"the tool reported an error: {text}" if text
                            else "the tool reported an error", outcome="returned", service=name,
                            method=method, call=answer.get("call"), state="response_received",
                            result=result)
        return result

    def __dir__(self):
        return sorted({*super().__dir__(), "tools", "tool", "listing", "stale", "relist"})

    def __repr__(self):
        cls = type(self)
        count = len(cls._tools)
        where = f" ({cls._placement})" if cls._placement else ""
        return (f"<outrig MCP service {cls._service!r}{where}: {_line(cls._description)} "
                f"({count} tool{'' if count == 1 else 's'})>")


_services._PRESENTERS["mcp"] = McpService
