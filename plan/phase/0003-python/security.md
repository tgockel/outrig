# Security boundary

Where the line is, once an agent has arbitrary Python execution and some of what it uses runs on
the host. `hosted-objects.md` is the mechanism for reaching across, and `boundary-policy.md`
decides what crosses; this page decides where the trusted side runs, what it grants, and what the
design claims about it.

The boundary has two halves, and they fail independently. *Placement* decides whether the trusted
side can be inspected or interfered with by the code calling it. *Mediation* decides what the calls
that cross it are allowed to do. Getting placement right and mediation wrong yields a process the
sandbox cannot read but can ask for anything -- and under the default this phase ships, that is
what a binding is, deliberately, until the operator tightens it.

Two Pythons appear on this page, and confusing them inverts the design. The **agent interpreter**
is the static CPython in the primary container, the one the model submits code to; it holds
proxies, and OutRig puts no credential in it. A **binding process** runs one hosted library on the
host, with the same static CPython and whatever environment the binding was given. Unqualified
"the interpreter" means the agent interpreter.

None of this is built yet. `0003-18` is a spike that proves binding processes and their supervision,
and `0003-20` builds them, with the declarations, the approval of repo-declared ones, and same-path
mounts; `0003-22` builds the interception and its events, and `0003-23` the policy. `SECURITY.md`'s
"Known boundaries" gains its entry with `0003-20`: a binding acts with the host user's authority,
and the default -- allow, published as events -- runs every call and publishes it.

Until then, OutRig starts nothing that holds a credential from its config where the agent can
reach it. That is also why `run-new` starts no MCP server: one placed in the primary would hold its
resolved secrets beside the interpreter, as the same user, and MCP servers presented as Python
objects are not in this phase (`mcp-wrappers.md`). What the container runtime passes in on its own
is not covered: podman forwards the host's proxy variables by default, and a proxy URL can carry a
password (#455).

## OutRig's existing property, unchanged

The security property is the container. The agent has arbitrary execution inside it and that is
the point; the host stays outside except for the workspace mount, the runtime services OutRig
connects, and -- with this phase -- the bindings the operator declares. Python does not weaken
that: the agent interpreter runs in the container that already granted arbitrary execution, not on
the host.

What Python changes is the *interior*. Before, an agent acted through MCP tools, and a tool
holding a credential could keep it in its own process. Now agents write code in a process they
share, and anything that process can read, any of them can print.

## Why a second process in the same container is not enough

The tempting arrangement is a trusted daemon beside the interpreter, in the same container,
talking over a Unix socket. It is not a boundary, and the reason is worth stating precisely.

Both processes run as the session user. Same-UID code can send signals to the daemon, read
`/proc/<pid>/` entries for it, and on a host whose `yama.ptrace_scope` permits it, attach to the
process and read its memory outright. A Unix socket controls who may *call* the daemon; it says
nothing about who may inspect it. So the daemon is a memory boundary in the sense that the token
is not in the interpreter's own address space, and nothing stronger.

That is not worthless -- it stops a credential appearing in a traceback, an inventory, or a
careless `print(os.environ)` -- but it must not be described as isolation, because the difference
matters the moment someone decides what to put behind it. It is why the trusted side is not in the
container at all.

## The trusted side runs on the host

**A hosted object runs on the literal host, in a process of its own per binding**, supervised by
the Rust owner (`hosted-objects.md`). Three reasons:

**What is worth hosting is on the host.** The repository the agent works on, its remotes, and the
user's credentials -- the ssh-agent, the credential helpers, the git config -- are where the user
is. A library running there uses them as the user's own tools do. A sidecar would have to be given
each of them -- the same authority, passed into a container -- and there would be a container to
build for every trusted library.

**The host has the property the same-container daemon lacks.** The container has its own PID
namespace, so code in it cannot name a host process: it cannot signal a binding process, read its
`/proc` entries, or attach to it. The only channel is the frames the Rust owner relays, and only
the interpreter has one. That is the property this page used to prefer a sidecar for, and the host
has it without another container.

**One process per binding keeps bindings apart.** Each binding process has only the environment
it was given, so credentials an embedder supplies to one binding (`embedding.md`) are not in
another's process. A reference from one binding cannot be passed to another, so one binding's
credentials never act on another binding's objects. The CLI supplies no environment: every binding
inherits the user's, so under the CLI the bindings hold the same authority, and the separation
matters when an embedder gives them different environments.

This reverses what the page used to say. It weighed two ways to make a trusted side real:
restricting the container so a daemon in it could not be inspected -- dropping `CAP_SYS_PTRACE`,
mounting `/proc` with `hidepid` -- and putting the trusted side in a sidecar, which it preferred.
Neither is needed for this now. Nothing trusted runs in the container, so nothing in it needs
hiding, and the trusted side needs the host's repository, remotes, and credentials, which a sidecar
would have to be handed.

## What a binding grants

A hosted object acts with the host user's authority, and whatever its library does on the host is
part of the grant -- including what the call expression does not show. With GitPython as the
example:

- **Programs the repository names.** Host `git` runs programs from repository config --
  `core.fsmonitor` on most commands that read the working tree, hooks on commit and push, filter
  and diff drivers -- and `.git/` is inside the workspace, which the agent can write. Git's
  `safe.directory` check does not intervene: it refuses repositories owned by another user, and
  with `--userns=keep-id` what the agent writes is owned by the user. So `repo.index.commit("...")`
  can run a hook the agent wrote, as the user, with the binding's environment.
- **Any command.** `repo.git.execute([...])` runs any argv, with its `env` and `shell` arguments
  passed through.
- **The user's ssh-agent.** A binding that inherits `SSH_AUTH_SOCK` can sign and authenticate with
  every key the ssh-agent holds -- push, fetch, log in to a host -- but cannot export a key: the
  agent protocol has no operation that returns one.
- **The user's credential helpers.** These do return secrets. `git credential fill` prints the
  username and the password or token a helper holds, to whatever asks.
- **The environment.** The CLI reads provider keys from the environment (`api-key = "${VAR}"`), and
  a binding inherits that environment, so a library that runs programs can print them.
- **OutRig's own files.** The host user's authority includes the global config, where bindings need
  no approval; the approval store; and the package cache that binding processes import from. A
  binding through which the agent can run host programs is one through which it can change what
  later sessions are granted.

This is documented, not policed. The transport does not limit it, and policy decides which
requests run, not what a request does on the host. Confining a hosted library's effects is
`plan/next/hosted-effect-confinement.md`.

So what a binding grants is decided by what is bound, and the recommendation for a consequential
action is a narrow binding the operator owns rather than the library that performs it
(`boundary-policy.md`, "Narrow bindings for consequential actions"): an object whose methods name
the intended effect -- `publish_commit(repo_id, commit_oid, destination_id, expected_remote_oid)` --
resolve their identifiers through configuration the operator wrote, recheck the expected remote
state when they execute, and control the hooks, helpers and configuration the operation runs with.
Its arguments are by value, so an approver's preview shows the intended effect, and the binding's
own recheck at execution is what makes the decision hold (`boundary-policy.md`). Binding the library
itself, a `Repo` with everything reachable from it, is an affirmative choice to grant the host
user's authority with events as the record; renaming a generic method proves nothing, since the
library under it still runs whatever the repository names.

**The old guarantee, restated.** OutRig still puts no credential from its config into the sandbox,
and a binding does not move one there: credentials stay in the binding process's environment, the
user's ssh-agent, and the user's credential helpers. What a binding adds is a way to use them from
the sandbox, and, through a library that runs programs, a way to read whatever the binding process
can read. Policy cannot prevent it: an allowed commit runs a hook the agent wrote, with the
binding's environment. Keeping a credential from the agent means giving the binding process an
environment without it -- which the embedding API can do per binding and the CLI does not -- or
the confinement that entry describes.

## Repo-declared bindings need the operator's approval

A binding's factory is code the host runs at session start, as the user, before the agent has done
anything: `factory = "os:system"` with an argument is a shell command. The repo config,
`.agents/outrig/config.toml`, is repository content. A cloned project brings one, and the agent can
edit it through the read-write workspace mount. Without a check, a repository -- or an agent in an
earlier session -- would choose what the host runs at the next start.

So a repo-declared binding starts only after the operator approves that exact declaration. OutRig
prompts once and remembers the approval by the declaration's digest; any edit changes the digest
and prompts again. Bindings from the global config or the embedding API need no approval, because
they come from the operator or the embedder directly. `0003-20` builds it, and decides what happens
when nobody can be prompted.

The same reasoning gives the policy rule: a repo's `[policy]` may only add restrictions
(`boundary-policy.md`), as a repo's `[network]` may not widen the operator's egress policy
(`SECURITY.md`). OutRig reads the operator's policy and the evaluator's model from the operator's
own layer -- the global config, or a `Config` an embedder passes to the builder -- which a repo
config cannot redefine.

## The default, and tightening it

With nothing configured, every hosted call runs and is published as events: the default is allow,
published as events (`boundary-policy.md`). With a library that runs programs, that is arbitrary
host execution, recorded. The default exists so an operator sees what an agent actually does before
constraining it, and it is the operator's to tighten: a rule that denies or escalates a type or a
member, a stricter `[policy] default`, or `default = "escalate"` with `/approve` and `/deny` at the
prompt. The events are a record only where something keeps them -- in the CLI, `events.jsonl`, when
`[events] mode = "record"`.

## Agents are not isolated from each other, and it does not matter

Agents are co-hosted in one interpreter process, one per thread (`agent-placement.md`), so any agent
can reach any other's names through `sys.modules`. That sounds like it should be a finding. It is
not, for two reasons that predate the choice.

**The grant is per-session, not per-agent.** `doc/concepts/mcp-trust-model.md` is explicit that a
subagent "can call nothing the operator did not already grant the session -- it is a second
consumer of a fixed environment, not a way to widen one." There is no per-agent capability for a
co-hosted sibling to borrow, because there is no per-agent capability.

**Every kernel reaches every binding.** Each kernel gets connections of its own to each binding
(`hosted-objects.md`), but that is for threading, not authority: every kernel is given every
binding, and any kernel can reach another's proxies through shared memory. The kernel an event
names is diagnostic. A process boundary between agents would not change that, which is worth
noticing before anyone proposes one as a fix.

What co-hosting does introduce is interference rather than disclosure: a shared working directory,
a shared environment, shared module state. `agent-placement.md` lists it, and it belongs beside the
existing warning that two subagents told to edit the same file will overwrite each other's edits.

## The threat model

The model is not assumed hostile. It is assumed capable of mistakes and of being steered by content
it reads -- a README, an issue, a commit message, a web page. The design claims what holds under
that assumption and no more:

- **Interception holds against any code in the container.** A steered model can write anything,
  raw RPyC frames included, so enforcement is on the host and covers every request handler.
  Admission holds the same way: the owner's gate and each binding's closed flag are on the host,
  so nothing in the container can get a request admitted once the owner has closed admission
  (`boundary-policy.md`).
- **What an allowed call does is the operator's grant.** Under the default -- allow, published as
  events -- with a library that runs programs, a steered model can do on the host what the user
  can, and the events record it. Nothing here makes that safe; the operator's policy narrows it,
  and a narrow binding ("What a binding grants") leaves less to narrow.
- **Capture and attribution are not security mechanisms.** Output attribution
  (`agent-placement.md`) holds against ordinary code, and deliberate evasion is outside this model.

## Open questions

- Whether the CLI should remove the variables its own config references -- provider keys above all
  -- from the environment a binding inherits. It would narrow what a library that runs programs can
  print, and would not touch the ssh-agent, the credential helpers, or the files under the user's
  home directory. `0003-20` carries it as a design fork.
- What happens to a repo-declared binding when nobody can be prompted: a non-interactive session,
  or an embedder that loads a repo config. `0003-20` decides.

## What this does not change

The agent still cannot grow its own environment. A binding comes from OutRig's configuration or
from an embedder, the same as every container and sidecar; there is no agent-callable operation
that creates one, and a session's bindings are fixed when it starts. The agent can write the repo
config, which is why a repo-declared binding waits for the operator.
`doc/concepts/mcp-trust-model.md` depends on that rule, and this phase keeps it, with the limit
stated above: it governs what the agent can add, not what a granted binding can do.

## Unverified

- That code in the container cannot address a binding process except through relayed frames
  depends on the container's separate PID namespace: OutRig's launch passes no `--pid` option. It
  has not been tested against a process in the container trying.
- That an ssh-agent cannot export a key is read from the agent protocol's specification, and that
  `git credential fill` returns what a helper holds from git's documentation; neither was run here.
- That `safe.directory` does not refuse an agent-written repository follows from git's ownership
  rule and `--userns=keep-id`'s file ownership, and was not tested.
- That GitPython starts commit hooks with a copy of its environment is read from its 3.2.0 source.
