# Security Policy

## Supported versions

OutRig is pre-1.0 and ships from a single `trunk` line. Security fixes land on the latest
`0.1.x` release; there are no separate maintenance branches for older patch releases yet.

| Version | Supported |
| ------- | --------- |
| 0.1.x   | yes       |
| < 0.1   | no        |

## Reporting a vulnerability

Please report security issues privately -- do **not** open a public GitHub issue for a
suspected vulnerability.

- Preferred: GitHub private vulnerability reporting. On the repository, go to
  **Security -> Advisories -> Report a vulnerability**. This opens a private channel with the
  maintainer.
- Alternatively, email **travis@gockelhut.com** with details and, ideally, a reproduction.

This is a small, single-maintainer project. Expect an initial acknowledgement within about a
week. Once a fix is ready it ships in the next `0.1.x` release, with credit in the CHANGELOG
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
  configured to.
- The network interceptor failing to enforce a configured host:port allow/deny policy.
- Capability handling that is more permissive than the selected capability profile
  (`default` / `no-net-raw` / `drop-all`).

## Known boundaries (by design, not vulnerabilities)

- **The agent has arbitrary code execution _inside_ the container.** That is the point --
  there are no command allowlists within a normal OutRig container. Anything reachable through
  the configured workspace mounts and network policy is reachable by the agent.
- OutRig relies on **rootless podman** for isolation. Beyond `--userns=keep-id`,
  `--security-opt=no-new-privileges`, and the selected capability profile, it does not add
  seccomp, AppArmor, or SELinux policy, a read-only root filesystem, or network egress policy
  in the container launch path.
- Network filtering in 0.1 is **host:port allow/deny plus DNS and audit logging**, not TLS
  interception. HTTPS MITM is explicitly deferred to a later release.

Treat the container as you would any environment where you grant an agent real tools: put only
what the agent should be able to use inside the boundary.
