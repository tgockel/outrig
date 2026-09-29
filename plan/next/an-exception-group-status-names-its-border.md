# An exception group's status line names the traceback's border, not the exception

## Context

`submit_python` opens an error result with a status line, `[this code raised <exception>]`,
kept whole however the rest is cut. `exception_line` in `crates/outrig/src/agent/tool.rs` takes
the traceback's last non-empty line for it. For an ordinary exception that is `ValueError: boom`.

An exception group is formatted as nested boxes, and its last line is the closing border. Code
whose `asyncio.TaskGroup` has a failing task -- `async with asyncio.TaskGroup() as tg:
tg.create_task(fails())` -- comes back as:

```text
[this code raised +------------------------------------]
```

The exception's own line, `ExceptionGroup: unhandled errors in a TaskGroup (1 sub-exception)`,
sits higher up, behind a `| ` prefix. Checked against the payload's 3.13.15 while `0003-09` was
built; `0003-09` makes the case likelier, since it tells the model to wait on tasks, and a
`TaskGroup` is the ordinary way to start several. A redirection raised in one of a `TaskGroup`'s
tasks arrives the same way, as a `BaseExceptionGroup`, since `MessageAvailable` is a
`BaseException`.

The traceback in the detail still names the group, until the detail is cut to fit the result's
bound. The status is kept whole for exactly that case, so there it is the only sign of what was
raised.

## Shape

- Find the line the traceback module writes for the group itself: the one just above the first
  member's separator (`+-+---------------- 1 ----------------`), less its `| ` prefix. The last
  line that is not a border is the last member's exception instead. Or have the interpreter report
  the exception's one-line form beside the traceback, since it has the exception object and the
  host only has text -- a change to the `result` message.
- Either way the status names the group, not its first member: the group is what was raised.

## Acceptance

- A `render` test in `agent/agent_tests.rs` over a formatted exception-group traceback: the status
  reads `[this code raised ExceptionGroup: unhandled errors in a TaskGroup (1 sub-exception)]`.
- The same for a `BaseExceptionGroup`, whose member is a `MessageAvailable`.
