# Security boundary

Where the line actually is, once an agent has arbitrary Python execution. `pyro-remote-objects.md`
covers the mechanism for reaching across a boundary; this page decides what the boundary is,
because the mechanism does not establish one by itself.

The boundary has two halves, and they fail independently. *Placement* decides whether the
trusted side can be inspected or interfered with by the process calling it, and is what the
rest of this page is about. *Mediation* decides what the calls that cross it are allowed to do,
and is `call-inspection.md` -- necessary because the transport supplies no per-call
authorization whatsoever. Getting placement right and mediation wrong yields a process the
sandbox cannot read but can ask for anything.

Two Pythons appear on this page once credentials move behind a boundary, and confusing them
inverts the design. The **agent interpreter** is the static CPython in the primary container, the
one the model submits code to; it holds proxies and never a credential. The **trusted
interpreter** is whatever runs the real objects on the other side of the socket, which this page
places in a sidecar. Unqualified "the interpreter" means the agent interpreter.

Nothing here is built in the first milestone. Until it is, OutRig starts nothing in the agent's
containers that holds a credential from its config, and that is the only reason the gap is
tolerable. It is why `run-new` starts no MCP server: one placed in the primary would hold its
secrets beside the interpreter as the same user, and not handing the model its tools would not
put them out of reach. What the container runtime passes in on its own is not covered: podman
forwards the host's proxy variables by default, and a proxy URL can carry a password
(`plan/next/proxy-credentials-reach-the-primary.md`).

## OutRig's existing property, unchanged

The security property is the container. The agent has arbitrary execution inside it and that is
the point; the host stays outside except for the workspace mount and the runtime services OutRig
connects. Python does not weaken that -- the agent interpreter runs in the container that
already granted arbitrary execution, not on the host.

What Python changes is the *interior*. Before, an agent acted through MCP tools, and a tool
holding a credential could keep it in its own process. Now agents write code in a process they
share, and anything that process can read, any of them can print.

## Why a second process in the same container is not enough

The tempting arrangement is a trusted daemon beside the interpreter, same container, talking
over a Unix socket. It is not a boundary, and the reason is worth being precise about rather
than hand-waving.

Both processes run as the session user. Same-UID code can send signals to the daemon, read
`/proc/<pid>/` entries for it, and on a host whose `yama.ptrace_scope` permits it, attach to the
process and read its memory outright. A Unix socket controls who may *call* the daemon; it says
nothing about who may inspect it. So the daemon is a memory boundary in the sense that the token
is not in the interpreter's own address space, and nothing stronger.

That is not worthless -- it stops a credential appearing in a traceback, an inventory, or a
careless `print(os.environ)` -- but it must not be described as isolation, because the
difference matters the moment someone decides what to put behind it.

## Agents are not isolated from each other, and it does not matter

Agents are co-hosted in one interpreter process, one per thread (`agent-placement.md`), so any agent
can reach any other's names through `sys.modules`. That sounds like it should be a finding. It is
not, for two reasons that predate the choice.

**The grant is per-session, not per-agent.** `doc/concepts/mcp-trust-model.md` is explicit that a
subagent "can call nothing the operator did not already grant the session -- it is a second
consumer of a fixed environment, not a way to widen one." There is no per-agent capability for a
co-hosted sibling to borrow, because there is no per-agent capability.

**The credential boundary is the socket, and the socket is container-level.**
`pyro-remote-objects.md` states the access control exactly: what is exposed, and who can open the
socket. Anything in the primary container can open it -- any agent, and equally any subprocess an
agent spawns. A process boundary between agents would not have changed that, which is worth
noticing before anyone proposes one as a fix.

What co-hosting does introduce is interference rather than disclosure: a shared working directory,
a shared environment, shared module state. `agent-placement.md` lists it, and it belongs beside
the existing warning that two subagents told to edit the same file will fight over it.

## Two ways to make it real

**A. Restrict the container so inspection is not available.** OutRig already controls the
container's configuration: capability profiles, `no_new_privileges`, and the `unmask` list are
existing config surface. Dropping `CAP_SYS_PTRACE`, keeping `no_new_privileges`, and mounting
`/proc` with `hidepid` would close the obvious paths.

The problem is what it rests on. `yama.ptrace_scope` is a host sysctl OutRig does not set and
cannot promise; `hidepid` interacts with the user namespace; and a same-UID process retains ways
to interfere short of reading memory. It would be a boundary whose strength depends on host
configuration OutRig cannot see, which makes it hard to state honestly in documentation.

**B. Put the trusted side in a sidecar container.** Preferred. OutRig already builds, places,
and connects sidecars -- that machinery exists and is what phase 0002 was about. A sidecar has
its own PID namespace and its own filesystem, so the interpreter cannot signal it, cannot read
its `/proc`, and cannot attach to it. The socket becomes the only channel, which is what the
`@expose` surface was already assumed to be.

It is also more flexible than a boundary built out of restrictions. A sidecar can have mounts
the primary does not, a different network policy, and its own image with whatever the trusted
code needs -- including a normal CPython with pip, which is what makes the MCP client question
in `mcp-wrappers.md` answerable at all. The boundary and the capability arrive together.

The cost is a container per trusted surface and the connection work to reach it, on top of the
serialization already required.

## Open questions

- Whether one sidecar holds every trusted object or there is one per credential domain. Per
  domain is the stronger claim and the larger build.
- How the socket is shared. A sidecar has its own filesystem, so the socket needs a mount both
  sides see, or the transport is TCP on a private network rather than a Unix socket -- which
  changes what `pyro-remote-objects.md` assumes.
- What the interpreter is told. A proxy it can call is also a proxy it can enumerate, and the
  exposed surface is the whole authorization model unless `call-inspection.md` is built, which
  is the only thing that would add a per-call decision.
- Whether option A is worth doing anyway as depth, given it is mostly existing config.
- What the threat model actually is. The model is not assumed hostile; it is assumed capable of
  mistakes and of being steered by content it reads. A design that only survives a benign model
  should say so rather than implying more.

## What this does not change

The agent still cannot grow its own environment. A sidecar for trusted objects comes from
OutRig's configuration, the same as every other sidecar; there is no agent-callable operation
that creates one. That rule is load-bearing in `doc/concepts/mcp-trust-model.md` and this phase
does not touch it.
