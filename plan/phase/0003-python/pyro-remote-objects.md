# Python remote objects

Deferred out of the first milestone. This records the direction and the evidence behind it, so
that the work can be picked up without re-deriving any of it.

The problem: the interpreter the model writes into has arbitrary execution. Anything it can
read, it can print, and an output filter cannot fix that -- a token in that process is a token
the model can emit. The answer is not to redact, but to keep the token out of the process.

## The shape

The real object lives in another process. The sandbox holds a proxy and reaches it over a
socket.

Where that process runs is decided in `security.md`, not here, and it is the part that
determines whether any of this is isolation: a second process in the *same* container, under
the same user, can be signalled and inspected by the interpreter and is therefore a boundary
only in the sense that the token is not in the interpreter's own address space. The diagram
below says "other side" rather than "trusted" for that reason.

```text
   UNTRUSTED (model writes here)          OTHER SIDE (placement: security.md)
   +--------------------------+           +------------------------------+
   | runtime.services.github  |           | Daemon                       |
   |   .create_issue(...)  ---+--- socket-+-> @expose create_issue(...)  |
   |                          |           |   real client + token        |
   | no token, no client      |           |                              |
   +--------------------------+           +------------------------------+
```

The model calls what looks like a method. The call is serialized, crosses the socket, and runs
against the real object on the other side. What comes back is data.

[Pyro5](https://pyro5.readthedocs.io/en/latest/) is the candidate, and it fits the hard
constraint this phase operates under.

## Why it fits

**It is vendorable.** Pyro5 5.17 is MIT and pure Python, and its only dependency, `serpent`, is
also MIT, also pure Python, and has no dependencies of its own. No compiled extensions anywhere.
The interpreter this phase ships is a static CPython with no pip and no ability to load
third-party extension modules, so "pure Python all the way down" is not a preference here, it
is the entry requirement. Two packages clear it.

**Unix domain sockets work.** `Daemon(unixsocket="/path/to.sock")`, with URIs of the form
`PYRO:objectid@./u:/path/to.sock`. That gives filesystem permissions as the access control, no
listening TCP port, and no exposure to anything else sharing the container's network namespace.
Worth knowing: this is confirmed in `Pyro5/socketutil.py` and the URI class documentation, and
is **absent from the configuration guide**. An undocumented path in a low-activity project is
where silent regressions live, so pin the version and write integration tests against the
socket.

**Objects do not escape by accident.** Custom classes are serialized as plain dicts and are not
deserialized back into instances of the original class. An object holding a token crosses as
inert data or not at all, and one with no serializable form -- an open socket, a live API
client -- fails to cross. That failure is the desired behavior.

**Pickle is gone.** Pyro5 supports serpent (the default), json, marshal, and optionally
msgpack. serpent decodes through `ast.literal_eval`, so a payload cannot carry code the
receiver will run. Note the epistemic status: this follows from the documented design, and the
documentation does not state it as a guarantee.

## The transport

Worth knowing in detail, because it decides whether calls can be mediated -- see
`call-inspection.md`.

A message is a 40-byte big-endian header, then optional annotation chunks, then the payload.
The header is `'!4sHBBHHII16sHH'`: tag `PYRO`, protocol version, message type, serializer id,
flags, sequence number, payload length, annotations length, a correlation uuid, two reserved
bytes, and the magic `0x4dc5`. Frame length is `40 + annotations + data`, and there is no
checksum; Pyro4 had one and Pyro5 removed it.

Six message types: `CONNECT`, `CONNECTOK`, `CONNECTFAIL`, `INVOKE`, `RESULT`, `PING`. A method
call is one `INVOKE` carrying object id, method name, positional arguments, and keyword
arguments. Under `serpent` those are a 4-tuple encoded as a Python literal; under `json` they
are a dict with the keys `object`, `method`, `params`, `kwargs`, and the payload is literally
UTF-8 JSON behind the binary header.

Serializer ids are wire constants -- serpent 1, marshal 2, json 3, msgpack 4 -- and selecting
`json` is a documented, supported configuration.

Annotations are 4-character ids with byte values, sitting in cleartext between header and
payload. They are explicitly never compressed, and nothing authenticates them.

The framing is self-describing enough to parse without being a Pyro endpoint, which is what
makes `call-inspection.md` possible at all.

## What it does not give

**There is no authentication.** None. HMAC existed in Pyro4 and was removed; the compatibility
shim raises `NotImplementedError` if asked for it. The documented replacement is mutual TLS,
which is awkward over a Unix socket. Access control is therefore: what is exposed, and who can
open the socket.

**`@expose` is the entire authorization mechanism.** It is default-deny, which is the right
default -- nothing is reachable unless decorated. But there is no per-method permission model
and no notion of a caller. Whoever holds the proxy may call any exposed method with any
argument. The exposed surface is the security boundary, so it has to be designed as one:
narrow, validating, capability-shaped. `@expose` on a credentials object is not isolation, it
is a longer path to the same disclosure.

The documentation is blunt about the neighboring case, and it is this one: running under
different credentials on the same machine should be treated *"as if you're exposing your server
on the internet (even when it's only running on localhost)."*

**The client chooses the serializer.** The daemon keeps no allowlist -- it looks up whatever
id arrived, and the changelog records that the allowlist was removed deliberately once the
unsafe serializers were dropped. Nothing about the transport can be assumed from the daemon's
configuration, because the other end picks it. The same is true of the oneway flag, which is
set from client-side metadata and can be applied to any method.

**Two documented traps.** Environment variables override nearly every configuration item, so
the daemon must set its configuration in code rather than inherit it from anywhere the sandbox
can influence. And `register_dict_to_class` hooks must never be registered on the daemon -- the
documentation calls them out as arbitrary-object construction from untrusted input.

## Open questions

- What is actually exposed. A capability per service, or one object per integration? This is
  the whole security design and none of it is settled.
- Whether the transport is still a Unix socket. `security.md` prefers putting the other side in
  a sidecar container, which has its own filesystem -- so the socket needs a mount both sides
  see, or the transport becomes TCP on a private network. That choice invalidates the Unix
  socket argument above, which is the strongest practical reason for choosing Pyro5 at all.
- How proxies reach the model: prebound names in the session namespace, or something it asks
  for. This interacts with the variable inventory.
- Maintenance risk. Pyro5 self-describes as in "super low maintenance mode" -- reported bugs
  looked at, no feature work. That is survivable for a stable protocol and worth knowing before
  depending on it.

## The verdict, for 0.3

Pyro5 is right for the shape and imperfect for the destination, and that is an acceptable place
to start.

What it gets right is the core idea: a proxy on one side, real objects on the other, a
default-deny surface, and a serialization model that will not carry a live client across the
boundary even by accident. Those are the properties this design is built on, and none of them
are things to build from scratch to find out whether the shape works.

What it gets wrong is everything around authorization. No authentication, no per-call policy,
no accepted-serializer allowlist, and a transport whose framing choices the untrusted end
controls. `call-inspection.md` covers the part of that gap worth closing here, and closes it
outside Pyro rather than by patching it.

So 0.3 does not have to be the last word. A later phase may replace the transport entirely --
the exposed-surface design, the capability shape, and the call mediation all survive that,
because none of them are Pyro's. What would not survive is building the credential design
around a Pyro-specific feature, which is a reason to keep using it plainly: a daemon, exposed
methods, and a proxy, with nothing clever.

## Unverified

- None of the protocol facts above were verified against a live socket. They are read from
  the `struct` format string and the pack and unpack call sites in `Pyro5/protocol.py`, not
  from a packet capture. Confirm before depending on a byte offset.
- The 5.17 release date reads as 2026-06-19 in the releases feed, the PyPI metadata, and the
  documentation PDF; one fetch of the releases HTML page said 2023. Check before citing.
- Pyro5's true minimum Python version: the packaging metadata says 3.7, the changelog says
  3.8 and 3.9 were dropped and 3.10 is the floor. Irrelevant at 3.13, but do not quote the
  metadata as fact.
