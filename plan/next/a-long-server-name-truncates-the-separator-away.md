# A long enough server name leaves no `__` in the advertised name

## Context

`tool_name::suffixed` truncates the replaced body to `MAX_NAME_LEN - 1 - hex_len` (57 at the
production width) before appending the suffix. A server name longer than that is cut before
its separator, so the advertised name contains no `__` at all:

```
sanitize(&"s".repeat(57), "a/")
  == "sssssssssssssssssssssssssssssssssssssssssssssssssssssssss_4012cd"
```

That contradicts the proxy's own `ServerInfo` instructions, which tell a model that "the prefix
identifies which backing MCP server hosts the tool". `is_valid_mcp_server_name`
(`crates/outrig/src/config/validate.rs`) is `^[a-zA-Z][a-zA-Z0-9_-]*$` with no length bound, so
a config can reach this.

Compositions already over 64 characters truncated the same way before `0002-44`, so the class
is not new. `0002-44` widened the window: a pair like the one above composes to 61 characters
and used to be returned intact, and is now lossy and therefore truncated.

Nothing parses the prefix -- the proxy routes by a hash-map lookup on the whole advertised
name -- so this is legibility, for a human reading a tool list and for a model reading the
instructions string, not a routing defect.

## Goal

Either the prefix survives, or nothing claims it does.

## Deliverables

- **Pick one.** Bound MCP server-name length in config validation, so a name that cannot
  survive prefixing is rejected where it is declared and the error names the limit; or qualify
  the `ServerInfo` instructions so the prefix is described as the usual case rather than a
  guarantee. The first is the better contract if a bound can be chosen without breaking an
  existing config; a bound around 24 leaves room for a useful tool name at every width.
- **Whichever is chosen, say it in `doc/reference/config.md`**, beside the server-name regex.

## Acceptance

- A server name too long to survive prefixing is either rejected at config load with an error
  naming the limit, or the instructions string no longer promises the prefix.
- `doc/reference/config.md`'s server-name rules match whichever was chosen.

## Dependencies

- None. Found reviewing `0002-44`.
