# Exact capture of the final provider request

## Shipped

Each model call records an ordered manifest of the canonical turn ids it carried, plus the
selection metadata, and content is reconstructed from the store rather than duplicated
(`history.md`, "Promotion"). `0003-15` gives each model event a logical call id and a provider
attempt id, failed attempts included, with the request's settings and nullable usage, so
aggregates come from unique attempts and parent and child inclusive totals are never summed.
What the manifest does not hold is the request as the adapter serialized it: the orientation,
the tool definitions, the settings, and the transformations rig and the adapter apply --
same-role merges, placeholder results, the shape a gateway requires. Two calls with the same
manifest can have sent different bytes.

## Alternative

Opt-in capture of the final client request at serialization, keyed by the logical call id and
the provider attempt id: the adapter's name and version, the bytes as sent or a canonical form
of them, and the identity of the client request. Credentials are excluded, and where redaction
removes anything else the record is marked inexact, so a reader never takes a redacted record
for the request. It is a capture category of its own under `observability.md`'s rules, off by
default and content-rich, so the owner enables it and decides where it is kept. An exact client
record still does not prove what a remote gateway processed; it says what left the process.

## Evaluation

Experiments that need exact reconstruction: attributing a measured difference to one harness
mechanism rather than to several coupled choices, which the manifest cannot do when the adapter
or its version changed between runs. The capture is worth building when such an experiment is
planned and the manifest plus the attempt record cannot answer it. Its cost is the bytes per
attempt and one more place a conversation body is stored.

## When

After `0003-15`'s attempt identity, which the capture keys on, and not before an experiment asks
for it.
