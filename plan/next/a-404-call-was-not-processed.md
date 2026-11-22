# A call a URL server answered 404 is read as may have dispatched

## Context

A URL MCP service's client keeps rmcp's own session recovery off (`0003-57`), so a server that
restarted between two calls -- its sessions gone -- answers the next call's POST 404, and rmcp
returns `SessionExpired` through `ServiceError::TransportSend`. `wait_state` reads every
`TransportSend` as `MayHaveDispatched`, so that call is recorded uncertain and makes the report's
verdict `stopped_with_unknown`, though the protocol has the server answer 404 for a session it
does not have -- before it handles the request. The service is marked disconnected, and the call
after it reconnects; the uncertainty is the only thing wrong.

## Shape

- In `call_tool_within`, a `TransportSend` whose error is the HTTP transport's `SessionExpired`
  is `ResponseReceived` (the server answered, and did nothing), its failure `transport`, so the
  service still goes down and the call is not uncertain.
- Whether the call that met it should be sent again on the new session -- it provably was not
  processed -- is a fork: `0003-56` sends nothing of an earlier call again.

## Acceptance

- A server restarted between two calls: the second call ends `response_received`, is not in
  `uncertain()`, and the third reconnects and is answered.
