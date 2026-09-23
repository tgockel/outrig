# 0003-13 -- A session records what its agents did, by category

## Context

`doc/usage/sessions.md` opens by saying sessions exist "so you can go back and inspect what
happened", and then records the container, its connections, and the servers' stderr. The agent is
absent, and the same page still carries a TODO admitting the conversation is not persisted at all.

`observability.md` answers that, and its three settled decisions bound the work: the session
directory is the interface so nothing listens, reading is read-only, and nothing is captured
richer than its category allows. That last one replaced a single rule -- "the record holds exactly
what the model saw" -- which the event set contradicted, since a model can be told a message
exists without its body entering its context.

Most of it is already on a wire. Inter-agent messages are the exception and the one cost
co-hosting adds: they never reach the host, so the interpreter emits an observation because
someone is watching rather than because delivery requires it.

The sink is not new work either. `AuditSink` in `network.rs` is generic in everything but the
record type -- bounded queue with backpressure, reserve-then-send enqueue, an exclusive lock,
rollback to the last whole line, poisoning, bounded loss accounting. This is its second caller.

## Goal

A session writes what its agents did, in categories whose capture rules are stated, on the same
terms as the audit log that already exists.

## Deliverables

- `<session_dir>/logs/events.jsonl`, one append-only stream. One stream rather than a file per
  subject, because an agent's timeline is a causal chain and recovering the order by
  timestamp-joining files fails exactly when it is needed.
- **`AuditSink` extracted** rather than copied, since this is its second caller and
  `plan/next/dynamic-resource-mounts.md` proposes a third.
- The CloudEvents envelope, with the spec's constraint honored: attribute names are lower-case
  alphanumeric, so the flat `outrig.session_id` style `network.jsonl` uses is not a legal
  extension attribute. Everything OutRig-specific goes in `data`; `type` takes the `org.outrig.`
  prefix already used for OCI and container labels.
- **The schema for every event, and emission for the producers that exist by now**: executions
  and their results, inventories, messages, `model.round.completed` with the usage the loop
  currently discards, tool-result truncation, the effective `max-tokens` ceiling, agent lifecycle,
  interrupts and cancels, `MemoryError`, the unattributed-output bucket, and the per-call view
  manifest. Retry and failover hops get their schema here and their emission in `0003-15`, which
  is the task that adds them -- defining a schema before its producer exists is fine, and
  requiring the integration is not.
- **A configuration surface, budgeted honestly.** `observability.md` says nothing new becomes
  public, and an opt-in TOML block touches `Config`, whose fields are public and whose parser
  denies unknown keys. Event types and the writer stay private; the config key is a deliberate,
  separately stated addition rather than something the "one entry point" budget silently absorbs.
- **The three categories, each with its rule**: model view, execution diagnostics, integration
  audit.
- Opt-in, mirroring `[network].mode`, defaulting off.
- **`model.round.completed` documented as "yielded control"**, not "all work succeeded".

## Acceptance

- A session with the mode on writes a parseable stream whose exact attribute names are asserted,
  in the shape `network.rs:6113` asserts Zeek's.
- **Token usage appears**, read from `response.usage` and `completion_calls` -- which the loop
  currently drops on the floor despite already calling `.extended_details()`.
- A promotion and the per-call manifest both appear, and the manifest reconstructs what the
  provider received.
- With the mode off, no file is created.
- A record is written before teardown returns, matching the durability contract
  `sessions.md:178` states for `network.jsonl`.
- The extracted sink keeps its existing behavior: the `network.jsonl` tests pass unchanged,
  **including its bounded-shutdown contract** -- records are written or their loss is explicitly
  accounted for, and a deadline can prevent a complete flush. An overrun test, and losses reported
  as agent-event losses rather than through a network-shaped error.
- **The file's permissions are set deliberately.** This stream carries message bodies that may
  never have been printed to the model, which is a different sensitivity from a connection log.
- `cargo test --workspace`, `cargo clippy --all-targets`, `cargo fmt --check` pass.

## Design forks

1. **Whether `events.jsonl` is reachable through `outrig logs` -- Recommended: no**, matching
   `network.jsonl`, which is deliberately not selectable by server name.
2. **Whether it should eventually default on -- defer.** It is the record a session was supposed
   to be, which argues yes; it is the conversation, which argues for deciding deliberately.

## Dependencies

- **Hard: 0003-05.** There is no session driving agents until the command exists.
- **Hard: 0003-12.** Manifest reconstruction is an unconditional acceptance criterion here, so its
  producer is a hard prerequisite rather than a soft one.

## See also

- `plan/phase/0003-python/observability.md` -- the categories, the envelope, and what is emitted.
- `crates/outrig/src/network.rs:1218-1799` -- `AuditSink` and `audit_writer`, the sink to extract.
- `doc/usage/sessions.md` -- the layout this extends, and the TODO it retires.
