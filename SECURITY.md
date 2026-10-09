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

OutRig's security property is the **container boundary**. MCP servers and the tools the agent
runs execute inside a podman-managed container; the host stays outside that boundary except
for the workspace mount(s) and the runtime services OutRig explicitly connects. See
[`doc/concepts/mcp-trust-model.md`](doc/concepts/mcp-trust-model.md) and
[`doc/concepts/containers.md`](doc/concepts/containers.md) for the full model.

Issues that bear on **host integrity** are in scope, for example:

- A path by which a container escapes the rootless-podman boundary to reach host files or
  processes outside the configured workspace mount(s).
- OutRig leaking host secrets, credentials, or environment into the container when not
  configured to, including through a podman default OutRig leaves in place, such as
  `--http-proxy`.
- The network interceptor failing to enforce a configured host:port allow/deny policy.
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
- Capability handling that is more permissive than the selected capability profile
  (`default` / `no-net-raw` / `drop-all`).
- A container launched more permissively than its `[images.<name>.security]` block asks for --
  `--security-opt=no-new-privileges` missing when `no-new-privileges` was not set to `false`,
  or a device reaching a container that did not list it in `devices`.

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
  mount namespace is joined). See [MCP Trust Model](doc/concepts/mcp-trust-model.md).
- A proxy handed to an MCP server through its `env` is the agent's to read, credentials
  included, and a filter policy that allows it allows whatever it will reach. See
  [Containers](doc/concepts/containers.md#the-containers-environment).
- Network filtering is **host:port allow/deny plus DNS and audit logging**, not TLS
  interception. HTTPS MITM is explicitly deferred to a later release.
- Hostname **deny** rules are best-effort against a hostile client. A container that resolves a
  name through the interceptor is bound to it and cannot shed that binding, but one that
  connects by address and asserts no name at all offers nothing to match against. Hostname deny
  rules describe traffic, not a containment boundary; bound a hostile container with address,
  CIDR, and port rules, or with `default = "deny"`.

Treat the container as you would any environment where you grant an agent real tools: put only
what the agent should be able to use inside the boundary.
