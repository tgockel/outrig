# Security Policy

## Supported versions

OutRig is pre-1.0 and ships from a single `trunk` line. Security fixes land on the latest
`0.2.x` release; there are no separate maintenance branches for older patch releases yet.

| Version | Supported |
| ------- | --------- |
| 0.2.x   | yes       |
| < 0.2   | no        |

## Reporting a vulnerability

Please report security issues privately -- do **not** open a public GitHub issue for a
suspected vulnerability.

- Preferred: GitHub private vulnerability reporting. On the repository, go to
  **Security -> Advisories -> Report a vulnerability**. This opens a private channel with the
  maintainer.
- Alternatively, email **travis@gockelhut.com** with details and, ideally, a reproduction.

This is a small, single-maintainer project. Expect an initial acknowledgement within about a
week. Once a fix is ready it ships in the next `0.2.x` release, with credit in the CHANGELOG
unless you ask to remain anonymous.

## Security model

OutRig's security property is the **container boundary**. What the agent runs executes inside a
podman-managed container: under `outrig run`, the MCP servers and the tools they run; under
`outrig run-new` (0.3), the agent's own Python and every program it starts. The host stays outside
that boundary except for the workspace mount(s), the runtime services OutRig explicitly connects,
and the bindings an operator declares, which act on the host (see below). See
[`doc/concepts/mcp-trust-model.md`](doc/concepts/mcp-trust-model.md),
[`doc/concepts/hosted-objects.md`](doc/concepts/hosted-objects.md) and
[`doc/concepts/containers.md`](doc/concepts/containers.md) for the full model.

Issues that bear on **host integrity** are in scope, for example:

- A path by which a container escapes the rootless-podman boundary to reach host files or
  processes outside the configured workspace mount(s).
- OutRig leaking host secrets, credentials, or environment into the container when not
  configured to, including through a podman default OutRig leaves in place, such as
  `--http-proxy`, or, under `outrig run-new`, through an MCP server's or a sidecar's environment:
  `run-new` starts neither, so no credential from those blocks reaches its container.
- Agent code starting a container, a sidecar, an MCP server, a binding, or a service. Those come
  from the configuration, the operator, an embedder, or a host program's `inject`, never from the
  agent: the agent cannot grow its own environment.
- The network interceptor failing to enforce a configured host:port allow/deny policy.
- The network interceptor attaching to a container that holds `CAP_NET_ADMIN` -- `cap-add` of
  `NET_ADMIN`, or of `ALL` without `NET_ADMIN` in `cap-drop` -- since whatever it runs as root
  could delete the nftables table that is the interception. Attaching refuses such a container.
- The network interceptor dialing, from the host's network namespace, an address that means
  something only inside the container's -- its loopback, or the interceptor's own listener --
  so that a container reaches a host service the policy never named.
- A name the container merely asserts -- a TLS `ClientHello` SNI or an HTTP `Host:` header --
  satisfying a hostname `allow` entry. A hostname rule grants only against a destination the
  interceptor itself resolved to that name.
- A repo config's `[network]` block reaching the egress policy. A repo may choose
  `[network].mode`; `default`, `allow`, and `deny` belong to the operator's global config, and
  a repo value carrying them must not widen or replace what the global config set.
- A repo config binding a host secret: a `[providers.<name>]` entry that carries an `api-key`
  belongs to the operator's global config, and a repo config -- committed, or edited by the
  agent through the read-write workspace -- that declares one must be refused rather than
  handed the operator's key to send to a `base-url` of its choosing.
- A repo config reading a host variable it was not allowed: a `${VAR}` in `build-args` or in an MCP
  server's `env` hands the variable's value to code the repo controls, so a repo config file --
  committed, or edited by the agent through the read-write workspace -- may name only the
  variables the global config lists in `[repo] env-allow`, and one that names another must be
  refused rather than resolved. So must a server an image's `org.outrig.mcp` label declares: the
  label is the image author's text -- a Dockerfile in the repo, or an image the repo names --
  and not a config file, but a reference in its `env` is read from the host and handed to that
  server all the same.
- Capability handling that is more permissive than the selected capability profile
  (`default` / `no-net-raw` / `drop-all`).
- A container launched more permissively than its `[images.<name>.security]` block asks for --
  `--security-opt=no-new-privileges` missing when `no-new-privileges` was not set to `false`,
  or a device reaching a container that did not list it in `devices`.
- A binding a repository's config declares (`[bindings.<name>]`) starting without the
  operator's approval of that exact declaration in that repository. A binding's factory is code
  the host runs as the user, and a repository's config is content the agent can write.
- Code in the container reaching the process that holds a session's bindings, or a host program,
  other than through the requests OutRig relays to it; a request to a binding or a live object
  running without being published as events; or one admitted after the session's owner closed
  admission. These checks are on the host so that nothing the container sends -- raw RPyC frames
  included -- gets past them. The same holds of a call of a service's method, and so does a call
  reaching its service with arguments its method's schema refuses.

## Known boundaries (by design, not vulnerabilities)

- **The agent has arbitrary code execution _inside_ the container.** That is the point --
  there are no command allowlists within a normal OutRig container. Anything reachable through
  the configured workspace mounts and network policy is reachable by the agent.
- OutRig relies on **rootless podman** for isolation. Beyond `--userns=keep-id`,
  `--security-opt=no-new-privileges`, and the selected capability profile, it does not add
  seccomp, AppArmor, or SELinux policy, a read-only root filesystem, or network egress policy
  in the container launch path.
- An image-config may opt out of `--security-opt=no-new-privileges` and may pass host device
  nodes through with `devices`. Both default to off, and both weaken the boundary when set: a
  container without `no_new_privs` can use a setuid-root binary in its own image to reach
  namespace-local root, and a passed-through device is real hardware access. Configuring
  either is a deliberate choice by whoever owns the config, not a vulnerability.
- A sidecar with **`view = "primary"`** is inside the primary's trust boundary, not beside it.
  It joins the primary's mount namespace with `CAP_SYS_ADMIN`/`CAP_SYS_PTRACE` (scoped to the
  rootless user namespace, not host root) and can read the primary's entire filesystem -- so its
  image is as trusted as the primary image. It is opt-in, defaults to `"none"`, and remains a
  container (cgroups, seccomp, network policy, and `no-new-privileges` still apply; only the
  mount namespace is joined). See [Trust Model](doc/concepts/mcp-trust-model.md).
- A proxy handed to an MCP server through its `env` is the agent's to read, credentials
  included, and a filter policy that allows it allows whatever it will reach. See
  [Containers](doc/concepts/containers.md#the-containers-environment).
- A variable listed in `[repo] env-allow` is the repo's to route. The repo picks which build-arg
  or MCP server environment receives it -- in its config, or through an image label it controls
  -- and whatever runs there can use it as far as the container allows. List only variables you
  would hand to everyone who can change that repo's config or the images it builds or names, the
  agent included. See [Config reference](doc/reference/config.md#repo).
- Network filtering is **host:port allow/deny plus DNS and audit logging**, not TLS
  interception. HTTPS MITM is explicitly deferred to a later release.
- A **binding** (`[bindings.<name>]`, 0.3) runs a library in a process **on the host, outside the
  container**, with the host user's authority: their files, their `ssh-agent`, their credential
  helpers, and every variable of their environment but the ones OutRig's config names through
  `${VAR}`. One process holds every binding of a session, so every binding's library runs with that
  one environment. Whatever the library does on the host is part of what the binding grants --
  GitPython runs `git`, which runs the hooks and `core.fsmonitor` a repository names, and the agent
  can write `.git/` through the workspace mount. The user's authority covers OutRig's own files too
  -- the global config and the record of approved bindings -- so a binding through which the agent
  can run host programs can change what later sessions are granted. A binding acts on the host, so a
  mount's `read-only` access does not limit it. Every request the agent's code makes of a binding
  runs, and is published as events: having the object is permission to use it. The events are a
  record only where something keeps them -- `outrig run-new` writes them to `events.jsonl` only
  under `[events] mode = "record"`. Binding a library is a deliberate grant by whoever wrote the
  config, not a vulnerability.
- A **host program** (`outrig run-new <script.py>`, 0.3) is trusted code you run on the host, with
  your authority and your environment. It is not a sandbox, and OutRig neither confines nor
  audits it. A live object it injects into its sessions (`session.inject`) acts with the
  program's authority, and every request an agent makes of it is published as a binding's is.
  Keep the program's imports, and the credentials it reads, outside paths an agent can write.
- A **service** (`SessionBuilder::serve`, 0.3) is Rust the embedder runs on the host, with the
  embedder's authority. Every method it describes is callable by every agent of the session, and
  every call is published as events: binding the service is the grant, and a service that should
  do less describes less. OutRig checks a call's arguments against the method's JSON Schema and
  nothing else: a service resolves what it acts on from its own configuration rather than from an
  argument naming it, and rechecks state when it acts. What a service puts in an error message or
  a result reaches the agent's Python as written; OutRig adds nothing of its own and redacts
  nothing of the service's. A call abandoned at its deadline or at the close is reported
  `unknown`, since what it did before the abort stands, and a call blocking a thread runs on in
  the embedder's process. A service written by whoever embeds OutRig is that embedder's trusted
  code, not a vulnerability.
- **Application code** (`SessionBuilder::application`, `outrig.Session(application=...)`, 0.3)
  runs in the container with the agent's authority, which is arbitrary execution there: it is the
  owner's code, trusted as the owner is. Its snapshot is mounted read-only so that the code the
  session ran is the code the owner supplied, which its digest attests -- not so that the agent
  cannot change its behavior: the agent's code and the application's share one interpreter, and
  a monkeypatch is plain Python. Anything the process can read, the agent can print, so
  application code holds no credential: a client that needs one is a binding on the host, which
  the application is given a reference to.
- **Agents in one `outrig run-new` session are not isolated from each other.** They share one
  interpreter process, a thread each, so any agent's code can reach another's names, and every
  agent can reach every binding and service. An agent class given `facilities=` is told of those
  alone, which narrows what it is told and not what it can do. The grant is the session's, not
  one agent's.
- Hostname **deny** rules are best-effort against a hostile client. A container that resolves a
  name through the interceptor is bound to it and cannot shed that binding, but one that
  connects by address and asserts no name at all offers nothing to match against. Hostname deny
  rules describe traffic, not a containment boundary; bound a hostile container with address,
  CIDR, and port rules, or with `default = "deny"`.
- A hostname **allow** rule grants the addresses its name resolved to, not the site: on shared
  hosting or a CDN, a container that resolved the allowed name can reach any other site served
  from that address. Name a host in `allow` only where you would trust everything served there.
- Under `default = "deny"` a container resolves only the names an `allow` hostname entry could
  match, and every lookup is recorded; address and CIDR `allow` entries open no lookups. An
  allowed name can still carry data in its labels to that zone's name servers, so a wildcard
  such as `*.example.com` is only as tight as the zone is trusted. Under `default = "allow"`
  lookups are open apart from names a hostname `deny` entry matches.

Treat the container as you would any environment where you grant an agent real tools: put only
what the agent should be able to use inside the boundary.
