# A URL server's listing failure still names a stderr file

## Context

`OutrigError::McpToolsListFailed` carries `stderr_path` and `stderr_tail` as plain values, and
renders `stderr (<path>):\n<tail>`. A URL MCP service (`0003-57`) runs no process, so its client
has no stderr file: its listing failure carries an empty path and a tail saying there is none,
and a failed relist's log line still reads `stderr ():`. The MCP table's startup cause reads the
empty path as no tail at all (`Failed::of`), a second place deciding the same thing.

## Shape

- `McpClient`'s stderr path becomes an `Option`, and `McpToolsListFailed`'s `stderr_path` and
  `stderr_tail` become `Option`s, its text leaving the stderr out when there is none -- a change
  to a public error's fields, so for a release that takes one.
- `Failed::of` reads the `Option` as it is.

## Acceptance

- A URL server whose `tools/list` fails: the error's text and the relist's log line name no
  stderr, and its startup cause has no tail.
