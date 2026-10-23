# Config Reference

outrig reads two TOML files:

- **Global config** -- user/machine-level. Default location: `~/.outrig/config.toml` (or
  `<XDG_CONFIG_HOME>/outrig/config.toml` if `XDG_CONFIG_HOME` is set to an absolute path). Holds
  `[providers.<name>]` and (typically) `[models.<name>]` since those reference API keys and
  model identifiers that belong to the user, not to any one repo. It is also where
  [`[repo]`](#repo) lives, which says which host environment variables a repo config may
  reference.
- **Repo config** at `.agents/outrig/config.toml` -- repository-level, committed to source
  control. Holds `[workspace]`, `[images.<name>]`, `[agents.<name>]`, and any repo-specific
  models, which may name the global providers. A repo config may not declare a provider, since
  every provider carries an `api-key`: the file sits inside the read-write workspace, so an agent
  could edit it for the next session, and a key names a secret in the operator's environment.
  For the same reason it may reference a host variable in `build-args` or an MCP `env` only if
  the global config lists it in [`[repo]`](#repo) `env-allow`.

Both files use the same schema. Names declared in either are visible everywhere; if a name
appears in both files, the **repo entry wins**, except where
[Resolution](#resolution-which-file-wins) says the global one stands: `[network]`'s policy and
`[events] mode`. Outrig keys are **kebab-case**; only inner-map
keys whose values map to environment variables (Dockerfile build-args, MCP `env` blocks) keep
their as-written form. A key outrig does not know is ignored with a warning rather than refused;
see [What a load sets aside](#what-a-load-sets-aside).

## Top level

```toml
# repo config (.agents/outrig/config.toml):
default-image = "coding"
default-agent = "coding"

# global config (~/.outrig/config.toml):
default-model         = "fast"
session-root          = "/var/lib/outrig/sessions"    # optional; defaults to XDG data dir
tool-call-max         = 100                           # optional; defaults to 50
tool-result-max       = 262144                        # optional; defaults to 256 KiB
subagent-depth-max    = 3                             # optional; defaults to 3
subagent-width-max    = 8                             # optional; defaults to 8
children-max          = 64                            # optional; defaults to 64
model-concurrency-max = 8                             # optional; defaults to 8
requests-max          = 256                           # optional; defaults to 256
token-budget          = 20000000                      # optional; no cap when unset
retry-budget-secs     = 600                           # optional; defaults to 600
mcp-call-timeout-secs = 600                           # optional; defaults to 600

[network]
mode = "default"                                      # optional: default, audit, or filter
default = "deny"                                      # optional for filter mode
allow = ["github.com:443", "*.npmjs.org"]             # optional; global only
deny  = ["*:22"]                                      # optional; global only

[events]
mode = "off"                                          # optional: off or record

[host]
grace-secs = 5                                        # optional; defaults to 5

[repo]
env-allow = ["GITHUB_TOKEN"]                          # optional; global only
```

| Key                     | Type    | Required               | Where  | Description               |
|-------------------------|---------|------------------------|--------|---------------------------|
| `default-image`         | string  | no                     | repo   | Default `--image`.        |
| `default-agent`         | string  | no                     | repo   | Default `--agent`.        |
| `default-model`         | string  | if agent omits `model` | global | Fallback model name.      |
| `session-root`          | path    | no                     | global | Sessions root dir.        |
| `tool-call-max`         | integer | no                     | global | Per-turn tool-call max.   |
| `tool-result-max`       | integer | no                     | global | Per-tool-result byte max. |
| `subagent-depth-max`    | integer | no                     | global | Max subagent nesting.     |
| `subagent-width-max`    | integer | no                     | global | Max live subagents/agent. |
| `children-max`          | integer | no                     | global | Max children (`run-new`). |
| `model-concurrency-max` | integer | no                     | global | Model requests in flight. |
| `requests-max`          | integer | no                     | global | Unsettled requests/child. |
| `token-budget`          | integer | no                     | global | Session token cap.        |
| `retry-budget-secs`     | integer | no                     | global | LLM retry budget (secs).  |
| `mcp-call-timeout-secs` | integer | no                     | either | MCP tools/call deadline.  |
| `network.mode`          | string  | no                     | either | Network mode.             |
| `network.default`       | string  | no                     | global | Filter fallback action.   |
| `network.allow`         | array   | no                     | global | Filter allow entries.     |
| `network.deny`          | array   | no                     | global | Filter deny entries.      |
| `events.mode`           | string  | no                     | either | Agent event log.          |
| `host.grace-secs`       | integer | no                     | either | Host program grace (s).   |
| `repo.env-allow`        | array   | no                     | global | Variables a repo may use. |

`default-agent` is optional. With neither `--agent` nor `default-agent`, `outrig run` starts
with no agent: no preamble is sent, every knob comes from the top level, and the image cascade
is `--image`, then `default-image`, then outrig's
[built-in default](#the-built-in-default-image-config). A model must still resolve from
`--model` or `default-model` -- that is the one thing an agentless session cannot do without.

`default-image` and `default-agent` belong in the repo config -- image-configs and agents are
project-scoped. `default-model`, `tool-call-max`, and the subagent limits belong in the global
config since they're user/machine-level. `tool-result-max` usually belongs there too, although
repo or agent config can tighten it for a noisy project.
`retry-budget-secs` is a default for every remote provider; a `[providers.<name>]` row that
sets its own overrides it, which is usually the better place since rate limits are a property
of the endpoint.
`mcp-call-timeout-secs` is how long an MCP `tools/call` may run before outrig cancels it at the
server and hands the model a timeout in place of a result -- 1 to 3600, default 600. It is the
default for every server in the session, sidecars and image-label servers included; a server's
own [`call-timeout-secs`](#imagesnamemcp) overrides it, and in either file the repo value wins.
The server is sent `notifications/cancelled`, which it may or may not act on.
`session-root` is global-only: it chooses where this machine writes session records, and so
where `outrig clean` and `outrig discard` delete. A repo config that sets it is rejected at
load.
`[network].mode` can live in either file; when both set it, the repo value wins for that repo.
A `[network]` table that declares no `mode` declares nothing, so it inherits. Network policy
keys (`default`, `allow`, and `deny`) are global-only because they describe the machine's
egress policy, not a project preference; a repo config that sets one is rejected at load.
`[events].mode` can live in either file too, but when the global config sets it, that value
stands. `[host].grace-secs` can live in either file, and the repo value wins, as for the
top-level keys.
`[repo].env-allow` is global-only too: it says which host variables a repo config's `${VAR}`
references, and an image label's, may name, so a repo file cannot answer for itself. A repo
config that declares it is rejected at load. See [`[repo]`](#repo).

`session-root` defaults to `<XDG_DATA_HOME>/outrig/sessions/` (typically
`~/.local/share/outrig/sessions/`). A relative value resolves against the global config's
directory, and a leading `~` is your home directory -- see [path resolution](#path-resolution).
Since only the global config can set it, `outrig run` and the session commands (`ls`, `logs`,
`discard`, `clean`) find the same root from any directory, inside a repo or not. The CLI flag
`--session-root <path>` overrides both the config value and the default; `--session-dir <path>`
(on `outrig run`/`logs`/`discard`) instead points at one specific session directory. See
[Sessions](https://tgockel.github.io/outrig/usage/sessions.html).

`tool-call-max` is the default maximum number of tool calls in one user turn. The compiled-in
default is `50`; config may set any value from `1` through `2000`.
`[agents.<name>].tool-call-max` overrides the top-level value for one agent, and
`outrig run --max-tool-calls <n>` overrides both for one invocation.

`tool-result-max` is the default maximum byte size for one MCP tool result before it is added to
the LLM-visible conversation history. The compiled-in default is `262144` bytes (256 KiB);
config may set any value from `1024` through `16777216` bytes.
`[agents.<name>].tool-result-max` overrides the top-level value for one agent, and
`outrig run --max-tool-result-bytes <n>` overrides both for one invocation. Results larger than
the max are truncated at a UTF-8 boundary and end with an `[outrig: tool result truncated]`
marker that reports the original size and max. The cap applies to the *rendered* text --
the single string the model is given, which is what a non-text content block contributes a
bracketed placeholder to -- not to the server's content blocks, which reach a client
connected to `outrig mcp` whole.

`subagent-depth-max` bounds how deeply subagents may nest. The primary agent you talk to is the
root at depth 1; an agent at depth `D` may launch subagents (which live at depth `D+1`) only
while `D` is under the limit. So `1` disables subagents entirely, `2` allows a single layer, and
the default `3` allows two. The compiled-in default is `3`; config may set any value from `1`
through `16`. `[agents.<name>].subagent-depth-max` overrides the top-level value for one agent.
The separate `[agents.<name>].subagents` toggle still applies: `false` withholds the launch
tools regardless of depth.

`subagent-width-max` bounds how many live subagents one launching agent may hold at once. The
compiled-in default is `8`; config may set any value from `1` through `16`.
`[agents.<name>].subagent-width-max` overrides the top-level value for one agent. The cap is per
launching agent, so a subagent's own budget is independent of its parent's. A subagent stays live
after it finishes, so `outrig__subagent_release` is what frees a slot.

`children-max`, `model-concurrency-max`, `requests-max`, and `token-budget` bound the child agents
`outrig run-new`'s Python agents launch with `runtime.spawn`, and that session's model calls.
`outrig run` reads none of them; its subagents keep `subagent-width-max`, which `run-new` does not
read. `subagent-depth-max` applies to both: a `run-new` agent at the limit cannot spawn. Each of
the four can be set at the top level or in an `[agents.<name>]` block, whose value wins for a
session that agent runs, and each is read when the session starts.

`children-max` is how many children one session holds at once -- launching, working, idle, or
wedged, since each is a thread and its memory in the interpreter. The compiled-in default is
`64`; config may set any value from `1` through `1024`. A launch past it fails at once with
`outrig.AgentLimitReached` rather than waiting, and releasing a child frees its place once its
kernel is gone.

`model-concurrency-max` is how many model requests a session has in flight at once, every agent's
together, the main agent's included. The compiled-in default is `8`; config may set any value
from `1` through `128`. A request past it waits its turn, in order. A request holds its place from
when it is sent until what its response used has been counted, and never while an agent's code
runs, so a parent waiting on a child holds none.

`requests-max` is how many requests one child may hold unsettled: submissions running or queued,
and requests unread or received and not yet answered. The compiled-in default is `256`; config
may set any value from `1` through `4096`. A `submit()` or `request()` past it fails at once with
`outrig.AgentLimitReached`, and an answer, a cancel, or the child's release frees a place.

`token-budget` caps the tokens a `run-new` session's model calls spend in all, every agent's
together and the main agent's included, counted as the `total_tokens` each provider reports.
Unset, nothing caps them; config may set any value from `1`. Once it is spent, no agent makes
another model call: a child's open work fails with `outrig.BudgetExhausted`, and the main agent's
round ends saying the budget is spent. Each request is checked as it is sent, once it has its
place, so a retry of one whose response spent the budget is not sent. Requests already in flight
when it runs out are counted as they finish, so the total can pass the budget by what they used.

## `[network]`

Network interception is disabled by default:

```toml
[network]
mode = "default"
```

Accepted modes:

- `default`: use Podman's configured default networking, do not install the interceptor, and do
  not write `logs/network.jsonl`.
- `audit`: allow all outbound session-container traffic, short of the per-container connection
  limit below, but write Zeek `conn.log`-style records to `<session_dir>/logs/network.jsonl`:
  one per TCP connection and one per name lookup.
- `filter`: install the same interceptor as audit mode, write the same audit log, and enforce
  global allow/deny policy before opening upstream TCP connections and before answering a name
  lookup (see [Name lookups](#name-lookups)). Traffic the interceptor cannot carry -- UDP to any
  port but 53, ICMP, anything else -- follows `default` directly.

Audit and filter mode require host `nft` and `nsenter` plus permission to enter the rootless podman
container's user/network namespaces. It rewrites the session container's `/etc/resolv.conf` to send
DNS to the per-session in-namespace DNS listener, installs nftables redirection for outbound TCP and
port 53 over both IPv4 and IPv6, and removes the nftables table during teardown. A lookup a tool
sends to some other resolver address, such as `dig @8.8.8.8`, is redirected to the same listener and
answered from the address it was sent to. That holds over TCP as well as UDP: the stub's retry after
a truncated answer, `dig +tcp`, and `use-vc` are answered as a UDP lookup is, forwarded to the
host's resolvers over TCP. The listener forwards each lookup on its own, up to 64 at once per
container, so one name a host resolver is slow to answer does not hold up the container's other
lookups. TCP connections are carried up to 256 at once per container, in either mode: each holds two
of outrig's open files, and the limit keeps one container from taking all of them. A connection past
it is closed as soon as it is accepted and recorded with `outrig.action = "deny"` and `outrig.rule =
"connection-limit"`. For the same reason `outrig` raises its own soft open-file limit to the hard
limit when it starts. A connection that reaches the interceptor's own listener without having been
redirected to it, such as a process in the container connecting straight to the listener's port, is
closed as soon as it is accepted and recorded with `outrig.action = "deny"` and `outrig.rule =
"not-redirected"`, in either mode. If either mode is requested and setup fails, the session fails
before MCP servers launch.

Either mode refuses a container that holds `NET_ADMIN`. `CAP_NET_ADMIN` in a container's own
network namespace is control of its nftables, so a root process in that container could delete the
redirect table and leave no record. A container holds it when its `cap-add` lists `NET_ADMIN` --
`CAP_NET_ADMIN` and `net_admin` mean the same -- or lists `ALL` without `NET_ADMIN` in `cap-drop`;
no `capability-profile` adds it. The refusal applies whether the mode came from the config or from
`--network`, and the error names `cap-add`. The primary fails the session before MCP servers
launch, a sidecar follows its `on-failure` (`warn` drops it, `abort` ends the session), and
`Outrig::launch` and `Outrig::add_sidecar` return the error. `SYS_ADMIN` is not refused: it does
not let a container change its netfilter, so the nested-runtime recipe in
[Containers](../concepts/containers.md) runs under either mode. A container that needs
`NET_ADMIN` runs under `mode = "default"`, or `--network default`, which installs nothing to
remove. A container outrig did not start is not inspected.

Lookups are forwarded to the host's own resolvers: the `nameserver` entries in the host's
`/etc/resolv.conf`, in order. Forwarding happens in the host's network namespace, so a loopback
entry answers just as it does for the host. On a host using systemd-resolved, that entry is its
stub, `127.0.0.53`, and resolved routes each container lookup as it routes the host's own: a
name in a VPN's routing domain goes to the VPN's server. A bare single-label name such as `nas`
can reach resolved as it is, since the resolver interception installs names none of the host's
search domains, and resolved does not look such a name up over DNS by default -- name such a
host in full. When `/etc/resolv.conf` is missing or names no nameserver, the listener uses the
upstream servers systemd-resolved lists in `/run/systemd/resolve/resolv.conf` instead. If
neither file names a resolver, setup fails rather than choosing one on the host's behalf.

Each lookup goes to those resolvers one at a time until one answers. One the host cannot reach at
all, such as an IPv6 server on a kernel booted without IPv6, is passed over at once, and so is one
that refuses the lookup; one that stays silent is passed over after 5 seconds. A lookup none of
them answers is answered SERVFAIL rather than not at all, so the tool that asked is told the
lookup failed -- at once, when every resolver refused -- and `outrig` warns about the first such
lookup in each container.

Filter policy lives in the global config only:

```toml
[network]
mode    = "filter"
default = "deny"              # optional; absent means "deny" in filter mode
allow   = ["github.com:443", "*.npmjs.org", "10.0.0.0/8"]
deny    = ["*:22", { host = "169.254.169.254", port = 80 }]
```

`allow` and `deny` entries can be compact strings or inline tables. The string `"host"` maps
to `{ host = "host" }`; `"host:443"` maps to `{ host = "host", port = 443 }`; `"*:22"` maps
to `{ host = "*", port = 22 }`; `"[2001:db8::1]:443"` maps to an IPv6 host plus port; and
CIDRs such as `"10.0.0.0/8"` or `"2001:db8::/32"` match IP destinations. Inline tables use
`{ host = "...", port = 443 }`, with `port` optional.

Filter evaluation checks `deny` entries first, then `allow` entries, then `default`. Denied
connections are closed immediately and still write an audit record with
`outrig.action = "deny"`, `outrig.rule`, and zero byte counts. `mode = "filter"` requires at
least one `allow` or `deny` entry, even when `default = "allow"`. What a hostname entry can
match, and so stop, is bounded: see [What a hostname rule matches](#what-a-hostname-rule-matches).

`allow` and `deny` entries match TCP connections, which the interceptor carries and can name a host
for. Anything else the container sends -- UDP to any port but 53, which is how QUIC and so HTTP/3
travel, ICMP, any other transport -- matches no entry and takes `default` directly. Under `default =
"deny"` the kernel drops it, by a rule in the same nftables table: the sending tool fails at once
with "Operation not permitted" rather than reaching the network. Such a datagram never reaches the
interceptor, so it writes no `network.jsonl` record. Under `default = "allow"`, and in audit mode,
it leaves by podman's default route, unrecorded. Name lookups, which are over UDP or TCP port 53,
are decided by the name asked for instead; see below. Traffic between processes in the container
over loopback is carried either way.

### Name lookups

A name the container looks up travels to whoever serves its zone, so a lookup is a way out of the
container like a connection is. The interceptor's DNS listener decides each one by policy and
records it. A lookup carries no port, so an entry's port counts neither for nor against it.

- A `deny` entry that names a host and has no port refuses the lookups it matches, under either
  default. A `deny` entry with a port, or one that is an address or a CIDR, refuses none.
- Under `default = "allow"`, every other lookup is answered.
- Under `default = "deny"`, a lookup is answered only when an `allow` hostname entry matches the
  name, whatever its port: `github.com:443` opens the lookup of `github.com`. `*.npmjs.org` does
  not match `npmjs.org`, so list the apex too. An `allow` entry that is an address or a CIDR
  opens no lookup -- it grants connections to that address, and a name is not one -- unless it
  matches every address (`*`, `0.0.0.0/0`, `::/0`), which opens every lookup.
- A query that is not a plain question for one name is not decided by its name: an answer, an
  opcode other than QUERY, extra records beyond one EDNS record, a compressed name, or one
  longer than 253 bytes. It is refused wherever some lookup could be -- under `default = "deny"`,
  and wherever a `deny` entry names a host without a port -- and answered as before elsewhere.

A refused lookup is answered REFUSED without any resolver being asked, so nothing about the name
leaves the host. The tool that asked sees the lookup fail: glibc tries again before giving up,
and musl waits out its own timeout. Under `default = "deny"` a reverse lookup, or the lookup of
a name no `allow` entry names, fails the same way.

Every lookup writes a `network.jsonl` record, answered or refused, before the query is sent to a
resolver. It is a record for service `dns`, `udp` or `tcp` as the lookup was made, its `outrig.host`
is the name asked for (with `outrig.host_source` `asserted`, since the container chose it), and
`outrig.action` and `outrig.rule` say what was decided and by which entry; a query that is not a
plain question is refused under `outrig.rule` `dns-unparsed`. A zone transfer (`AXFR` or `IXFR`)
over TCP is refused under `dns-transfer`: its answer is several messages, which the listener does
not carry. Its other fields are those of a connection's record.

An allowed name can still carry data. `allow = ["*.example.com"]` lets the container look up
`<data>.example.com`, which reaches the name servers of example.com, and the same entry grants
connections to every host under it, so name only zones you would trust with either. An EDNS
record a query carries is forwarded to the host's resolver whole. A lookup is decided by its
name alone, over TCP as over UDP, so an entry for port 53 -- `deny = ["*:53"]` -- does not govern
it; no connection to port 53 is made that a destination could decide.

> **Changed in 0.2.2.** Before this release every lookup was answered, whatever the policy.
> Under `default = "deny"`, a name that resolved only because it fell in an allowed address or
> CIDR now needs an `allow` entry of its own, or the container has to connect by address. Under
> `default = "allow"`, a name a hostname `deny` entry matches is no longer resolved; its
> connection used to be denied, and now the denial is recorded as the lookup.

### What a hostname rule matches

A hostname entry in `allow` grants only against a destination outrig itself resolved to that
name. The evidence is the container's own DNS lookup: the interceptor answers the query,
validates the response, and binds the addresses that response attributes to the queried name.
A binding lasts as long as the answering record's TTL, clamped to between 30 seconds and one
hour -- the floor keeps a TTL-0 answer usable by the connection that prompted it, and the
ceiling stops a generous TTL from pinning a name for the whole session. Bindings belong to the
container that earned them and are shared with nothing else in the session.

The name a client writes into a TLS `ClientHello` or an HTTP `Host:` header is a claim, not
evidence. It can match a `deny` entry -- a forged name there costs that client only its own
connection -- but it never matches an `allow` entry. Under the policy below, a container that
connects straight to `203.0.113.66:443` and announces `allowed.example` is denied, and the same
container is allowed once it has resolved `allowed.example` to that address through the
interceptor:

```toml
[network]
mode    = "filter"
default = "deny"
allow   = ["allowed.example:443"]
```

A hostname `allow` entry with no binding simply does not match. Evaluation continues with the
remaining entries and then `default`; the entry is never turned into a deny, so
`default = "allow"` keeps meaning what it says.

Consequences worth planning for:

- A container that reaches an address without asking the interceptor's resolver -- a baked
  `/etc/hosts` entry, a hardcoded address, DNS over HTTPS -- earns no binding, so
  hostname `allow` entries do not cover it. Address and CIDR entries still do.
- `host.containers.internal` and `host.docker.internal`, which reach the host, are such
  `/etc/hosts` entries, so an entry naming them never matches; name their address instead. Under
  podman 5's default rootless network, pasta, that is `169.254.1.2`, an address that means the
  host only inside the container -- so `"169.254.1.2:8080"` allows one host port, and a deny
  covering `169.254.0.0/16` covers the host too. Under podman 4 it is the host's own address.
  Either way the connection reaches what it would with interception off, which excludes a host
  service listening only on loopback, and its record names the address the container asked for.
- A host glob with no letter in it (`*`, `10.0.*`, `192.168.*`) describes addresses rather than
  names: it is matched against the destination address and never against a resolved name, so a
  registrable hostname that happens to match it (`10.0.attacker.example` against `10.0.*`)
  grants nothing. `allow = ["*"]` and `deny = ["*:22"]` mean exactly what they always did.
- A hostname `deny` entry is advisory against a client that does not cooperate. A container that
  reaches an address without a lookup here, and announces no name, leaves it nothing to match:
  under `default = "allow"`, `deny = ["evil.example"]` lets a connection to that address by
  literal IP through. A portless entry also refuses the lookup (see
  [Name lookups](#name-lookups)), which shuts the interceptor's own resolver and no other path.
  Only an address, a CIDR, an address glob (`203.0.113.0/24`, `*:22`), or `default = "deny"`
  binds such a client.
- A hostname `allow` entry grants the address its name resolved to, not the site. Under the
  policy above, a container that has looked up `allowed.example` can connect to that address and
  announce `other.example`, a name served from the same address by a CDN or shared host, and is
  allowed for as long as the binding lasts. Only `deny` entries weigh the announced name; nothing
  checks it against the entry that earned the allow.

Client-asserted names are still recorded. An audit record carries the best known name in
`outrig.host` and says where it came from in `outrig.host_source`, which is `resolved` for a
name the interceptor validated and `asserted` for one the client claimed.

> **Changed in 0.2.0.** Before this release a client-supplied `Host:` or SNI satisfied a
> hostname `allow` entry on its own, so an allowlist permitted more than it appeared to. The
> same allowlist is narrower now. A container whose name resolution does not go through the
> interceptor may need address or CIDR entries to keep working.

`outrig run --network default|audit|filter` and `outrig mcp --network default|audit|filter`
override this setting for one fresh session. `--network audit` and `--network filter` are
rejected with `outrig mcp --attach` because borrowed containers are not retrofitted with a new
interceptor.

## `[events]`

Whether `outrig run-new` records what its agent did. Off by default:

```toml
[events]
mode = "off"
```

- `off`: record nothing, and write no file.
- `record`: write every event the agent produces to `<session_dir>/logs/events.jsonl`, readable by
  its owner only. The
  [event log reference](https://tgockel.github.io/outrig/reference/events.html) lists what that
  holds -- including what you type to the agent and what it sends back, and each model call's
  conversation.

The mode can live in either file. When the global config sets it, that value stands; a repo's
applies only when the global config says nothing. The record is your own of your own session,
so a cloned project can neither start recording it nor stop it. `outrig run` reads the key and
records nothing, and with `record` set it says so at startup. There is no command-line flag for
it yet.

## `[host]`

How `outrig run <script.py>` treats the Python program it runs on the host:

```toml
[host]
grace-secs = 5
```

- `grace-secs`: how many seconds the program has to end on its own after the first `SIGINT` or
  `SIGTERM` that `outrig` forwards to it. Past it, or at a second signal, `outrig` stops the
  program's process group: `SIGTERM`, then `SIGKILL` five seconds later. Time spent shutting down
  a session the program asked to shut down does not count. `0` stops the group at the first
  signal; at most `3600`. `outrig run --grace <secs>` overrides it for one invocation. See
  [Usage -> outrig run](https://tgockel.github.io/outrig/usage/run.html#signals-and-the-grace).

## `[repo]`

Global config only. `[repo]` says what a repo may take from your machine, through its config or
through the labels of the images it builds or names. Today that is one key, `env-allow`:

```toml
[repo]
env-allow = ["GITHUB_TOKEN", "HTTPS_PROXY"]    # optional; default: none
```

A `${VAR}` reference in a repo config's `build-args` or in an MCP server's `env` is resolved from
the host environment, and the value goes to code the repo controls: a build of its own
Dockerfile, or a server command it chose. The repo config sits inside the read-write workspace,
so it may be committed hostile, or edited by the agent for the next session. A repo config file
may therefore name only the variables `env-allow` lists, and the list is empty by default:

- **A reference to a variable that is not listed is refused when the config loads** -- by
  `outrig run`, `outrig run-new`, `outrig mcp`, and `outrig build` alike, before anything is
  built or started, and before the variable is read. The error names the image, the build-arg or
  the server and its `env` key, the variable, and the file that declared it, and says to list the
  variable here.
- **A literal needs no entry.** Neither does anything you wrote or typed yourself: `--env` values
  on the command line, and the `build-args` and `env` of images declared in the global config,
  which may reference any variable.
- **Every image a repo config file declares is checked**, used or not, because loading does not
  know which one a session will select. A `--config` file is a repo config for this rule, so a
  profile of your own loaded that way needs its variables listed here too.
- **An image's label is held to it too.** The `org.outrig.mcp` label is not a config file, but it
  is text the image's author wrote -- a Dockerfile in the repo, which the agent can edit, or an
  image the repo names -- and a `${VAR}` in a server's `env` there is resolved from your
  environment just the same. Once the label has merged in, a reference to a variable that is not
  listed is refused when the session starts, by `outrig run`, `outrig mcp`, and
  `outrig mcp show-merged` alike. The error names the server, the `env` key, the variable, and
  the image, and it comes before any sidecar starts or any server runs, so the variable is never
  read. This covers the primary image's label and each named sidecar's, whoever named the image:
  your global config and `--image` too. A server your config declares by name replaces the
  label's entry whole and follows the rules above instead. An image of your own that references
  a variable -- an `outrig image build` image whose `image.toml` has `${DB_TOKEN}` -- needs it
  listed here to start. `outrig image build` itself is not held: it runs the servers of a
  directory you chose.
- **One list covers all of these places** -- build-args, MCP `env`, and a label's `env` -- and
  applies to every repo this machine runs. An entry is an exact variable name,
  `^[A-Z_][A-Z0-9_]*$` as in a `${VAR}` reference. There are no patterns.

**A variable on the list is the repo's to route.** The repo decides which build-arg or server
environment receives it, in its config or in a label it controls, and the Dockerfile `RUN` step
or MCP server that gets it can do anything the container may with it: print it, write it into the
workspace, or send it wherever the session's [network policy](#network) lets traffic out. A
build-arg is also recorded in the image's history (`podman history --no-trunc`). List a variable
only if you would hand it to everyone who can change that repo's config or the images it builds
or names, the agent included.

A repo config may not declare `env-allow`, even as an empty list, any more than it may set
`session-root` or the `[network]` policy keys: it would be answering for itself. The load
rejects the file and names the key.

The global config is read leniently (see [What a load sets aside](#what-a-load-sets-aside)), so
an `env-allow` that is misspelled, is not a list, or holds an entry that is not a variable name
is set aside with a warning instead of stopping every repo on the machine. That leaves the list
empty and every reference refused, an image label's included: a mistake here narrows what a repo
may read and never widens it. The warning is printed before the refusal it causes.

## `[providers.<name>]`

A provider tells outrig how to reach a model: an endpoint that speaks a known wire format. Every
style carries an `api-key`, which names a variable in the operator's environment, so providers
belong to the global config: a repo config that declares one is rejected at load, whatever name
it uses, and its models name the global config's providers. The accepted `style` values are
`"openai"` and `"anthropic"`. Which other fields are valid depends on `style`.

A local model is an ordinary `style = "openai"` provider pointed at a server on `localhost`
(Ollama, vLLM, `llama.cpp`) -- see
[Local models](https://tgockel.github.io/outrig/concepts/llm-providers.html#local-models).
The in-process `style = "mistralrs"` was removed in 0.3. A global config that still declares one
loads with that provider [set aside](#what-a-load-sets-aside) and a warning, and any other file
that declares one is refused.

### `style = "openai"`

```toml
[providers.openai]
style    = "openai"
base-url = "https://api.openai.com/v1"
api-key  = "${OPENAI_API_KEY}"

[providers.local-ollama]
style    = "openai"
base-url = "http://localhost:11434/v1"
api-key  = "${OLLAMA_API_KEY}"
```

| Key                    | Type         | Required | Default | Description                      |
|------------------------|--------------|----------|---------|----------------------------------|
| `style`                | string       | yes      | --      | Must be `"openai"` for this row. |
| `base-url`             | string (URL) | yes      | --      | HTTPS endpoint for the provider. |
| `api-key`              | string       | yes      | --      | Env-var reference, see below.    |
| `request-timeout-secs` | integer      | no       | `600`   | HTTP timeout for LLM calls.      |
| `retry-budget-secs`    | integer      | no       | `600`   | Transient-retry budget, seconds. |
| `role-alternation`     | string       | no       | --      | Strict alternation, see below.   |

`request-timeout-secs` bounds each individual attempt, and defaults high enough not to cut
off long reasoning completions. It must be between `1` and `3600` seconds; `0` is rejected,
because it is an *immediate* timeout rather than a disabled one and every request would fail
before it could be answered. `retry-budget-secs` bounds *all* the attempts together: an
LLM request that fails transiently is retried until it succeeds or the budget runs out, then
ends the turn without ending the session. Which failures count as transient, how the wait is
chosen, and how `Retry-After` is honored are described in
[LLM providers](https://tgockel.github.io/outrig/concepts/llm-providers.html#transient-failures);
this page is
the reference for the keys themselves.

Two things about the budget that belong here, because they are about the keys:

- It is **wall clock from the first attempt**, including time each attempt spends in flight,
  not only time spent sleeping. That matters when it is set near `request-timeout-secs`: one
  attempt that runs the timeout out spends the whole budget and buys no retries. It does not
  matter for a rate limit, which comes back in milliseconds.
- `0` disables retries -- the first failure is final. Useful for scripted runs that would
  rather fail fast than wait.
- It is not the only bound on a call that never reaches the endpoint. While no attempt has
  got bytes back -- a refused connection, an unresolvable host, a TLS mismatch -- the loop
  stops after 30 seconds instead, on the reasoning that a wrong address does not come right
  in ten minutes. Getting connected is separately capped at ten seconds per attempt, so a
  host that drops packets rather than refusing them cannot hold an attempt open for the
  whole `request-timeout-secs` first. Both are fixed and have no key; the loop takes
  whichever of the two budgets is shorter, so setting this one below it, or to `0`, still
  wins -- for the *retrying*. Neither budget cancels an attempt already in flight, so a
  value below ten seconds does not shorten the connection cap, and an unreachable address
  is settled in about 30 seconds rather than exactly 30. Neither bounds the wait for a
  response once connected -- that is `request-timeout-secs`, and it is why that default is
  high.
- It is not the only bound on the second retry layer. A provider that answers with a body
  outrig cannot use gets at most two retries whatever the budget says, because such a
  response comes back immediately and the budget alone would spend itself on dozens of them.
  `0` still switches that layer off along with everything else.

`role-alternation` says whether the provider requires the user's and the model's turns to
alternate in every request. Unset, or `"relaxed"`, is a provider that merges or accepts two
messages of one role in a row, which OpenAI's API and Anthropic's both do. `"strict"` is one that
refuses them -- a Bedrock-backed Claude behind an OpenAI-compatible gateway is known to -- and
makes `outrig run-new` send it a conversation that alternates by construction. The conversation
`run-new` sends is a part of the whole, and a cut can put one role after itself: a turn brought
back without the rest of its round opens on the model's call right after the model's own reply,
and a round that ended on tool results is followed by the next round's opening. With `"strict"`,
the first is left out, and the second is left out whole once the next round opens, since each of
its turns ends on results. What is left out stays in `runtime.history`, the line that opens a
round counts it, and the event log names it. When a provider refuses a call that carried such a
pair, the error says to set this key. It configures nothing on the provider's side, and `run`,
which sends whole rounds, ignores it. See
[`outrig run-new`](https://tgockel.github.io/outrig/reference/cli.html#outrig-run-new) for
what a shortened conversation looks like to a provider.

### `style = "anthropic"`

Anthropic's native Messages API: requests go to `{base-url}/v1/messages` and authenticate
with the `x-api-key` header. Use this to reach Claude directly. Reaching Claude through an
OpenAI-compatible bridge (OpenRouter and friends) is a `style = "openai"` provider pointed
at that bridge instead -- both work, and they are different rows.

```toml
[providers.anthropic]
style    = "anthropic"
base-url = "https://api.anthropic.com"
api-key  = "${ANTHROPIC_API_KEY}"
```

| Key                    | Type         | Required | Default | Description                         |
|------------------------|--------------|----------|---------|-------------------------------------|
| `style`                | string       | yes      | --      | Must be `"anthropic"` for this row. |
| `base-url`             | string (URL) | yes      | --      | HTTPS endpoint for the provider.    |
| `api-key`              | string       | yes      | --      | Env-var reference, see below.       |
| `request-timeout-secs` | integer      | no       | `600`   | HTTP timeout for LLM calls.         |
| `retry-budget-secs`    | integer      | no       | `600`   | Transient-retry budget, seconds.    |
| `role-alternation`     | string       | no       | --      | Strict alternation, see below.      |

`base-url` is the API root, without the `/v1/messages` path -- outrig appends that. A
trailing `/v1`, `/messages`, or `/v1/messages` is trimmed if you write one anyway, so
`https://api.anthropic.com` and `https://api.anthropic.com/v1` behave identically. Timeout,
transient-retry, and `role-alternation` behavior match `style = "openai"` exactly.

Anthropic requires an output-token ceiling on every request. outrig knows one for the model
identifiers it recognizes; any other identifier needs `max-tokens` on the model or the
agent, or the first turn fails saying so. See
[anthropic models](#anthropic-models).

### `api-key` syntax

`api-key` **must** use the env-var-substitution form `"${VAR_NAME}"` -- exactly that, nothing
else. outrig refuses any other value, including a plain string that happens to look like an API
key. This keeps actual key material out of config files unconditionally:

```toml
api-key = "${OPENAI_API_KEY}"   # OK -- outrig reads $OPENAI_API_KEY at run time
api-key = "sk-..."              # ERROR -- looks like a literal key, refused
api-key = "$OPENAI_API_KEY"     # ERROR -- braces required
api-key = "OPENAI_API_KEY"      # ERROR -- ${...} required
```

The variable name must match `^[A-Z_][A-Z0-9_]*$`. If the named environment variable is unset
when outrig needs the key, outrig fails with a pointed error.

A provider that carries an `api-key` is accepted only from the global config (or the file named
by `--global-config`). The reference is resolved on the host, and the provider's `base-url`
says where the host process sends the result; a repo config that could pair the two would
decide where the operator's secret goes. `outrig run`, `outrig mcp`, and `outrig build` all
refuse a repo config -- `.agents/outrig/config.toml` or a `--config` file -- that declares one,
naming the provider and the variable.

See [Concepts -> LLM Providers](https://tgockel.github.io/outrig/concepts/llm-providers.html).

## `[models.<name>]`

A model entry has two shapes, and sets exactly one of them:

- **A provider shape** -- `provider` plus the `identifier` that provider's API knows the
  model by.
- **An alias shape** -- `alias`, naming one or more other models. No provider-shape field
  is allowed alongside it.

### Alias models

An `alias` gives one name to another model, or to an ordered set of provider-equivalent
ones.

```toml
# One name for one model. Repoint this line and every agent naming `opus`
# follows, with no other edit.
[models.opus]
alias = "opus-5"

# One name for a set of equivalents, in preference order.
[models.smart]
alias = ["opus-5-bedrock", "opus-5-anthropic", "opus-5-azure"]

# Aliases may name aliases. This flattens to opus-5-bedrock,
# opus-5-anthropic, opus-5-azure, haiku-5.
[models.default]
alias = ["smart", "haiku-5"]
```

| Key     | Type    | Required | Default | Description                                     |
|---------|---------|----------|---------|-------------------------------------------------|
| `alias` | str/arr | yes      | --      | Other model names, in preference order.         |

An alias **is** a model. It lives in the same table, and every place that accepts a model
name accepts it unchanged: `--model`, `default-model`, `[agents.<name>].model`, and the
`outrig__subagent` tool's `model` argument.

An alias resolves by flattening depth-first in config order, keeping the first occurrence
of a repeated name. OutRig then picks the **first candidate this build could actually
reach**: its provider exists, its style is one this build has a client for, and its
`api-key` variable is set and non-empty. That is what lets one committed config serve a
laptop with `ANTHROPIC_API_KEY` and a CI runner with a Bedrock role.

Selection happens once, when the session starts, and answers *"am I configured for
this"* -- **not** *"is this endpoint up"*. An alias with no reachable candidate fails the
session, naming each candidate and why it was skipped.

The order is a runtime fallback as well. A model call that fails against one candidate --
after that endpoint's own retries are spent, or immediately on a failure no retry fixes,
such as a `401` -- moves to the next, and prints that it did. Every call starts again at
the head of the list, so one rate-limit window does not demote the preferred vendor for the
rest of the session.

The whole chain is bounded by one `retry-budget-secs` rather than one per candidate, and
the value used is the one on the **first selectable candidate's** provider -- a
`retry-budget-secs` set on any later candidate's provider is not consulted. So a `0` there
disables retries for the whole chain, and reordering an alias can change which budget
governs it.

When every candidate has failed the report names each one's reason, and what ends depends
on why. If at least one failed recoverably -- a rate limit, an unusable response -- the
**turn** ends, keeping any tool calls it had already run, and can be continued, or sent
again if it failed on its first call. If every one was terminal, such as a `401` at each
vendor, the **session** ends, exactly as that failure ends it for a single model: no resend
can satisfy credentials that are refused everywhere. `outrig run-new` ends the round either
way, and its session goes on.

A single-target alias is pure renaming, so it keeps its target's own errors -- an unset key
still names the variable rather than becoming one line of a list.

Two spelling notes. `alias = "opus-5"` and `alias = ["opus-5"]` are the same config; a
round-trip through OutRig rewrites the first as the second. And an OutRig older than this
feature refused every key it did not know, so it
rejects a config using `alias` outright rather than degrading -- worth knowing before putting
one in a shared repo config.

### Remote-provider models

Models on a remote provider -- `style = "openai"` or `style = "anthropic"` -- name their
model with an `identifier`.

```toml
[models.fast]
provider   = "openai"
identifier = "gpt-4o-mini"

[models.smart]
provider   = "openai"
identifier = "gpt-4o"
```

| Key              | Type    | Required | Default | Description                               |
|------------------|---------|----------|---------|-------------------------------------------|
| `provider`       | string  | yes\*    | --      | Name of an entry in `[providers.<name>]`. |
| `identifier`     | string  | yes      | --      | Model id passed to the provider API.      |
| `max-tokens`     | integer | no       | --      | Output-token ceiling per turn, see below. |
| `context-window` | integer | no       | --      | The model's whole window in tokens.       |

\* Required unless the entry sets `alias` instead, which forbids every field in this table
-- `max-tokens` and `context-window` included.

`max-tokens` on a model is the fallback for every agent that uses it;
`[agents.<name>].max-tokens` wins where it is set. Leaving both unset lets the provider
decide -- which is fine for OpenAI-compatible endpoints, and is what the next section is
about for Anthropic. A value must be at least 1: `0` leaves no room to answer and is refused at
load, on a model and on an agent alike.

Note the spelling. In config it is `max-tokens`, like every other key; `max_tokens` is an
unknown key, ignored with a warning. The underscored form is what the provider API calls it, so
it is what appears in a raw error coming back from one.

`context-window` is the model's whole context window in tokens -- a request and its reply
together -- as the provider publishes it, such as `200000` for current Claude models. It
configures nothing on the provider's side; it tells `outrig run-new` how much one model call can
carry. Each call sets aside the reply's ceiling (`max-tokens`, or the lower one the provider
holds it to) and carries the system prompt and as much of the conversation as fits in the rest.
Unset, `run-new` assumes a 128,000-token window, sets aside at most a quarter of it for the
reply, and warns once at startup; it never infers a window from `identifier`. A model with a
larger window is sent less than it could take until its row says so, and one with a smaller
window can still refuse a request as too long. A `max-tokens` at or above `context-window` is
refused when the session starts. Under an alias each model is held to its own window: a call
that moves to the next model is sent what fits that model's. `outrig run` ignores the key, and
says so at startup: one line naming the models that set it, which only `outrig run-new` reads.

#### anthropic models

```toml
[models.sonnet]
provider   = "anthropic"
identifier = "claude-sonnet-4-6"

[models.older]
provider   = "anthropic"
identifier = "claude-3-5-sonnet-20241022"
max-tokens = 8192
```

The Messages API rejects a request with no `max_tokens`, so outrig has to send one. Three
tiers decide which, highest priority first:

1. `[agents.<name>].max-tokens`, then `[models.<name>].max-tokens`.
2. The published ceiling for a Claude identifier outrig recognizes -- 128000 for the
   `claude-opus-4-6` and later families, 64000 for the `claude-opus-4`, `claude-sonnet-4`,
   and `claude-haiku-4-5` families.
3. Otherwise a fallback of **32768**, announced once on stderr:

   ```text
   [outrig] claude-3-5-sonnet-20241022 has no published output-token ceiling in this
            build, so turns are capped at 32768. Set [models.older].max-tokens (or
            [agents.coding].max-tokens) to choose your own.
   ```

Tier 2 is also a cap, not only a default: a tier-1 value *above* the published ceiling is
lowered to it. The API refuses an over-ceiling request outright, so the whole turn fails
rather than being cut short, and the published number is the only one that runs. This
applies where outrig recognizes the identifier; for one it does not, there is no ceiling to
cap against and the configured value travels whole.

Tier 3 is a floor under the failure, not a recommendation -- prefer setting the ceiling
yourself, as `[models.older]` does above. The recognized set in tier 2 is whatever the
pinned rig release knows, so an identifier newer than that release lands in tier 3 even
though it is current -- and, for the same reason, is not capped.

32768 is chosen to fail in the direction you can see. A model whose real limit is *lower*
-- the 3.x families cap at 8192 or 4096 -- rejects the request outright, naming its own
limit, and one config line fixes it. A ceiling set too low instead cuts replies off
mid-sentence with nothing logged, which is much harder to recognize as a config problem.
`outrig config init` prompts for `max-tokens` when it writes an Anthropic model, so a
generated config carries an explicit one either way.

## `[agents.<name>]`

An agent is the runnable unit: a model plus a system prompt, optionally bound to an image-config
so `outrig run --agent <name>` knows which sandbox to use.

The whole table is optional. A run that names no agent behaves as an entry with no keys set:
no preamble, no image hint, every limit from the top level. Declare agents when you want a
preamble or a per-agent knob; skip them to run a model against a sandbox and nothing more.

```toml
[agents.coding]
# model omitted -> falls back to top-level default-model
image = "coding"
preamble  = "You are a careful coding assistant. Repo is at /workspace."
temperature = 0.2
max-tokens  = 4096
tool-call-max = 300
tool-result-max = 1048576

[agents.review]
model    = "smart"        # explicit override of default-model
preamble = "You are a meticulous code reviewer..."
```

- `model` (string, optional, default: `default-model`): name of an entry in
  `[models.<name>]`.
- `preamble` (string, optional, default: none): system prompt for this agent. If it is
  omitted, requests carry no system prompt.
- `repo-instructions` (string, optional, default: `"agents-md"`): whether the system prompt also
  carries the repository's own instructions for agents, after `preamble`. `"agents-md"` loads
  `AGENTS.md` at the workspace's root or else `.agents/AGENTS.md`, whichever exists first, and
  adds nothing for a workspace with neither. `"none"` leaves the repository's files out.
  `run-new` reads the file inside the container, at the workspace's container path, once as the
  session starts, so the prompt carries what the agent would see by reading the file itself.
  It carries the first 32,768 bytes, cut on a character boundary, and when the file is longer
  it ends with a note naming the file and its size. The text counts against every call's
  context window, so set `"none"` for an agent whose model has little room. A file with
  nothing in it is passed over for the next place; one that is not UTF-8 or cannot be read
  loads nothing, and a warning on the host says so. A child agent is never given the file: its
  parent writes its prompt. `outrig run` ignores the key and says so at startup.
- `image` (string, optional, default: `default-image`): default image-config
  to launch.
- `temperature` (float, optional, default: provider default): sampling temperature.
- `max-tokens` (integer, at least 1, optional, default: the model's `max-tokens`, else the
  provider default): output token max per turn. Worth setting in one place or the other for an
  Anthropic model whose identifier outrig does not recognize, which otherwise runs on a
  fallback ceiling -- see [anthropic models](#anthropic-models).
- `tool-call-max` (integer, optional, default: top-level value or `50`): tool calls per turn.
- `tool-result-max` (integer, optional, default: top-level value or `262144`): bytes per result.
- `subagents` (bool, optional, default: `true`): whether this agent may launch subagents. When
  `false`, the `outrig__` subagent tools are not registered at all, so the agent's tool list and
  context cost are unchanged from a build without the feature.
- `subagent-depth-max` (integer, optional, default: top-level value or `3`): how deeply this
  agent's subagents may nest. See the top-level `subagent-depth-max`.
- `subagent-width-max` (integer, optional, default: top-level value or `8`): how many live
  subagents this agent may launch. See the top-level `subagent-width-max`.
- `children-max`, `model-concurrency-max`, `requests-max`, `token-budget` (integers, optional,
  default: the top-level value): the limits a `run-new` session this agent runs is held to. See
  the top-level keys.

If `model` is omitted, outrig falls back to the top-level `default-model`; an error if neither is
set, except `outrig run --model <name>` may supply the selected agent's model for that run. When
`outrig run --agent <a>` runs, the chosen image-config is `--image` if given, otherwise
`agents.<a>.image` if set, otherwise `default-image`, otherwise outrig's
[built-in default](#the-built-in-default-image-config). A run with no agent drops the middle
rung: `--image` if given, otherwise `default-image`, otherwise the built-in.
`tool-call-max` is per turn, not per session; follow-up prompts start a fresh count.
`tool-result-max` is per result and applies equally to successful MCP results and MCP error
messages. It also caps what `outrig__get_result` hands back from a subagent.

`subagents` is on by default. A subagent shares this agent's container, MCP tools and limits, but
not its preamble or context, and may name a different `[models.<name>]` at launch -- see
[Concepts -> Subagents](../concepts/subagents.md). Turn it off for agents that should stay
single-threaded, or to save the context the five tool schemas occupy. A subagent may itself
launch subagents up to `subagent-depth-max`; each launching agent only sees the subagents it
launched.

## `[workspace]`

```toml
[workspace]
host-path      = "."
container-path = "/workspace"

[[workspace.mounts]]
host-path      = "../shared-docs"
container-path = "/resources/shared-docs"

[[workspace.mounts]]
host-path      = "/var/tmp/outrig-cache"
container-path = "/resources/cache"
access         = "read-write"
```

- `host-path` (path, optional, default: `"."`): primary workspace host path, an
  existing directory. A relative value resolves against the directory of the
  file that declared it -- see [path resolution](#path-resolution).
- `container-path` (path, optional, default: `"/workspace"`): where the primary
  workspace is mounted in the container, an absolute path other than `/`. In a session with a
  [binding](#bindingsname), the workspace is mounted at its host path instead, and a declared
  `container-path` must be that path.
- `workspace.mounts` (array, optional, default: `[]`): extra directory bind-mounts.
- `mounts[*].host-path` (path, required): host directory to mount. Relative paths
  resolve against the directory of the file that declared the entry -- see
  [path resolution](#path-resolution). Global and repo mounts are concatenated,
  so one list can hold entries with different base directories.
- `mounts[*].container-path` (path, optional, default: the resolved `host-path`): absolute
  in-container mount point. Left unset, the directory appears in the container where it is on
  the host. In a session with a [binding](#bindingsname) it must be left unset or be that
  path.
- `mounts[*].access` (string, optional, default: `"read-only"`): either
  `"read-only"` or `"read-write"`.

A repo config may not declare a `host-path` (primary, extra, or sidecar mount) that covers your home
directory or one above it, or -- read-write -- the global config file, its directory, or a
directory its path passes through (a symlink or missing component there could be swapped to
redirect it): the repo config sits inside the workspace, so an agent could otherwise rewrite the
global config for the next session. Declare such a mount in the global config if you mean it. A
workspace no config declares is held to the same two rules.

The primary workspace bind-mount is always read-write and uses `--userns=keep-id` so files
written inside the container appear with your host UID/GID. Extra mounts default to read-only;
set `access = "read-write"` only for directories the agent should be able to modify. See
[Concepts -> Workspace](../concepts/workspace.md).

## `[bindings.<name>]`

> `outrig run-new` and the library's `harness` only; `outrig run` ignores bindings.

A binding is an object a process on the host holds for the agent, presented to its Python under
`<name>`. One process holds every binding of a session: OutRig installs each binding's packages,
starts the process before the agent's interpreter, calls each factory there in order, tells the
agent each name and description, and stops the process -- with every program it started -- when
the session ends. Nothing is bound by default.

```toml
[bindings.repo]
description = "The project's Git repository, on the host, with your remotes and credentials."
requires    = ["GitPython==3.2.0"]
factory     = "git:Repo"
args        = [{ path = "." }]
serialize   = true
```

- `description` (string, required): shown to the model beside the name.
- `requires` (array of strings, optional, default: `[]`): requirement specifiers -- a name,
  extras, a version specifier, markers, never a path, a URL or a pip option -- installed with
  the embedded Python's pip into `~/.cache/outrig/bindings/<hash>/`, once per set. Only
  pure-Python wheels install.
- `factory` (string, required): the callable that builds the object, as `module:callable`.
- `args` (array, optional, default: `[]`) and `kwargs` (table, optional, default: `{}`): literal
  values passed to the factory. A table holding `path` -- `{ path = "..." }`, and nothing else
  -- is a path, which reaches the factory as an absolute host path, its `..` left for the host
  to resolve; any other string is a string. A datetime, a NaN or an infinity cannot be passed.
- `serialize` (boolean, optional, default: `false`): `true` when the library is not
  thread-safe. The binding then runs one call at a time across all its connections, every
  kernel's included.

The agent's Python reaches a binding under its name and uses it as its library documents:
`repo.head.commit` asks the host for `repo.head`, then for its `commit`, and gives back a value or
another such object, waiting for each answer. Each request is checked on the host, where a
private name -- one that starts with `_` -- is refused, and, with
[`[events] mode = "record"`](#events), recorded with what it asked and how it ended (see
`doc/reference/events.md`).

The bindings' process runs on the host, as you, in the workspace's host directory, with your
environment less every variable the config names through `${VAR}` (each `api-key`,
`build-args` value and MCP `env` value), which are OutRig's own secrets. Every binding's
library runs with that one environment. What a library does on the host is part of what its
binding grants; see `SECURITY.md`.

**A repository's binding needs your approval.** A factory is code the host runs as you at
session start, and a repository's config is content a cloned project brings and the agent can
write. So a binding declared in the repo config -- or in a `--config` file, which takes its
place -- starts only once you approve that exact declaration in that repository. `run-new`
shows it whole and asks once; the approval is remembered under
`$XDG_STATE_HOME/outrig/approved-bindings/` (or `~/.local/state/outrig/approved-bindings/`),
by a digest of the declaration and the repository's root, so any edit asks again. With no
terminal to ask on, the binding does not start, and the session runs without it. A binding in
the global config needs no approval.

**Same paths.** When a session has a binding, the workspace and every
[`[[workspace.mounts]]`](#workspace) entry appear in the container at their host paths, so a
path the agent writes and a path the library reads name the same file. A `container-path` that
differs is a config error. Each binding's packages are mounted read-only in the container
under `/outrig/bindings/` and put on the interpreter's import path, so the library can be
imported there too -- as a local copy, which knows nothing of the host's object -- and its
exception classes catch what a call to the binding raised. A library may need more to import:
GitPython looks for a `git` program as it is imported, which an image without one lacks, unless
`GIT_PYTHON_REFRESH=quiet` is set first.

Bindings are ordered: the global config's, then the repo config's, then those the library's
`bind` adds, by name within each. That is the order the agent is told of them in, the order
their factories are called in, and the order their packages go on the import path of the
container and of the bindings' process alike: where two bindings' packages hold different
versions of one package, both sides import the first binding's.

## `[images.<name>]`

You declare one or more image-configs. The selected one becomes the agent's environment.
Each block takes exactly one of two shapes:

### Build-from-Dockerfile (existing form)

```toml
[images.coding]
dockerfile = ".agents/outrig/images/coding/Dockerfile"
context    = ".agents/outrig/images/coding"
build-args = { NODE_VERSION = "20" }
```

For a build-from-Dockerfile image, the block name becomes the built image's repository (the
image is tagged `<name>:<content-hash>`). The content hash includes Dockerfile/context content,
resolved build args, and the OutRig labels derived from `[images.<name>.mcp]`, so MCP config
changes produce a new inspectable cache tag. Use a repo-specific, lowercase name (e.g.
`outrig-standard`, not `standard`) so `podman images` makes clear which repo it came from. The
name must be a valid container image repository component -- see the validation rules below.

- `dockerfile` (path, required\*): path to the Dockerfile, relative to the directory of the
  file that declared the block -- see [path resolution](#path-resolution). For a repo
  image-config that is the repo root; for one declared in the global config it is that file's
  own directory.
- `context` (path, required\*): path to the build context, same rule.
- `build-args` (table str->str, optional, default: `{}`): extra Dockerfile `ARG`s.
  Keys are ARG names. Values are either literal strings or `${VAR}` references resolved
  from the host environment at `outrig build` time. A reference reaches buildah by name and
  outrig shows it only as the reference, as the
  [MCP `env` value syntax](#mcp-env-value-syntax) below describes for podman. buildah itself
  records every build-arg a `RUN` step sees, value included, in the image's history
  (`podman history --no-trunc`), so a build-arg does not keep a secret out of the image.
  In a repo config a reference may name only a variable the global config lists in
  [`[repo]`](#repo) `env-allow`; one you declare in the global config may name any.

### Use-existing-image (new form)

```toml
[images.scratch]
image-name = "docker.io/library/ubuntu:24.04"
```

- `image-name` (string, required\*): image reference passed to `podman pull` / `podman run`.
  Accepts any ref form podman supports: `name:tag`, `registry/name:tag`, `name@sha256:...`.

\* Exactly one of the two shapes must be set. Setting `image-name` alongside `dockerfile`,
`context`, or `build-args` is an error. Setting neither is also an error.

### The built-in default image-config

When nothing names an image -- no `--image`, no `agents.<n>.image`, no `default-image` --
outrig supplies one of its own rather than failing. This is what makes `outrig run` work in a
repo with no `.agents/outrig/config.toml` at all. It is ordinary config, injected into the
merged result at the bottom of the precedence order:

```toml
[images.outrig-default]
image-name = "docker.io/library/buildpack-deps:bookworm-scm"

  [images.outrig-default.mcp]
  fs    = { sidecar = "outrig-default-fs" }
  shell = { sidecar = "outrig-default-shell" }

[images.outrig-default-fs]
image-name = "docker.io/mcp/filesystem:latest"

[images.outrig-default-shell]
# Built from a Dockerfile outrig writes into your user cache directory
# (`~/.cache/outrig/builtin-images/`), never into your repo.

[sidecars.outrig-default-fs]
image = "outrig-default-fs"
view  = "primary"
args  = ["/workspace"]

[sidecars.outrig-default-shell]
image = "outrig-default-shell"
view  = "primary"
```

`buildpack-deps:bookworm-scm` is the smallest official image that carries `git`, `curl`, and
`ca-certificates` while setting no `ENTRYPOINT` and baking in no user -- both of which outrig
needs, since it appends `sleep infinity` after the image ref (which an `ENTRYPOINT` would
swallow as arguments) and writes the runtime user's `/etc/passwd` entry at start. Because both
servers run with `view = "primary"`, the commands they spawn resolve in *this* container's
filesystem, which is why the primary is the one that needs `git`.

The first run pulls two images and builds one. Each is reachable by name from `outrig build`
and from `--image`, so that cost can be paid deliberately -- one command per part:

```sh
$ outrig build --image outrig-default          # pulls buildpack-deps
$ outrig build --image outrig-default-fs       # pulls the filesystem server
$ outrig build --image outrig-default-shell    # builds the shell server
```

**Reserved names.** `outrig-default`, `outrig-default-fs`, and `outrig-default-shell` are
reserved as `[images.<name>]`; the latter two are also reserved as `[sidecars.<name>]`. Your
config wins: declare any one of them and outrig injects *none* of the built-in and says so.
Injection is all-or-nothing because a half-injected set is a broken config -- your
`[images.outrig-default]` beside outrig's `[sidecars.outrig-default-fs]` would leave that
block's `args` unreachable, which is a hard error for every command in the repo.

**Without the `outrig-enter` launcher.** `view = "primary"` needs the launcher, which is only
embedded when outrig was built with the `<arch>-unknown-linux-musl` target. Without it the
built-in degrades rather than failing: `fs` switches to `workspace = "rw"`, giving the same
file tools over a bind mount, and `shell` is dropped. A shell has no honest degraded form --
a bind-mounted sidecar sees none of the primary's toolchain, so it would report a different
environment than the one you have.

**`default-image` cannot name it.** `default-image = "outrig-default"` is still an error: that
key is validated when the config is loaded, before the built-in is injected. You never need to
write it -- the built-in is the rung that fires when the key is absent. To pin it explicitly,
declare `[images.outrig-default]` yourself, which shadows the built-in entirely.

Notes:

- `outrig image add` writes its output under `.agents/outrig/images/<name>/`. You can
  put Dockerfiles anywhere you want by editing these paths; the `.agents/outrig/images/`
  default just keeps outrig-specific files together.
- Inner keys of `build-args` are user-defined Dockerfile `ARG` names; they're left as written
  since they map to env-var-style identifiers.
- outrig does **not** inject UID/GID build-args. Host UID/GID are mapped to the container at
  run time, not baked into the image. See
  [Concepts -> Workspace](../concepts/workspace.md#uidgid-runtime-user-mapping).

### `[images.<name>.security]`

Optional runtime security controls for the selected container:

```toml
[images.coding.security]
capability-profile = "no-net-raw"
cap-drop = ["MKNOD", "SETFCAP"]
cap-add  = ["NET_BIND_SERVICE"]
no-new-privileges = false
devices  = ["/dev/fuse"]
unmask   = ["/proc/*"]
```

- `capability-profile` (string, optional, default: `"default"`): named Linux capability
  profile. Accepted values are:
  - `"default"`: preserve podman's default capability set and emit no capability flags unless
    `cap-drop` or `cap-add` is set.
  - `"no-net-raw"`: emit `--cap-drop=NET_RAW`.
  - `"drop-all"`: emit `--cap-drop=ALL`.
- `cap-drop` (array, optional, default: `[]`): extra Linux capabilities to drop.
- `cap-add` (array, optional, default: `[]`): Linux capabilities to add after profile and
  explicit drops are rendered.
- `no-new-privileges` (bool, optional, default: `true`): emit
  `--security-opt=no-new-privileges`. Setting it to `false` restores setuid escalation inside
  the container, letting any setuid-root binary in the image be used; see
  [Containers](../concepts/containers.md#devices-and-privilege-escalation) for the tradeoff.
  A nested container runtime does **not** need this cleared -- see
  [Nested container runtimes](../concepts/containers.md#nested-container-runtimes).
- `devices` (array, optional, default: `[]`): host device nodes to pass through, emitted as
  one `--device=<entry>` per entry in declaration order. An entry is podman's own form,
  `<host-path>[:<container-path>][:<permissions>]`: a bare absolute path passes the node
  through, `<container-path>` (absolute) puts it at another name, and `<permissions>` is some of
  `r`, `w`, `m`. Rootless podman does not enforce `<permissions>` -- `:r` left the node writable
  -- so do not rely on it.
- `unmask` (array, optional, default: `[]`): paths to exclude from podman's default masking,
  emitted as one `--security-opt=unmask=<path>` per entry in declaration order. Entries are
  absolute paths with `filepath.Match` glob syntax, and reach podman verbatim -- asking for
  `/proc/*` never widens into `ALL`. The one non-path value is `ALL`, podman's "mask nothing"
  token, which must be spelled in exact uppercase and must be the list's only entry; see the
  validation rules below for why. This turns a hardening flag off; see
  [Containers](../concepts/containers.md#devices-and-privilege-escalation) for what it
  re-exposes.

Capability names may be written as `NET_RAW` or `CAP_NET_RAW`; outrig normalizes to the
podman form without the `CAP_` prefix. This section does not configure seccomp, AppArmor,
SELinux, read-only roots, mount policy, or network egress filtering.

### `[images.<name>.mcp]`

Map of MCP server entries, **keyed on server name**. Each entry is one of two shapes, told
apart by type -- an array is the short form, a table the full form:

```toml
[images.coding.mcp]
# Short form -- array of strings, becomes { command = [...] }
shell = ["bash", "-lc", "exec shell-mcp-command"]

# Full form -- table with command + optional env
fs = { command = ["mcp-server-filesystem", "/workspace"] }
build = { command = ["cargo-mcp"], env = { CARGO_HOME = "/workspace/.cargo" } }
tests = { command = ["test-mcp"], call-timeout-secs = 1800 }

# Full form with a placement key -- runs in a sidecar container
lint = { command = ["mcp-lint", "--stdio"], sidecar = "tools" }
grep = { command = ["mcp-grep"], image = "ghcr.io/example/mcp-grep:1" }

# entrypoint-stdio -- no command; the container's ENTRYPOINT is the server. In a repo config,
# FETCH_TOKEN has to be listed in the global config's [repo] env-allow.
fetch = { image = "ghcr.io/example/mcp-fetch:2", env = { TOKEN = "${FETCH_TOKEN}" } }
serve = { image = "docker.io/mcp/filesystem:latest", args = ["/workspace"] }
```

`shell-mcp-command` is a placeholder. Replace it with the shell MCP server you install in the
image, or declare any other MCP command that should run inside the container.

- `command` (array of strings, required unless using the short form or the entrypoint-stdio
  form): argv of the MCP server.
- `env` (table str->str, optional, default: `{}`): env vars set on the `podman exec`
  invocation -- or, for entrypoint-stdio servers, baked in via `podman create --env`, which
  stores them in the container's config, visible to `podman inspect` on the host. A `${VAR}`
  value reaches podman by name rather than on its command line; see the
  [value syntax](#mcp-env-value-syntax). In a repo config, or in an image's `org.outrig.mcp`
  label, a reference may name only a variable the global config lists in [`[repo]`](#repo)
  `env-allow`.
- `sidecar` (string, optional): run this server in the named
  [`[sidecars.<sc>]`](#sidecarssc) container instead of the primary.
  With `command`, the server is exec-stdio in that container; *without* `command`, that
  container's ENTRYPOINT is the server (entrypoint-stdio).
- `image` (string, optional): give this server a dedicated anonymous sidecar created from this
  image ref (resolved like the sidecar `image` key). Mutually exclusive with `sidecar`. With
  `command`, the server is exec-stdio in that sidecar; *without* `command`, the image's
  ENTRYPOINT is the server (entrypoint-stdio). The container's lifetime equals the server's:
  its exit surfaces as tool errors while the session continues.
- `args` (array of strings, optional, default: `[]`): positional arguments for an
  entrypoint-stdio server, appended after the image ref on `podman create`. Requires `sidecar`
  or `image` and rejects `command`, which is already a full argv. The same key exists on
  [`[sidecars.<sc>]`](#sidecarssc); declaring it in both places for one
  container is an error rather than a merge.

  podman *appends* trailing arguments to an exec-form `ENTRYPOINT` and *replaces* `CMD`. So
  `args` is for images whose server is an `ENTRYPOINT`: for an image that puts its server in
  `CMD` instead, `args` replaces that `CMD` and the server never starts.

- `view` (string, optional, default: `"none"`): `"none"` or `"primary"`, with the meaning and
  the exclusions documented on [`[sidecars.<sc>]`](#sidecarssc). Legal here only on the inline
  entrypoint form -- it needs `image` and rejects `command`, because the anonymous sidecar it
  configures is the one `image` creates:

  ```toml
  [images.dev.mcp]
  fs = { image = "docker.io/mcp/filesystem:latest", view = "primary", args = ["/workspace"] }
  ```

  The `args` written here and the elements the sidecar image declares end up meaning different
  paths -- see [Concepts -> MCP Servers](../concepts/mcp-servers.md#primary-filesystem-view).

- `call-timeout-secs` (integer, optional, default: the top-level
  [`mcp-call-timeout-secs`](#top-level), else `600`): how long one `tools/call` to this
  server may run, 1 to 3600. Past it outrig sends the server `notifications/cancelled` and the
  model gets a timeout error in place of a result. Any transport, any placement. For a server
  whose tools run builds or test suites; `initialize` and `tools/list` are not affected -- they
  have a fixed 120-second bound each.

Any other key in a full-form entry is [set aside with a warning](#what-a-load-sets-aside), as
it is in every other table. Read that warning before the session goes on: an entry whose
`command` is misspelled has none, so beside `sidecar` or `image` it runs as an entrypoint-stdio
server. In an `org.outrig.mcp` label or an `image.toml` `[mcp]` table the key is still an error.

Notes:

- The first element of `command` must be on `$PATH` inside the container, or absolute.
- The map key (e.g. `fs`, `shell`, `build`) is the server name. It must match
  `^[a-zA-Z][a-zA-Z0-9_-]*$` and be unique within an image-config.
- The server name appears in `outrig logs <session> <server>` and as the prefix on the tools
  the server advertises (`<server>__<tool>`). The rule bounds no length, but a tool name outrig
  has to rewrite to fit 64 characters keeps the prefix only for a name of 54 characters or
  fewer -- see
  [Concepts -> MCP Servers](../concepts/mcp-servers.md#long-server-names).
- Each `env` value is either a literal string forwarded verbatim or a `${VAR}` reference
  resolved from the host environment at MCP startup and shown only as the reference -- see the
  subsection below. A reference in a repo config file is refused at load, and one in an image
  label when the session starts, unless the global config's [`[repo]`](#repo) `env-allow` lists
  the variable.
- Images can provide the same table via their `org.outrig.mcp` OCI label (placement keys are
  repo-config-only and rejected in labels; `call-timeout-secs` is allowed). Repo config entries
  override image entries by server name, wholesale -- a label's `call-timeout-secs` does not
  survive a config entry of the same name that omits it; see
  [Concepts -> MCP Servers](../concepts/mcp-servers.md#embedding-mcp-config-in-the-image).
- Build-from-Dockerfile repo images are stamped with the merged `org.outrig.mcp` label on cache
  misses, so `outrig image inspect <name>:<content-hash>` can show their declared repo-local MCP
  entries without starting a container. The label is part of the image's cache key, so changing
  a primary-hosted entry's `env` or `call-timeout-secs` rebuilds the image.

## `[sidecars.<sc>]`

Named sidecar containers hosting MCP servers away from the primary; see
[Concepts -> Containers](../concepts/containers.md#sidecar-containers). The block key `<sc>`
is the sidecar name; it embeds in the container name (`outrig-<sid>-<sc>`) and must match
`^[A-Za-z0-9][A-Za-z0-9_-]*$`.

Top-level and referenced by name, like `[models.<n>]` or `[providers.<n>]`, so any number of
image-configs can share one block. A session starts the blocks its image-config's `[mcp]`
entries name and only those -- declaring a block instantiates nothing on its own.

```toml
[sidecars.tools]
image      = "mcp-tools"
workspace  = "ro"
start      = "auto"
on-failure = "warn"

[[sidecars.tools.mounts]]
host-path      = "~/.cache/example"
container-path = "/cache"
access         = "read-write"

[sidecars.tools.security]
capability-profile = "no-net-raw"
```

- `image` (string, required): resolved exactly like `--image` -- an `[images.<name>]` config
  name first, else a raw podman ref that must be present locally.
- `args` (array of strings, optional, default: `[]`): positional arguments for the image's
  `ENTRYPOINT`, used only when this block is an *entrypoint host* -- see below. Same semantics
  and the same podman `ENTRYPOINT`/`CMD` rule as the `args` key on
  [`[images.<name>.mcp]`](#imagesnamemcp); setting it in both places is an error.
- `workspace` (string, optional, default: `"none"`): `"none"`, `"ro"`, or `"rw"`. Mounts the
  session workspace at the primary's container path with that access.
- `view` (string, optional, default: `"none"`): `"none"` or `"primary"`. `"primary"` runs the
  sidecar against the *primary container's* filesystem view -- its rootfs and every mount, at
  the primary's paths -- via the `outrig-enter` launcher, so an off-the-shelf MCP image serves
  the primary's files without the primary image carrying that tool. Entrypoint-stdio only, and
  mutually exclusive with `workspace` (the view already holds the workspace) and with
  `capability-profile = "drop-all"` (the view needs the mount capabilities). It grants
  `CAP_SYS_ADMIN` and `CAP_SYS_PTRACE` in the primary's user namespace -- a real posture
  change; see [MCP Trust Model](../concepts/mcp-trust-model.md) and `SECURITY.md`. The same key
  exists on the inline [`[images.<name>.mcp]`](#imagesnamemcp) one-liner form.
- `start` (string, optional, default: `"auto"`): `"auto"` starts with the session; `"manual"`
  declares a sidecar that starts only when asked -- `/sidecar add <name>` in the REPL, or
  `Outrig::add_sidecar` / `LaunchSpec::with_sidecar` from the library API. Until then its
  servers are skipped with a notice. Not available on an entrypoint host, whose container
  lifetime is its server's.
- `on-failure` (string, optional, default: `"abort"`): how a start/bootstrap/connect failure of
  this sidecar is handled at session start. `"abort"` fails the session; `"warn"` logs, skips
  the sidecar and its servers, and continues.
- `mounts` (array of tables, optional): same shape and validation as
  [`[[workspace.mounts]]`](#workspace), including
  [path resolution](#path-resolution) -- a relative `host-path` resolves against
  the directory of the file that declared the sidecar block.
- `security` (table, optional): same keys as [`[images.<name>.security]`](#imagesnamesecurity).

A block is an **entrypoint host** when the one `[images.<name>.mcp]` entry naming it omits
`command` -- the container process *is* the server, rather than a shell to `podman exec` into:

```toml
[sidecars.tools]
image     = "docker.io/mcp/filesystem:latest"
args      = ["/workspace"]
workspace = "ro"

[images.coding.mcp]
fs = { sidecar = "tools" }
```

Container lifetime equals server lifetime, so such a block hosts exactly one server and cannot
be `start = "manual"`. It also skips the in-container user bootstrap -- there is no `podman
exec` window before the entrypoint runs -- so the image's own `USER` applies to `workspace` and
`mounts`.

#### MCP `env` value syntax

Each entry on the right-hand side of an `env` table is one of:

```toml
build = { command = ["cargo-mcp"], env = {
  CARGO_HOME = "/workspace/.cargo",  # literal -- forwarded to podman as-is
  GH_TOKEN   = "${GITHUB_TOKEN}",    # reference -- resolved from host env at MCP startup
  STILL_LIT  = "${lower_case}",      # literal -- doesn't match ^[A-Z_][A-Z0-9_]*$
  ALSO_LIT   = "prefix-${X}-suffix", # literal -- embedded substitution is not supported
} }
```

The reference form is exactly `"${VAR}"`, where `VAR` matches `^[A-Z_][A-Z0-9_]*$` -- the
same syntax `api-key` accepts. Anything else is treated as a literal and passed through
verbatim, including malformed-looking references (lower-case names, unmatched braces, or
embedded substitution). If the named host env var is unset when the MCP server is about to
start, MCP startup fails with an error naming the variable, the server, and the env key.

In a repo config file a reference is first held to the global config's [`[repo]`](#repo)
`env-allow`: a variable that is not listed there is refused when the config loads, whether or
not it is set. A reference in an image's `org.outrig.mcp` label is held to it too, when the
session starts and the label has merged in. A literal is not a reference and needs no entry, and
neither does a value you type with `--env`.

A reference's value stays out of what outrig prints and off podman's command line, which any
local user can read with `ps` for as long as the command runs -- for an exec-stdio server, the
whole session. outrig's errors, the `--verbose` transcript (`container.log` and the terminal),
and the `RUST_LOG=debug` log all show the entry as `GH_TOKEN=${GITHUB_TOKEN}`, the way the
config wrote it. podman is given a bare `--env GH_TOKEN`, with the value in podman's own
environment, which only your user can read, and copies it into the container from there.
`build-args` reach buildah the same way, as a bare `--build-arg`.

The exception is a key podman or buildah would read from that environment themselves, where
setting it would change what they do. Such a key keeps its value on the command line, though
outrig still shows it as the reference:

- process and locale: `HOME`, `PATH`, `TMPDIR`, `USER`, `LOGNAME`, `SHELL`, `TERM`, `TZ`,
  `TZDIR`, `LANG`, `LANGUAGE`, `SUDO_USER`, `container`, and any key starting `LC_`, `LD_`,
  `XDG_`, or `CLICOLOR`
- proxies: any key ending `_PROXY`, in either case
- the Go runtime and its TLS roots: `GODEBUG`, `GOGC`, `GOMAXPROCS`, `GOMEMLIMIT`,
  `GOTRACEBACK`, `SSL_CERT_FILE`, `SSL_CERT_DIR`
- systemd and D-Bus: `NOTIFY_SOCKET`, `INVOCATION_ID`, `JOURNAL_STREAM`, and any key starting
  `LISTEN_`, `WATCHDOG_`, or `DBUS_`
- the containers stack: `REGISTRY_AUTH_FILE`, `REGISTRIES_CONFIG_PATH`, `GNUPGHOME`,
  `SSH_AUTH_SOCK`, `SOURCE_DATE_EPOCH`, `BUILD_REGISTRY_SOURCES`, `CI_DESIRED_DATABASE`,
  `SUPPRESS_BOLTDB_WARNING`, `DISABLE_HC_SYSTEMD`, `LOGLEVEL`, `OPT`, `MOBY_DISABLE_PIGZ`,
  `BBOLT_VERIFY`, `BURNTSUSHI_TOML_110`, and any key starting `CONTAINER_`, `CONTAINERS_`,
  `STORAGE_`, `BUILDAH_`, `PODMAN_`, `DOCKER_`, `CNI_`, or `OCICRYPT_`
- internal markers: any key starting `_`, such as `_PODMAN_PAUSE` or `_CONTAINERS_*`

The list covers what podman 4.9 and 5.7 and buildah 1.33 and 1.42 read, including the libraries
they are built from.

A proxy key that references the variable of its own name, such as
`HTTPS_PROXY = "${HTTPS_PROXY}"`, is passed by name all the same: podman and buildah already
have that value and read proxies as they find them. No other key on the list is, even
self-referenced, because the engine may rewrite its own copy first -- buildah makes a relative
`TMPDIR` absolute. A key that is not a plain variable name (`^[A-Za-z_][A-Za-z0-9_]*$`) keeps
its value on the command line too. To keep a secret off it, give the secret a key outside these.

outrig cannot redact what the server, or a build step, prints itself.

See [Concepts -> MCP Servers](../concepts/mcp-servers.md).

## Resolution: which file wins

Both files are loaded; entries are merged by name. Repo entries win over global entries. A name
defined only once is straightforward; a name defined in both means the repo's definition is
used in full (no per-key merging).

```
~/.outrig/config.toml             .agents/outrig/config.toml         effective
[providers.openai]                                                   global value
[models.fast]                     [models.fast]                      repo overrides
                                  [models.review]                    repo only
```

Two maps have a repo-side restriction. A repo config may declare no provider, since every
provider carries an `api-key`, so every provider in the effective config is one the global file
declared, `base-url` included; a repo `[models.<name>]` names a global provider the way `fast`
does above. And an image a repo config declares may reference a host variable, in
`build-args` or an MCP `env`, only if the global config's [`[repo]`](#repo) `env-allow` lists
it. A repo image replaces a global one of the same name whole, references included, so the
restriction follows whichever file's entry wins: the references in an image you declare in the
global config are yours, and are not held to the list. A server an image's label declares is
held to it wherever the image was declared, since the label is the image author's text; the
entry your config declares for the server by name replaces the label's, and follows the rule
above.

`outrig run` walks the chain at startup: agent -> model (explicit or `default-model`) -> provider
-> resolved api-key from env. Anything that fails to resolve is an error printed to stderr
before the REPL starts.

`[workspace]` primary fields merge per key. A repo `host-path` or `container-path` overrides
the corresponding global value; when the repo is silent, the global value remains in effect.
If neither file declares a key, its built-in default (`.` or `/workspace`) applies. Extra
`workspace.mounts` are combined instead of replaced: global mounts are kept first, followed by
repo mounts. Duplicate final `container-path` values are rejected during validation.

`[sidecars.<sc>]` merges by name like the other top-level maps, so a repo image-config can
reference a sidecar the user declared globally.

`[bindings.<name>]` merges by name too. A repo binding that replaces a global one of the same
name is the repository's, and needs your approval like any other.

`[network].mode` merges per key like `[workspace]`'s primary fields: a repo `mode` overrides
the global one, and when the repo is silent -- whether it omits `[network]` entirely or writes
the table without a `mode` -- the global mode remains in effect. If neither file declares it,
the built-in `default` applies. This matters when global config enables audit or filter mode
and a repo explicitly sets `mode = "default"`, which is the only way a repo turns interception
off. Network policy keys are global-only: a repo config that sets `network.default`,
`network.allow`, or `network.deny` is rejected at load, and no repo value can reach the
effective policy even if it carries one.

### Path resolution

A relative path is resolved against the directory of the config file that declared it, not
against whichever repo is current. Provenance is recorded per entry before the merge, so a
concatenated `workspace.mounts` list can hold entries with different base directories.

| Declared in                  | Relative paths resolve against         |
|------------------------------|----------------------------------------|
| `.agents/outrig/config.toml` | the repo root                          |
| `--config <path>`            | `<path>`'s parent directory            |
| `~/.outrig/config.toml`      | that file's directory (`~/.outrig/`)   |
| `--global-config <path>`     | `<path>`'s parent directory            |

A `--config` path that ends in `.agents/outrig/config.toml` is that repo's own config and takes the
first row; any other takes the second. See
[Reference -> CLI](https://tgockel.github.io/outrig/reference/cli.html#global-flags).

A binding's `{ path = "..." }` arguments are the one exception: a relative one always
resolves against the repository's root, whichever file declared it, so a global binding's
`{ path = "." }` names the project each session works on rather than the directory holding
the global config.

Absolute paths are used as-is and ignore the rule entirely. This applies to
`[images.<name>].dockerfile` and `.context`, to `[workspace].host-path`, to `host-path` in
both `[[workspace.mounts]]` and `[sidecars.<sc>.mounts]`, and to `session-root`. Because
`[workspace]` merges per key, a `host-path` inherited from the global file resolves beside that
file, not from the repo root. `session-root` is global-only, so it always resolves beside the
global config.

A path whose first component is `~` starts from your home directory instead: `~` alone is the
home directory, and `~/.cache/example` a directory inside it. Like an absolute path, it means the
same place whichever file declares it. The `~` has to be the whole first component, so
`~alice/src` is an ordinary relative path, and `./~/src` reaches a directory really named `~`.
Nothing else is expanded: the `${VAR}` form that `api-key`, `build-args`, and MCP `env` accept is
literal in a path. Your home directory is `$HOME`, or your passwd entry's when `HOME` is unset or
empty; without either, `~` is an ordinary directory name too.

Provenance is recorded, not written back. A merged config is a flattened snapshot: re-serializing
one emits each inherited path as the literal text its source file used, without the base
directory that text was resolved against. Writing that snapshot out and loading it as a repo
config therefore re-reads relative inherited paths against the repo root. Outrig never does
this -- the merge result is used in memory and discarded -- but a tool that round-trips a loaded
config through disk should resolve paths first, or keep the two files separate. `session-root`
is the exception: having one possible base, it is resolved as it is read, so a loaded config
holds it absolute and re-serializes it that way.

The practical effect is that a global `[images.<name>]` can use the build shape: its Dockerfile
and context live beside `~/.outrig/config.toml` and are found from any repo on the machine.

Because provenance is recorded per entry, a diagnostic about a config-declared path names the
file that declared it, as a trailing `(declared in "<path>")`. Every image,
`[workspace].host-path`, and mount rule carries it -- including the mount rules that judge the
value rather than look for a directory, since the question it answers is which file to go edit.
The reported path stays the raw config value rather than the resolved one:

```
workspace mount host-path "shared" does not exist (declared in "/home/you/.outrig/config.toml")
```

An entry built through the library API rather than loaded from a file records no source, and
its diagnostics carry no clause.

## Full examples

### Global `~/.outrig/config.toml`

```toml
default-model      = "fast"
session-root       = "/var/lib/outrig/sessions" # optional; default = XDG data dir
tool-call-max      = 100                         # optional; default = 50
tool-result-max    = 262144                      # optional; default = 256 KiB
subagent-depth-max = 3                           # optional; default = 3
subagent-width-max = 8                           # optional; default = 8
children-max       = 64                          # optional; default = 64
model-concurrency-max = 8                        # optional; default = 8
requests-max       = 256                         # optional; default = 256
token-budget       = 20000000                    # optional; no cap when unset
retry-budget-secs  = 600                         # optional; default = 600
mcp-call-timeout-secs = 600                      # optional; default = 600

[network]
mode = "default"                                 # optional; default, audit, or filter
default = "deny"                                 # optional for filter mode
allow = ["github.com:443", "*.npmjs.org"]        # optional; global only
deny  = ["*:22"]                                 # optional; global only

[repo]
env-allow = ["GITHUB_TOKEN"]                     # optional; global only

[providers.openai]
style    = "openai"
base-url = "https://api.openai.com/v1"
api-key  = "${OPENAI_API_KEY}"

[providers.anthropic]
style    = "anthropic"
base-url = "https://api.anthropic.com"
api-key  = "${ANTHROPIC_API_KEY}"

[providers.local]
# a model served on this machine, by Ollama here; see LLM providers -> Local models
style    = "openai"
base-url = "http://127.0.0.1:11434/v1"
api-key  = "${OLLAMA_API_KEY}"

[models.fast]
provider   = "openai"
identifier = "gpt-4o-mini"

[models.smart]
provider   = "openai"
identifier = "gpt-4o"

[models.claude]
provider   = "anthropic"
identifier = "claude-sonnet-4-6"
max-tokens = 16384

[models.local-fast]
provider   = "local"
identifier = "qwen3:4b"
```

### Repo `.agents/outrig/config.toml`

```toml
default-image = "coding"
default-agent = "coding"

[network]
mode = "audit"   # the only `[network]` key a repo config may set

# No `[providers.<name>]` -- a provider with an `api-key` belongs in the global config; the
# agents below name the global models.

[workspace]
host-path      = "."
container-path = "/workspace"

[[workspace.mounts]]
host-path      = "../shared-docs"
container-path = "/resources/shared-docs"

[[workspace.mounts]]
host-path      = "/var/tmp/outrig-cache"
container-path = "/resources/cache"
access         = "read-write"

[agents.coding]
# model omitted -> uses global default-model = "fast"
image       = "coding"
preamble    = "You are a careful coding assistant. Repo is at /workspace."
temperature = 0.2
tool-call-max = 300
tool-result-max = 1048576

[agents.review]
model    = "smart"        # explicit override
preamble = "You are a meticulous code reviewer."

[images.coding]
dockerfile = ".agents/outrig/images/coding/Dockerfile"
context    = ".agents/outrig/images/coding"
build-args = { NODE_VERSION = "20" }

  [images.coding.security]
  capability-profile = "no-net-raw"
  cap-drop = ["MKNOD", "SETFCAP"]
  cap-add  = ["NET_BIND_SERVICE"]

  [images.coding.mcp]
  fs    = { command = ["mcp-server-filesystem", "/workspace"] }
  shell = ["bash", "-lc", "exec shell-mcp-command"]
  build = { command = ["cargo-mcp"], env = { CARGO_HOME = "/workspace/.cargo" } }
  tests = { command = ["test-mcp"], call-timeout-secs = 1800 }
```

## What a load sets aside

Every outrig on a machine reads the same global config, so a file written for a newer outrig
should not stop an older one. A load sets aside what it cannot use, prints a warning on stderr
for each thing it set aside, and goes on:

```text
[outrig] warning: /home/you/.outrig/config.toml:5: unknown key `telemetry`, ignored
```

- **An unknown key** is ignored, in the global config, the repo config, or a `--config` file:
  a typo, or a key a newer outrig reads. An unknown table is one warning, not one per key in it.
- **A value outrig cannot read, in the global config**, is dropped with the smallest entry that
  holds it: a wrong type, an unknown variant such as a newer provider `style`, or a missing
  required key. That is the key itself, or one mount of a `workspace.mounts` or
  `sidecars.<sc>.mounts` list; a bad element of any other list drops the whole key, so no argv
  runs short an argument. When what is left of a table cannot stand alone -- a provider without
  its `style` -- the table goes too, with a warning of its own. In a repo config or a `--config`
  file the same value is an error, and when a misspelled required key is what an entry lacks,
  the error names the misspelling.

Validation then runs on what is left, so an entry something else needed is still an error --
printed after the warning that explains it. A misspelled `provider` is warned about, and then
its model is refused for having no provider. Likewise an `env-allow` that is set aside leaves
the [`[repo]`](#repo) list empty, so a repo config's references, and an image label's, are
refused, after the warning that says why. Some things are never set aside:

- anything in `[network]`, `[images.<name>.security]`, or `[sidecars.<sc>.security]`, where
  leaving a key out could give a container more access than the file asks for;
- anything in `[bindings]`, where leaving a key or value out would have the host build an object
  other than the one the file declares;
- a TOML syntax error, which leaves nothing to read;
- a table header with an unquoted `.` in a name, such as `[models.opus-4.7]`, which is refused
  with a hint to quote it: `[models."opus-4.7"]`.

`outrig mcp self`'s `validate_config` tool returns the same warnings beside its errors. An
`org.outrig.mcp` image label and an `image.toml`, which no config load reads, still refuse an
unknown key.

## Validation rules

`outrig run` and `outrig mcp` use the full validation path. `outrig build` validates every
image-config in the merged config but does not require agent/model/provider wiring to resolve.

- `default-image` must name an existing `[images.<name>]` block.
- `default-agent` must name an existing `[agents.<name>]` block. Absence is fine; a name that
  matches nothing is not.
- Every `agents.<name>.model` (if set) must name an existing `[models.<name>]`. If `model` is
  omitted, `default-model` must be set and must name an existing `[models.<name>]`.
- Every `models.<name>.provider` must name an existing `[providers.<name>]`.
- Every `[models.<name>]` must set exactly one of `provider` or `alias`. Setting neither, or
  setting `alias` alongside any provider-shape field (including `max-tokens` and
  `context-window`), is an error naming every offending key. Unlike the other model rules,
  this one and the three below are checked on **every** path, `outrig build` included -- they
  establish the entry's shape rather than resolve a cross-reference.
- Every `alias` must name at least one model; `alias = []` is an error.
- Every name in an `alias` must name an existing `[models.<name>]`.
- Alias entries must not form a cycle. The error names the cycle, e.g. `a -> b -> a`.
- An `alias` chain may be at most 32 hops deep. A graph that deep is a mistake rather than a
  configuration, and the limit is what makes it report as one.
- Every `agents.<name>.image` (if set) must name an existing `[images.<name>]`.
- Every `providers.<name>.style` must be one of `{"openai", "anthropic"}`. Other styles are
  reserved for future Rig adapters and listed as TODO in the providers concept page.
- Every `providers.<name>.api-key` (on a remote style) must match
  `^\$\{[A-Z_][A-Z0-9_]*\}$`.
- Every `providers.<name>.base-url` (on a remote style) must be an `http://` or `https://` URL.
  The error says what is wrong -- no scheme, another scheme, no parse -- without quoting the
  value, which may carry credentials. `outrig build` does not check it.
- Every `[models.<name>]` whose provider has a remote style must set
  `identifier`. The error names the style of the provider the model points at.
- `tool-call-max`, if set at the top level or on an agent, must be between `1` and `2000`.
- `tool-result-max`, if set at the top level or on an agent, must be between `1024` and
  `16777216` bytes.
- `subagent-depth-max`, if set at the top level or on an agent, must be between `1` and `16`
  (`1` disables subagents).
- `subagent-width-max`, if set at the top level or on an agent, must be between `1` and `16`.
- `children-max`, `model-concurrency-max`, and `requests-max`, if set at the top level or on an
  agent, must be between `1` and `1024`, `128`, and `4096` respectively.
- `token-budget`, if set at the top level or on an agent, must be at least `1`.
- `[host] grace-secs`, if set, must be at most `3600`.
- `retry-budget-secs`, if set at the top level or on a remote provider, must be at most `3600`
  seconds. `0` is legal and disables retries.
- `request-timeout-secs`, if set on a remote provider, must be between `1` and `3600` seconds.
  Unlike `retry-budget-secs`, `0` is **not** legal: it is an immediate timeout rather than a
  disabled one, so every request would fail before it could be answered.
- `mcp-call-timeout-secs` at the top level and `call-timeout-secs` on an MCP entry, if set,
  must be between `1` and `3600` seconds. `0` is not legal, for the same reason as
  `request-timeout-secs`. `outrig build` checks both too.
- `[network].mode`, if set, must be `default`, `audit`, or `filter`. `filter` additionally
  requires at least one global `allow` or `deny` entry.
- A repo config may set `[network].mode` only; `[network].default`, `[network].allow`, and
  `[network].deny` are rejected there.
- A repo config may declare no provider: an `openai` or `anthropic` provider in
  `.agents/outrig/config.toml`, or in a `--config` file, is rejected there, naming the provider
  and the variable its key references.
- `[repo].env-allow` may be set only in the global config; a repo config that declares it, even
  as `env-allow = []`, is rejected there. Each entry must be a variable name, matching
  `^[A-Z_][A-Z0-9_]*$`; in the global config a bad entry sets the whole key aside with a warning.
- Every `${VAR}` in the `build-args` or an MCP `env` of an image that `.agents/outrig/config.toml`,
  or a `--config` file, declares must name a variable the global config's `[repo].env-allow`
  lists. The load is refused otherwise, naming the image, the build-arg or the server and its
  `env` key, the variable, and the file. Images the global config declares, literal values, and
  `--env` values are not held to the list.
- Every `${VAR}` in the `env` of a server an image's `org.outrig.mcp` label declares must name a
  variable the global config's `[repo].env-allow` lists, whoever declared the image. It is the
  session start that is refused, naming the server, the `env` key, the variable, and the image,
  because the label is read off the image rather than from a config file. A server the config
  declares by name replaces the label's entry whole and is not held here, and `outrig image
  build` does not hold the `image.toml` it is pointed at.
- Every server name in `[images.<name>.mcp]` must match `^[a-zA-Z][a-zA-Z0-9_-]*$` and be
  unique within its image-config.
- Every `command` array must be non-empty.
- `sidecar` and `image` are mutually exclusive on an MCP entry, `sidecar` must name a declared
  `[sidecars.<sc>]` block, `image` must not be empty, and an `image` entry's server name must
  not collide with a sidecar name (it occupies that name).
- `args` on an MCP entry requires `sidecar` or `image`, and is rejected next to `command`.
- `args` on `[sidecars.<sc>]` requires that some image-config host an entrypoint-stdio server
  in that block, and the same container's arguments must not also be declared on the MCP entry.
- An entrypoint host hosts exactly one MCP server and must be `start = "auto"`.
- `args` is rejected in an `org.outrig.mcp` label and in standalone `image.toml`, alongside the
  placement keys: labels declare exec-stdio servers, whose arguments belong in `command`.
- `call-timeout-secs` in an `org.outrig.mcp` label or a standalone `image.toml` is held to the
  same `1` to `3600` range: `outrig image build` refuses it before stamping the label, and a
  session refuses a label that carries one out of range.
- `dockerfile` and `context` must exist on disk, resolved against the declaring file's directory,
  or your home directory for a leading `~` (build path only). The error names both the path as
  written and the file that declared it.
- Each `[images.<name>]` must set exactly one of: `image-name`, or `dockerfile` + `context`.
  Setting both shapes, neither, `image-name` with `build-args`, or only one of
  `dockerfile`/`context` without the other is an error.
- `image-name` must not be empty.
- A build-from-Dockerfile `[images.<name>]` block key must be a valid container image
  repository component -- lowercase letters and digits, separated by one `.`, one or two `_`,
  or a run of `-` (`^[a-z0-9]+(([._]|__|-+)[a-z0-9]+)*$`), the separators podman accepts -- and
  at most 237 characters, because it becomes the built image's repository and buildah refuses
  one over 255 once it prepends `docker.io/library/`. Image-name configs are exempt: their block
  key is just a label.
- Every `[images.<name>.security].capability-profile`, if set, must be one of
  `default`, `no-net-raw`, or `drop-all`.
- Every capability name in `cap-drop` or `cap-add` must be non-empty and match
  `^[A-Z0-9_]+$` after optional `CAP_` stripping.
- Capability names must not be duplicated within `cap-drop` or within `cap-add`, after
  optional `CAP_` stripping.
- The same normalized capability name must not appear in both `cap-drop` and `cap-add`.
- Every `[images.<name>.security].devices` entry must be non-empty and shaped as podman parses
  it: `<host-path>[:<container-path>][:<permissions>]`, at most three fields, the host and
  container path absolute or empty (meaning the host path), `<permissions>` each of `r`, `w`, `m`
  at most once, or empty (podman allows that for a device directory and refuses it for a single
  node, so validation leaves it to podman). `:` is accepted here but not in `unmask` because
  podman splits an `unmask` value into several paths, while the colons of a `--device` value
  separate the parts of one device.
  Whether the node exists is not checked -- validation may run on a machine that is not the
  launch host, so podman reports a missing node at launch instead.
- Device paths must not be duplicated within one `devices` list.
- Every `[images.<name>.security].unmask` entry must be non-empty and either an absolute path
  or the literal `ALL`.
- `ALL` must be spelled in exact uppercase, and must be the only entry in the list. Measured
  against podman 5.7, both near-misses fail silently: lowercase `all` still clears the masked
  paths -- so `/proc/acpi` looks unmasked -- while leaving the read-only ones, so
  `/sys/fs/cgroup` stays read-only. `["/proc/*", "ALL"]` does the same, because podman lifts
  the read-only paths only when `ALL` comes first. outrig rejects both rather than silently
  reordering the list; every other entry is redundant beside `ALL` anyway.
- Every non-`ALL` entry must be a well-formed `filepath.Match` glob -- no unterminated `[`
  class, no dangling `\` escape. Podman logs a pattern syntax error for a malformed one and
  creates the container with the path still masked, so an unchecked typo would go quiet at
  launch and resurface later as an unrelated-looking failure.
- No `unmask` entry may contain `:`. Podman splits an unmask value on it, so a colon-joined
  entry would be several paths wearing one entry's clothes; declare one path per entry.
- Unmask paths must not be duplicated within one `unmask` list.
- `session-root` may be set only in the global config. A relative value resolves against that
  file's directory, and a leading `~` is your home directory; outrig creates it if missing.
- `workspace.host-path` and every `workspace.mounts[*].host-path`, if validated with a repo root,
  must exist and be a directory. Relative host paths resolve against the declaring file's
  directory, and a leading `~` is your home directory; an undeclared `workspace.host-path` is
  `.`, the repo root.
- `workspace.container-path` and every `workspace.mounts[*].container-path` must be absolute and
  must not be `/`; a mount's left unset is its resolved host path.
- Extra workspace mount `container-path` values must be unique, including no collision with the
  primary workspace `container-path` -- its host path, when the config declares a binding.
  Container paths are compared after folding `.` and `..`, so `/workspace/../workspace`
  duplicates `/workspace` and `/a/..` is `/`.
- Every `[bindings.<name>]` name must be a Python identifier of ASCII letters, digits and `_`,
  not starting with a digit, and not a Python keyword. `runtime`, `asyncio`, `outrig` and
  `__dunder__` names are taken in every kernel and rejected too.
- Every `bindings.<name>.factory` must be `module:callable`, the module a dotted path.
- Every `bindings.<name>.description` must not be empty.
- Every `bindings.<name>.requires` entry must be a requirement specifier: not a path, an archive
  name, a URL, a direct reference or a pip option.
- A binding's `serialize`, if set, must be a boolean.
- With any binding declared, a `workspace.container-path` or `workspace.mounts[*].container-path`
  that is declared must be its resolved host path. The error names the key.
- Every `workspace.mounts[*].access`, if set, must be either `read-only` or `read-write`.
- An unknown key, at any level, is ignored with a warning rather than rejected, except in
  `[network]` and the `security` tables; see [What a load sets aside](#what-a-load-sets-aside).

## See also

- [Concepts](https://tgockel.github.io/outrig/concepts/index.html) -- narrative explanations of what
  these keys mean.
- [Reference -> CLI](https://tgockel.github.io/outrig/reference/cli.html) -- flags that override or
  interact with config.
