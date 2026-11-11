# outrig

Run an LLM agent's tools inside a [podman](https://podman.io/)-managed container. This is the
library crate. It does two things:

- **Containers and their tools.** It acquires a container image, starts the container, connects
  the MCP servers running inside it, and hands you a typed surface for listing and calling their
  tools. The entry point is [`Outrig::launch`].
- **An agent that acts by writing Python** ([`harness`]). A session holds a container, a Python
  interpreter in it, and an agent whose one tool runs Python there. Your program starts it,
  drives its rounds, watches its events, and stops it with a report that says whether everything
  it started has stopped. A session can host bindings -- objects a process on the host holds for
  the agent, such as a GitPython `Repo` -- which the agent's Python uses under their names as their
  libraries document, each request checked on the host and recorded; they start before the
  interpreter and stop with the session, and one a repository's config declares only once its
  operator approves it. `outrig run-new` is built on it and nothing else of the loop. A Python
  program on the host, run by [`harness::HostProgram`], owns sessions with no agent of their own
  through `outrig.Session`, drives the agent classes it constructs in them, whose replies it
  decodes into its own classes, and serves their agents objects it holds with
  `session.inject` -- live objects, each operation on one run in the program and recorded as a
  binding's is -- and whatever sessions it leaves open the engine stops and reports. A session
  serves skills -- a `SKILL.md` of instructions and, optionally, Python -- from the
  [`harness::SkillSource`]s it is given, each skill's files fetched from the host as the
  agent's code imports or reads them, with nothing mounted for them; a user's `/name`, sent with
  [`harness::UserChannel::send_directive`], asks the agent to run one, which its code does with
  `outrig.skills.invoke`.

The `outrig` command-line tool lives in the companion
[`outrig-cli`](https://crates.io/crates/outrig-cli) crate.

Every session's container gets OutRig's own static CPython, mounted read-only at
`/outrig/python`, so the image needs no Python of its own. The interpreter is part of the build:
`build.rs` downloads a pinned `python-build-standalone` release once per machine, verifies its
SHA-256, and embeds it, and the first launch unpacks it under `$XDG_CACHE_HOME/outrig/python`.
For a build with no network, set `OUTRIG_PYTHON_ARCHIVE` to a copy of the archive; without either,
the build warns and every launch fails with a message saying so. The same build fetches and
embeds the pinned RPyC wheel that carries hosted-object requests between the interpreter and the
bindings process; `OUTRIG_RPYC_WHEEL` names a local copy of it for a build with no network.
OutRig reserves `/outrig` inside the container, so neither the workspace nor a mount may be
placed there. OutRig's own Python package, `outrig`, is part of the build too, and every session
mounts it read-only at `/outrig/lib`; a session's bindings mount their packages read-only under
`/outrig/bindings`, and a session with bindings, or a host program's, mounts RPyC at
`/outrig/rpyc`.

## Example

```rust,no_run
# async fn example() -> Result<(), Box<dyn std::error::Error>> {
use outrig::config::McpServerSpec;
use outrig::{LaunchSpec, Outrig};
use std::collections::BTreeMap;

// Describe the MCP servers to run inside the container.
let mut mcp = BTreeMap::new();
mcp.insert(
    "fs".into(),
    McpServerSpec::Short(vec!["mcp-server-filesystem".into(), "/workspace".into()]),
);

// Launch a container from an existing image and connect every MCP server.
let spec = LaunchSpec::from_image("my-image:latest", mcp, "/tmp/outrig-logs".into());
let outrig = Outrig::launch(&spec).await?;

// Enumerate the tools every connected server advertises.
for tool in outrig.tools() {
    println!("{}/{}: {}", tool.server, tool.name, tool.description);
}

// Dispatch a single `tools/call`.
let result = outrig
    .call_tool("fs", "list_directory", serde_json::json!({ "path": "/workspace" }))
    .await?;
// `result.content` is the server's blocks in order; `render_text` is the
// single-string view a language model is given.
println!("{}", result.render_text());

// Stop every server, then the container.
outrig.shutdown().await?;
# Ok(())
# }
```

`LaunchSpec` also has `LaunchSpec::build` (build an image from a `Dockerfile`) and
`LaunchSpec::from_config` (drive it from a parsed config), plus builder methods for mounts,
capability profiles, network policy, embedded MCP handling, and the session id its containers
are named and labeled for. By default, OutRig merges MCP servers from an image's
`org.outrig.mcp` label with the launch spec. A `${VAR}` in a label entry's `env` is read from the
launching process's environment, so `Outrig::launch` refuses one the spec does not list:
`LaunchSpec::with_embedded_mcp_env_allow` names the variables a label may reference, and
`LaunchSpec::from_config` takes them from the config's `[repo].env-allow`. Library callers that
want the launch spec's MCP map to be authoritative can opt out of the label:

```rust,no_run
# use outrig::{EmbeddedMcpPolicy, LaunchSpec};
# use std::collections::BTreeMap;
let spec = LaunchSpec::from_image("my-image:latest", BTreeMap::new(), "/tmp/outrig-logs".into())
    .with_embedded_mcp_policy(EmbeddedMcpPolicy::Ignore);
```

## Running an agent

```rust,no_run
# async fn example(config: outrig::config::Config) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
use outrig::harness::{DEFAULT_DRAIN, RoundOutcome, SessionBuilder, SessionError, Verdict};

// Everything the session needs from outside, given before it starts: here, each `${VAR}` its
// model's key names is read from this program's own store rather than the environment.
let mut builder = SessionBuilder::new(config, Some("coding"), None)
    .secrets(|var: &str| (var == "ANTHROPIC_API_KEY").then(|| "sk-...".to_string()));
let mut events = builder.subscribe();
let mut session = builder.start().await?;

// The user reaches the agent through its channel; a round runs on what waits there. A session
// started with `no_primary` has neither.
let user = session.user_channel().ok_or(SessionError::NoPrimary)?;
user.send("summarize the README").await?;
if let RoundOutcome::Ended(end) = session.round().await? {
    println!("{}", end.reply);
}

// Stop it: nothing new is admitted, what runs gets a few seconds, then the container stops.
session.close_admission();
let report = session.shutdown(DEFAULT_DRAIN).await;
assert_eq!(report.verdict(), Verdict::Clean);
# let _ = events.try_recv();
# Ok(())
# }
```

A session started with `no_primary` has no agent of its own: it is what each `outrig.Session` a
Python program run by `outrig run-new <script.py>` enters, the program driving the agents in it.
It resolves no model and asks its `Secrets` for nothing as it starts, and has no rounds and no
`user` channel; its container, interpreter and bindings, its admission, its events and its
shutdown report are the same as a session's with a primary agent:

```rust,no_run
# async fn example(config: outrig::config::Config) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
use outrig::harness::{DEFAULT_DRAIN, SessionBuilder, SessionError};

let mut builder = SessionBuilder::new(config, None, None).no_primary();
let mut events = builder.subscribe();
let mut session = builder.start().await?;
assert!(session.user_channel().is_none());
assert!(matches!(session.round().await, Err(SessionError::NoPrimary)));

session.close_admission();
let report = session.shutdown(DEFAULT_DRAIN).await;
println!("{:?}", report.verdict());
# let _ = events.try_recv();
# Ok(())
# }
```

A session's owner gives it application code -- the Python package its agent classes are in --
with `SessionBuilder::application`: a directory, or the bytes of a tar of one that the owner's
binary carries. The session copies it to a snapshot named for its digest, mounts that read-only
in the container ahead of the workspace, installs the pure-Python packages it `requires`, and
imports the modules it names before any kernel opens; one that does not import fails the start,
naming it. `Session::application` says which code the session runs:

```rust,no_run
# async fn example(config: outrig::config::Config) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
use outrig::harness::{ApplicationSpec, SessionBuilder};

let spec = ApplicationSpec::directory("python/src")
    .imports(["review.agents"])
    .label("review 1.4");
let session = SessionBuilder::new(config, None, None)
    .no_primary()
    .application(spec)
    .start()
    .await?;
let code = session.application().expect("the session's application code");
println!("{} ({:?})", code.digest, code.label);
# Ok(())
# }
```

A session with no primary agent runs the application's own agent instead: its owner constructs a
root from a class the application code holds, in a kernel of its own, where the instance is
`self`. The arguments are data, as JSON, or the session's facilities by the names they were bound
under -- a service `SessionBuilder::serve` bound, a binding -- and never an object of the owner's.
What comes back is a `RootHandle`, never the Python object: `ready()` once the root's orienting
round has ended, or what its launch met instead -- a constructor that raised, with its traceback,
included; `release()`; and, for each method the class marks `@outrig.agent`, `request()`, whose
outcome is the reply as JSON text the root's kernel checked against the method's return type, for
the owner to deserialize into a type of its own. The root's rounds open through its own runner, on
its orienting round and on each request, and never through `Session::round`:

```rust,no_run
# async fn example(config: outrig::config::Config, spec: outrig::harness::ApplicationSpec,
#     task: std::sync::Arc<dyn outrig::harness::Service>)
#     -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
use outrig::harness::{Argument, ConstructSpec, Facility, SessionBuilder};
use serde_json::json;

let session = SessionBuilder::new(config, None, None)
    .no_primary()
    .application(spec)
    .serve("task", task)
    .start()
    .await?;
let root = session
    .construct(
        ConstructSpec::new("review.agents:Reviewer", "fast")
            .arg("task", Argument::Facility(Facility::service("task")))
            .alias("task"),
    )
    .await?;
root.ready().await?;
#[derive(serde::Deserialize)]
struct Verdict {
    path: String,
    ok: bool,
}
let reply = root.request("review", json!("src/main.py"))?.outcome().await?;
let verdict: Verdict = serde_json::from_str(&reply)?;
println!("{}: {}", verdict.path, verdict.ok);
root.release().await;
# Ok(())
# }
```

`SessionBuilder::construct` constructs the root as the session starts, failing the start when it
cannot be, and `SessionBuilder::check` resolves its model and keys before any container exists;
`Session::constructed` is its handle, and `Session::root` finds any root by its instance id.

## The container facade beside `harness`

An application can run an engine of its own over the container facade beside one over `harness`,
in one process. These items keep their 0.2 signatures and behavior on `version/0.3.x` -- in its
pre-releases, the 0.3.0 release, and the 0.3.x releases after it -- until the first application to
embed `harness` has retired the engine it runs over them:

- `Outrig::launch`, `tools`, `call_tool`, `exec_capture` and `shutdown`;
- `image::{ensure_image, compute_tag, compute_tag_for, probe_pulled, probe_cached,
  read_image_env}`;
- `LaunchSpec`, with `with_mount`, `with_mounts`, `with_sidecar` and `with_embedded_mcp_policy`;
- `SidecarSpec`, `MountSpec`, `EmbeddedMcpPolicy`, `McpToolResult`, `ToolHandle` and
  `CapabilityProfile`;
- `OutrigError::{Io, McpStartupFailed, Process}`.

A change to one is announced in this crate's `CHANGELOG.md`, with a migration note, before it
lands, and a removal follows a `#[deprecated]` attribute by at least one tagged release: that is
the notice an application pinned to a git revision gets. The repository's `public-api.txt` records
every public item, and CI fails when the surface and that record differ, so a change to one shows
in review.

A container launched with `Outrig::launch` and a `harness::Session` do not touch each other: each
mints its own session id and names its container for it, each writes its own log directory, and
stopping one stops only its own containers. To run both from one image, ensure it once and give
both the tag `image::ensure_image` returns -- `LaunchSpec::from_image(tag, ..)` for the facade, and
`harness::container_spec(.., Some(&tag), ..)` for a session's `SessionBuilder::container`.
`LaunchSpec::from_config` over a `dockerfile` image block builds the image under the block's name
instead, which is a second image.

## Documentation

Full docs render as an mdbook at <https://tgockel.github.io/outrig/>. Source lives in the
[repository](https://github.com/tgockel/outrig).

## License

Licensed under the Apache License, Version 2.0.
