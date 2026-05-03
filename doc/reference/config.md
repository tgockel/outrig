# Config Reference

outrig reads two TOML files:

- **Global config** -- user/machine-level. Default location: `~/.outrig/config.toml` (or
  `<XDG_CONFIG_HOME>/outrig/config.toml` if `XDG_CONFIG_HOME` is set). Holds
  `[providers.<name>]` and (typically) `[models.<name>]` since those reference API keys and
  model identifiers that belong to the user, not to any one repo.
- **Repo config** at `.agents/outrig/config.toml` -- repository-level, committed to source
  control. Holds `[workspace]`, `[containers.<name>]`, `[agents.<name>]`, and any repo-specific
  providers or models.

Both files use the same schema. Names declared in either are visible everywhere; if a name
appears in both files, the **repo entry wins**. Outrig keys are **kebab-case**; only inner-map
keys whose values map to environment variables (Dockerfile build-args, MCP `env` blocks) keep
their as-written form. Unknown keys are an error -- outrig validates with `deny_unknown_fields`.

## Top level

```toml
# repo config (.agents/outrig/config.toml):
default-container = "coding"
default-agent     = "coding"

# global config (~/.outrig/config.toml):
default-model     = "fast"
session-root      = "/var/lib/outrig/sessions"        # optional; defaults to XDG data dir
model-cache-root  = "/var/cache/outrig/models"        # optional; defaults to XDG cache dir
```

| Key                 | Type   | Required               | Where  | Description                   |
|---------------------|--------|------------------------|--------|-------------------------------|
| `default-container` | string | for `outrig run`       | repo   | Default `--container-config`. |
| `default-agent`     | string | for `outrig run`       | repo   | Default `--agent`.            |
| `default-model`     | string | if agent omits `model` | global | Fallback model name.          |
| `session-root`      | path   | no                     | global | Sessions root dir.            |
| `model-cache-root`  | path   | no                     | global | GGUF download cache dir.      |

`default-container` and `default-agent` belong in the repo config -- containers and agents are
project-scoped. `default-model`, `session-root`, and `model-cache-root` belong in the global
config since they're user/machine-level. Each may also appear in the other file; repo entries
override global by name.

`session-root` defaults to `<XDG_DATA_HOME>/outrig/sessions/` (typically
`~/.local/share/outrig/sessions/`). The CLI flag `--session-root <path>` overrides both the
config value and the default; `--session-dir <path>` (on `outrig run`/`logs`/`discard`) instead
points at one specific session directory. See [Sessions](../usage/sessions.md).

`model-cache-root` defaults to `<XDG_CACHE_HOME>/outrig/models/` (typically
`~/.cache/outrig/models/`). It only matters for `style = "mistralrs"` models configured
with `model-id` -- that's where the auto-downloaded GGUFs land. See
[Concepts -> In-process LLMs](../concepts/in-process-llm.md).

## `[providers.<name>]`

A provider tells outrig how to reach a model -- either a remote HTTPS endpoint that speaks
a known wire format, or a local in-process backend. Multiple providers in either file. Repo
entries with the same name override globals. The accepted `style` values are `"openai"` and
`"mistralrs"`. Which other fields are valid depends on `style`.

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
| `request-timeout-secs` | integer      | no       | `120`   | HTTP timeout for LLM calls.      |

### `style = "mistralrs"`

In-process LLM backed by the [`mistralrs`](https://crates.io/crates/mistralrs) crate. No
HTTP, no API key. The provider table is bare -- just the `style` tag. Each set of
weights is its own `[models.<name>]` row referencing this provider, so a single
`mistralrs` provider can back many models. See
[Concepts -> In-process LLMs](../concepts/in-process-llm.md).

```toml
[providers.local]
style = "mistralrs"
```

| Key     | Type   | Required | Default | Description                         |
|---------|--------|----------|---------|-------------------------------------|
| `style` | string | yes      | --      | Must be `"mistralrs"` for this row. |

`base-url` and `api-key` are not allowed on `style = "mistralrs"`. The weight-source
fields (`model-id`, `model-path`, `model-file`, `revision`, `context-length`) live
on `[models.<name>]` -- see the
[mistralrs models](#mistralrs-models) subsection.

#### Always parses, even without `--features mistralrs`

outrig **always** recognizes `style = "mistralrs"` for parsing and cross-reference
validation, regardless of whether the binary was built with `--features mistralrs`. The
build-time feature gates only the *use* of the provider: trying to resolve an agent that
points at a `mistralrs` provider on a non-feature build fails at run time, with a message
that names the missing flag.

The reason is portability -- a checked-in `.agents/outrig/config.toml` can declare both
remote and in-process providers, and the same config works for teammates whether or not
they built with the feature on.

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

See [Concepts -> LLM Providers](../concepts/llm-providers.md).

## `[models.<name>]`

A model points at a provider and supplies whatever that provider needs to identify the
weights or wire-format model name. The required fields depend on the provider's `style`.

### openai-style models

```toml
[models.fast]
provider   = "openai"
identifier = "gpt-4o-mini"

[models.smart]
provider   = "openai"
identifier = "gpt-4o"
```

| Key          | Type   | Required | Default | Description                               |
|--------------|--------|----------|---------|-------------------------------------------|
| `provider`   | string | yes      | --      | Name of an entry in `[providers.<name>]`. |
| `identifier` | string | yes      | --      | Model id passed to the provider API.      |

### mistralrs models

For an in-process `style = "mistralrs"` provider, the model row carries the weight
spec. Either `model-id` (HuggingFace auto-download) or `model-path` (local GGUF file)
-- exactly one. `identifier` is **not** allowed on mistralrs models -- the weights
are the model.

```toml
# HuggingFace auto-download:
[models.phi3-fast]
provider   = "local"
model-id   = "microsoft/Phi-3-mini-4k-instruct-gguf"
model-file = "Phi-3-mini-4k-instruct-q4.gguf"
# revision       = "main"   # optional git ref on the HF repo
# context-length = 4096     # optional override

# Local GGUF file:
[models.llama-local]
provider   = "local"
model-path = "/var/cache/outrig/models/llama-3-8b-instruct.q4.gguf"
```

| Key              | Type    | Required | Default  | Description                                  |
|------------------|---------|----------|----------|----------------------------------------------|
| `provider`       | string  | yes      | --       | Name of a `style = "mistralrs"` provider.    |
| `model-id`       | string  | one of\* | --       | HF repo id, e.g. `microsoft/Phi-3-mini-...`. |
| `model-path`     | path    | one of\* | --       | Local path to a GGUF file.                   |
| `model-file`     | string  | no       | --       | Which GGUF in a multi-file repo. With `id`.  |
| `revision`       | string  | no       | `"main"` | HF git ref to pin. With `model-id`.          |
| `context-length` | integer | no       | model    | Override the model's default context window. |

\* Exactly one of `model-id` / `model-path` must be set; setting both, or neither,
is an error.

## `[agents.<name>]`

An agent is the runnable unit: a model plus a system prompt, optionally bound to a container so
`outrig run --agent <name>` knows which sandbox to use.

```toml
[agents.coding]
# model omitted -> falls back to top-level default-model
container = "coding"
preamble  = "You are a careful coding assistant. Repo is at /workspace."
temperature = 0.2
max-tokens  = 4096

[agents.review]
model    = "smart"        # explicit override of default-model
preamble = "You are a meticulous code reviewer..."
```

| Key           | Type    | Required | Default             | Description                            |
|---------------|---------|----------|---------------------|----------------------------------------|
| `model`       | string  | no       | `default-model`     | Name of an entry in `[models.<name>]`. |
| `preamble`    | string  | no       | minimal default     | System prompt for this agent.          |
| `container`   | string  | no       | `default-container` | Default container-config to launch.    |
| `temperature` | float   | no       | provider default    | Sampling temperature.                  |
| `max-tokens`  | integer | no       | provider default    | Output token cap per turn.             |

If `model` is omitted, outrig falls back to the top-level `default-model`; an error if neither is
set. When `outrig run --agent <a>` runs, the chosen container is `--container-config` if given,
otherwise `agents.<a>.container` if set, otherwise `default-container`.

## `[workspace]`

```toml
[workspace]
host-path      = "."
container-path = "/workspace"
```

| Key              | Type | Required | Default        | Description                                         |
|------------------|------|----------|----------------|-----------------------------------------------------|
| `host-path`      | path | no       | `"."`          | Host path to bind-mount, relative to the repo root. |
| `container-path` | path | no       | `"/workspace"` | Where `host-path` is mounted in the container.      |

The bind-mount is read-write and uses `--userns=keep-id` so files written inside the container
appear with your host UID/GID. See [Concepts -> Workspace](../concepts/workspace.md).

## `[containers.<name>]`

You declare one or more container-configs. The selected one becomes the agent's environment.

```toml
[containers.coding]
dockerfile = ".agents/outrig/containers/coding/Dockerfile"
context    = ".agents/outrig/containers/coding"
build-args = { NODE_VERSION = "20" }
```

| Key          | Type           | Required | Default | Description                                           |
|--------------|----------------|----------|---------|-------------------------------------------------------|
| `dockerfile` | path           | yes      | --      | Path to the Dockerfile, relative to the repo root.    |
| `context`    | path           | yes      | --      | Path to the build context, relative to the repo root. |
| `build-args` | table str->str | no       | `{}`    | Extra Dockerfile `ARG`s; keys are ARG names.          |

Notes:

- `outrig container add` writes its output under `.agents/outrig/containers/<name>/`. You can
  put Dockerfiles anywhere you want by editing these paths; the `.agents/outrig/containers/`
  default just keeps outrig-specific files together.
- Inner keys of `build-args` are user-defined Dockerfile `ARG` names; they're left as written
  since they map to env-var-style identifiers.
- outrig does **not** inject UID/GID build-args. Host UID/GID are mapped to the container at
  run time, not baked into the image. See
  [Concepts -> Workspace](../concepts/workspace.md#uidgid-runtime-user-mapping).

### `[containers.<name>.mcp]`

Map of MCP server entries, **keyed on server name**. Each entry is one of two shapes via a
serde-untagged dispatch:

```toml
[containers.coding.mcp]
# Short form -- array of strings, becomes { command = [...] }
shell = ["bash", "-lc", "exec mcp-server-shell"]

# Full form -- table with command + optional env
fs = { command = ["mcp-server-filesystem", "/workspace"] }
build = { command = ["cargo-mcp"], env = { CARGO_HOME = "/workspace/.cargo" } }
```

| Field     | Type             | Required            | Default | Description                                   |
|-----------|------------------|---------------------|---------|-----------------------------------------------|
| `command` | array of strings | yes (or short form) | --      | Argv of the MCP server.                       |
| `env`     | table str->str   | no                  | `{}`    | Env vars set on the `podman exec` invocation. |

Notes:

- The first element of `command` must be on `$PATH` inside the container, or absolute.
- The map key (e.g. `fs`, `shell`, `build`) is the server name. It must match
  `^[a-zA-Z][a-zA-Z0-9_-]*$` and be unique within a container-config.
- The server name appears in `outrig logs <session> <server>` and as the prefix on every tool
  the server advertises (`<server>__<tool>`).

See [Concepts -> MCP Servers](../concepts/mcp-servers.md).

## Resolution: which file wins

Both files are loaded; entries are merged by name. Repo entries win over global entries. A name
defined only once is straightforward; a name defined in both means the repo's definition is
used in full (no per-key merging).

```
~/.outrig/config.toml             .agents/outrig/config.toml         effective
[providers.openai]                                                   global value
[providers.local]                 [providers.local]                  repo overrides
                                  [providers.staging]                repo only
```

`outrig run` walks the chain at startup: agent -> model (explicit or `default-model`) -> provider
-> resolved api-key from env. Anything that fails to resolve is an error printed to stderr
before the REPL starts.

## Full examples

### Global `~/.outrig/config.toml`

```toml
default-model    = "fast"
session-root     = "/var/lib/outrig/sessions"   # optional; default = XDG data dir
model-cache-root = "/var/cache/outrig/models"   # optional; default = XDG cache dir

[providers.openai]
style    = "openai"
base-url = "https://api.openai.com/v1"
api-key  = "${OPENAI_API_KEY}"

[providers.local]
# requires `cargo build --features mistralrs` to actually use, but always parses.
style = "mistralrs"

[models.fast]
provider   = "openai"
identifier = "gpt-4o-mini"

[models.smart]
provider   = "openai"
identifier = "gpt-4o"

[models.phi3-fast]
provider   = "local"
model-id   = "microsoft/Phi-3-mini-4k-instruct-gguf"
model-file = "Phi-3-mini-4k-instruct-q4.gguf"
```

### Repo `.agents/outrig/config.toml`

```toml
default-container = "coding"
default-agent     = "coding"

[workspace]
host-path      = "."
container-path = "/workspace"

[agents.coding]
# model omitted -> uses global default-model = "fast"
container   = "coding"
preamble    = "You are a careful coding assistant. Repo is at /workspace."
temperature = 0.2

[agents.review]
model    = "smart"        # explicit override
preamble = "You are a meticulous code reviewer."

[containers.coding]
dockerfile = ".agents/outrig/containers/coding/Dockerfile"
context    = ".agents/outrig/containers/coding"
build-args = { NODE_VERSION = "20" }

  [containers.coding.mcp]
  fs    = { command = ["mcp-server-filesystem", "/workspace"] }
  shell = ["bash", "-lc", "exec mcp-server-shell"]
  build = { command = ["cargo-mcp"], env = { CARGO_HOME = "/workspace/.cargo" } }
```

## Validation rules

- `default-container` must name an existing `[containers.<name>]` block.
- `default-agent` must name an existing `[agents.<name>]` block.
- Every `agents.<name>.model` (if set) must name an existing `[models.<name>]`. If `model` is
  omitted, `default-model` must be set and must name an existing `[models.<name>]`.
- Every `models.<name>.provider` must name an existing `[providers.<name>]`.
- Every `agents.<name>.container` (if set) must name an existing `[containers.<name>]`.
- Every `providers.<name>.style` must be one of `{"openai", "mistralrs"}`. Other styles are
  reserved for future Rig adapters and listed as TODO in the providers concept page. The
  build-time feature gate (`--features mistralrs`) is **not** checked at validate time --
  see "Always parses, even without `--features mistralrs`" above.
- Every `providers.<name>.api-key` (on `style = "openai"`) must match
  `^\$\{[A-Z_][A-Z0-9_]*\}$`.
- Every `[models.<name>]` whose provider has `style = "openai"` must set
  `identifier` and must not set any of `model-id`, `model-path`, `model-file`,
  `revision`, `context-length`.
- Every `[models.<name>]` whose provider has `style = "mistralrs"` must set
  exactly one of `model-id` / `model-path`. `model-file` and `revision` are only
  meaningful with `model-id`. A `model-path`, if set, must exist on disk relative
  to the repo root (or be absolute). `identifier` is not allowed on mistralrs
  models.
- `model-cache-root`, if set, must be an absolute path; outrig creates it if missing.
- Every server name in `[containers.<name>.mcp]` must match `^[a-zA-Z][a-zA-Z0-9_-]*$` and be
  unique within its container-config.
- Every `command` array must be non-empty.
- `dockerfile` and `context` must exist on disk relative to the repo root.
- `session-root`, if set, must be an absolute path; outrig creates it if missing.
- Unknown keys at any level are rejected.

## See also

- [Concepts](../concepts/README.md) -- narrative explanations of what these keys mean.
- [Reference -> CLI](cli.md) -- flags that override or interact with config.
