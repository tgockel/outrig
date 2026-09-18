# Call inspection

Every call the sandbox makes across the isolation boundary is a structured message on a socket
OutRig controls. That is an opportunity, and it answers a gap the transport itself leaves:
Pyro5 has no per-call authorization at all. `@expose` decides whether a method is reachable and
nothing decides who may call it with what.

A process sitting between the two ends can supply what is missing. It can also record what
crossed, which is worth having on its own -- a session where an agent had credentials should be
able to say what it did with them.

Nothing here is in the first milestone. The transport facts it rests on are in
`pyro-remote-objects.md`; the placement question is in `security.md`.

## What is on the wire

A 40-byte big-endian header, then optional annotation chunks, then the payload. The header is
`'!4sHBBHHII16sHH'`: the tag `PYRO`, protocol version, message type, serializer id, flags,
sequence number, payload length, annotations length, a correlation uuid, two reserved bytes,
and the magic `0x4dc5`. A frame is `40 + annotations_length + data_length` bytes, and there is
**no checksum** -- Pyro4 had one and Pyro5 removed it.

Six message types exist: `CONNECT`, `CONNECTOK`, `CONNECTFAIL`, `INVOKE`, `RESULT`, `PING`. A
method call is one `INVOKE` whose payload carries four fields -- object id, method name,
positional arguments, keyword arguments.

Under the `json` serializer that payload is literally UTF-8 JSON:

```json
{"object": "<objectid>", "method": "foo", "params": [1], "kwargs": {"x": 2}}
```

Under the default `serpent` serializer it is a Python literal expression encoding the same four
fields as a tuple. Both are parseable; only one is parseable without a Python-literal reader,
which matters if the inspector is written in Rust.

## Why it has to terminate rather than observe

The obvious build is a passive tap: read frames, log them, forward the bytes. That is
observability, and it is not a boundary, for one reason above all. (`observability.md` builds
exactly that tap elsewhere, deliberately and with no enforcement claim; the distinction this
paragraph draws is what keeps the two from being confused for each other.)

**The client chooses the serializer.** The daemon has no allowlist -- it looks up whatever id
arrived, and Pyro's own changelog records that the allowlist was removed deliberately once the
unsafe serializers were dropped. So an inspector that understands JSON can be bypassed by a
client that sends serpent. The same is true of several other framing choices the client
controls.

A mediating proxy that terminates both legs and enforces a strict subset does not have that
problem. It decides what it accepts, and anything outside it is refused rather than forwarded
unexamined:

- one serializer id, refused otherwise;
- `FLAGS_KEEPSERIALIZED` refused outright, because it moves the object id and method name out
  of the payload and into a marshal-encoded annotation where a payload-only reader will not
  find them;
- batches expanded into their individual calls before any decision is made;
- compression refused, or inflated before inspection.

This is the same shape `crates/outrig/src/mcp_proxy.rs` already has for MCP: terminate,
understand, re-expose. It is not a new pattern for this codebase.

## What breaks a naive implementation

Each of these is a real feature of the protocol, not an edge case, and each one defeats an
inspector that assumes one message is one call on one object.

**A batch is N calls in one message.** The method name on the wire is the literal `<batch>` and
the payload carries a list of `(method, args, kwargs)`. Top-level `kwargs` is `None`. An
inspector that counts messages under-counts calls and authorizes the wrong thing.

**A oneway call has no response.** The daemon sends nothing back and suppresses errors for it.
So there is no channel on which to report a denial -- a refused oneway call is indistinguishable
from an accepted one, from the client's side. Worse, *which* methods are oneway is decided
client-side from metadata, so the flag can be set on anything.

**Streaming results stop naming their object.** A call returning an iterator replies with an
exception carrying a `STRM` annotation and a stream id. Every subsequent item is an ordinary
`INVOKE` of `get_next_stream_item` against `Pyro.Daemon`, carrying the stream id and nothing
else. Per-object authorization requires correlating those ids back to the call that created
them, and streams can outlive the connection that made them.

**Pings are not serialized.** `MSG_PING` carries serializer id 42, which is not a registered
serializer. An inspector that looks up the serializer unconditionally raises on the first ping.

**Annotations are readable and writable by anyone.** They sit in cleartext between the header
and the payload and are explicitly never compressed. That makes them a convenient place for the
inspector to stamp a correlation id -- and equally convenient for the client to forge one.
Nothing authenticates them, so they are useful for tracing and useless for trust.

## What it would record

OutRig already writes a per-session network audit at `network.jsonl`, using a bounded queue
that applies backpressure rather than dropping records, and a record schema borrowed from a
well-known format rather than invented. A call log belongs beside it, on the same terms: one
line per call, the object and method, a bounded rendering of the arguments, the decision, and
the session and container it belongs to.

Argument rendering is the part that needs care. Arguments are the most useful field in the log
and the most likely to contain a secret -- which is the whole reason the boundary exists. A
call log that faithfully records every argument reintroduces the disclosure it was built to
prevent, into a file that outlives the session.

## Open questions

- Whether the policy vocabulary is the existing `Allow` / `Deny` / `Audit`, which would let an
  operator run in audit mode first and see what an agent actually calls before constraining it.
- What is authorized against: the method, the object, or the pair. Per-object is the weaker
  claim and the easier build.
- What a denial looks like to generated Python. An exception raised from the proxy is the
  obvious answer and has no path for a oneway call.
- Whether arguments are logged, redacted, hashed, or recorded only as a shape. The useful
  answer is probably per-parameter and declared alongside the exposed surface.
- Whether the inspector is a separate process or part of whatever already bridges the boundary.
  If the trusted side is a sidecar, something is already forwarding.
- Whether this is worth doing at all before there is a second thing behind the boundary. One
  service with three methods does not need a policy engine.
