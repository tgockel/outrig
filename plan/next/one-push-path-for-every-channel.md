# One push path for the user channel and an external one

`0003-51` added a second way for the host to queue an item in a kernel: a session-level
`channel.msg` line answered `posted` (`_channel_msg` in `_interpreter.py`;
`Interpreter::post_item`, `Query::Posted`, `Reply::Posted` in `python/host.rs`), beside the user
channel's per-kernel `msg` line answered `msg` (`_msg`; `Interpreter::posting`, `Query::Post`,
`Reply::Msg`). Both queue one item and answer once; they differ in envelope -- the external
answer carries a refusal's `code` and `problems`, the user channel's a bare string -- and the
user path's `Reply::Msg` arm emits `message.refused` with `user` hard-coded.

The session-level route is needed (an external channel's window exists before any kernel holds
it). The split replies and queries are not: one push reply, `{pending}` or `{error: {code,
message, problems}}`, and one host method, with `posting` becoming `post_item` plus the user
channel's event closure, would leave one answer to keep in step and give `UserChannel::send` a
typed refusal (`full`, `contract`) instead of a string. Found by `0003-51`'s `/simplify`;
deferred as a change to the user channel, outside that task.
