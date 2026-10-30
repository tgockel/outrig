# A model's context window from rig's catalog before asking the provider

## Context

A call is held to a window from one of three sources (`WindowSource`): the model row's
`context-window`, what the provider reported when the session asked (`crates/outrig/src/agent/
window.rs`: Anthropic's `GET /v1/models/{id}`, a LiteLLM gateway's `/model/info`), or the
assumed 128,000 tokens. rig 0.45 ships a catalog (`rig::catalog::Catalog::builtin()`) whose
`ModelSpec` carries `context_window: Option<u32>` beside the `max_output_tokens` the Anthropic
ceiling already comes from, resolved by `Catalog::resolve("<vendor>/<identifier>")`, dated
snapshots included. The upgrade to 0.45 (`chore/rig-0.45`) left the window's sources as they
were.

## Shape

- A new `WindowSource::Catalog` between `Configured` and `Provider`: the catalog's
  `context_window` for an identifier it lists, so a session on a listed model asks the provider
  nothing and a proxy that reports no window is no longer held to the assumed one.
- `window.rs`'s GET stays for identifiers the catalog does not list: a proxy's own names, and
  models newer than the pinned rig release.
- The `model.call` manifest's `budget.window_source` gains the value; `doc/reference/events.md`
  names it.

## Acceptance

- A session on `claude-sonnet-4-6` with no `context-window` records `window_source: "catalog"`
  and the catalog's window, and sends no models request.
- A session on an identifier the catalog does not list behaves as today.
