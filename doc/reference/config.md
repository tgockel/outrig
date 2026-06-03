# Config Reference

outrig reads two TOML files:

- **Global config** -- user/machine-level. Default location: `~/.outrig/config.toml` (or
  `<XDG_CONFIG_HOME>/outrig/config.toml` if `XDG_CONFIG_HOME` is set). Holds
  `[providers.<name>]` and (typically) `[models.<name>]` since those reference API keys and
  model identifiers that belong to the user, not to any one repo.
- **Repo config** at `.agents/outrig/config.toml` -- repository-level, committed to source
  control. Holds `[workspace]`, `[images.<name>]`, `[agents.<name>]`, and any repo-specific
  providers or models.

Both files use the same schema. Names declared in either are visible everywhere; if a name
appears in both files, the **repo entry wins**. Outrig keys are **kebab-case**; only inner-map
keys whose values map to environment variables (Dockerfile build-args, MCP `env` blocks) keep
their as-written form. Unknown keys are an error -- outrig validates with `deny_unknown_fields`.

## Top level

```toml
# repo config (.agents/outrig/config.toml):
default-image = "coding"
default-agent     = "coding"

# global config (~/.outrig/config.toml):
default-model     = "fast"
session-root      = "/var/lib/outrig/sessions"        # optional; defaults to XDG data dir
model-cache-root  = "/var/cache/outrig/models"        # optional; defaults to XDG cache dir
tool-call-max     = 100                               # optional; defaults to 50
tool-result-max   = 262144                            # optional; defaults to 256 KiB

[network]
mode = "default"                                      # optional: default, audit, or filter
default = "deny"                                      # optional for filter mode
allow = ["github.com:443", "*.npmjs.org"]             # optional; global only
deny  = ["*:22"]                                      # optional; global only
```

| Key                | Type    | Required               | Where  | Description               |
|--------------------|---------|------------------------|--------|---------------------------|
| `default-image`    | string  | for `outrig run`       | repo   | Default `--image`.        |
| `default-agent`    | string  | for `outrig run`       | repo   | Default `--agent`.        |
| `default-model`    | string  | if agent omits `model` | global | Fallback model name.      |
| `session-root`     | path    | no                     | global | Sessions root dir.        |
| `model-cache-root` | path    | no                     | global | GGUF download cache dir.  |
| `tool-call-max`    | integer | no                     | global | Per-turn tool-call max.   |
| `tool-result-max`  | integer | no                     | global | Per-tool-result byte max. |
| `network.mode`     | string  | no                     | either | Network mode.             |
| `network.default`  | string  | no                     | global | Filter fallback action.   |
| `network.allow`    | array   | no                     | global | Filter allow entries.     |
| `network.deny`     | array   | no                     | global | Filter deny entries.      |

`default-image` and `default-agent` belong in the repo config -- image-configs and agents are
project-scoped. `default-model`, `session-root`, `model-cache-root`, and `tool-call-max`
belong in the global config since they're user/machine-level. `tool-result-max` usually belongs
there too, although repo or agent config can tighten it for a noisy project. `[network].mode`
can live in either file; when both set it, the repo value wins for that repo. Network policy
keys (`default`, `allow`, and `deny`) are global-only because they describe the machine's
egress policy, not a project preference. Each may also appear in the other file; repo entries
override global by name.

`session-root` defaults to `<XDG_DATA_HOME>/outrig/sessions/` (typically
`~/.local/share/outrig/sessions/`). The CLI flag `--session-root <path>` overrides both the
config value and the default; `--session-dir <path>` (on `outrig run`/`logs`/`discard`) instead
points at one specific session directory. See [Sessions](../usage/sessions.md).

`model-cache-root` defaults to `<XDG_CACHE_HOME>/outrig/models/` (typically
`~/.cache/outrig/models/`). It only matters for `style = "mistralrs"` models configured
with `model-id` -- that's where the auto-downloaded GGUFs land. See
[Concepts -> In-process LLMs](../concepts/in-process-llm.md).

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
marker that reports the original size and max.

## `[network]`

Network interception is disabled by default:

```toml
[network]
mode = "default"
```

Accepted modes:

- `default`: use Podman's configured default networking, do not install the interceptor, and do
  not write `logs/network.jsonl`.
- `audit`: allow all outbound session-container traffic, but write Zeek `conn.log`-style
  records to `<session_dir>/logs/network.jsonl`.
- `filter`: install the same interceptor as audit mode, write the same audit log, and enforce
  global allow/deny policy before opening upstream TCP connections.

Audit and filter mode require host `nft` and `nsenter` plus permission to enter the rootless podman
container's user/network namespaces. It rewrites the session container's `/etc/resolv.conf` to
send DNS to the per-session in-namespace DNS listener, installs nftables redirection for
outbound TCP and UDP/53, and removes the nftables table during teardown. If either mode is
requested and setup fails, the session fails before MCP servers launch.

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
least one `allow` or `deny` entry, even when `default = "allow"`.

`outrig run --network default|audit|filter` and `outrig mcp --network default|audit|filter`
override this setting for one fresh session. `--network audit` and `--network filter` are
rejected with `outrig mcp --attach` because borrowed containers are not retrofitted with a new
interceptor.

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

`base-url` and `api-key` are not allowed on `style = "mistralrs"`. The model-specific
fields (`model-id`, `model-path`, `model-file`, `revision`, `context-length`, `device`)
live on `[models.<name>]` -- see the
[mistralrs models](#mistralrs-models) subsection.

#### Always parses, even without `--features local-llm`

outrig **always** recognizes `style = "mistralrs"` for parsing and cross-reference
validation, regardless of whether the binary was built with `--features local-llm`. The
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
# device         = "cuda"  # optional; defaults to "cpu"

# Multi-shard quantization (one quant split across files):
[models.llama-70b]
provider   = "local"
model-id   = "MaziyarPanahi/Meta-Llama-3-70B-Instruct-GGUF"
model-file = [
    "Meta-Llama-3-70B-Instruct.Q4_K_M-00001-of-00002.gguf",
    "Meta-Llama-3-70B-Instruct.Q4_K_M-00002-of-00002.gguf",
]

# Local GGUF file:
[models.llama-local]
provider   = "local"
model-path = "/var/cache/outrig/models/llama-3-8b-instruct.q4.gguf"
# device     = "metal" # optional; defaults to "cpu"
```

| Key              | Type    | Required | Default  | Description                                  |
|------------------|---------|----------|----------|----------------------------------------------|
| `provider`       | string  | yes      | --       | Name of a `style = "mistralrs"` provider.    |
| `model-id`       | string  | one of\* | --       | HF repo id, e.g. `microsoft/Phi-3-mini-...`. |
| `model-path`     | path    | one of\* | --       | Local path to a GGUF file.                   |
| `model-file`     | str/arr | with `id`| --       | GGUF filename(s) inside the HF repo.         |
| `revision`       | string  | no       | `"main"` | HF git ref to pin. With `model-id`.          |
| `context-length` | integer | no       | model    | Override the model's default context window. |
| `device`         | string  | no       | `"cpu"`  | One of `cpu`, `cuda`, `cuda:N`, `metal`.     |

\* Exactly one of `model-id` / `model-path` must be set; setting both, or neither,
is an error.

`device = "cuda"` and `device = "cuda:N"` require a binary built with
`--features "local-llm cuda"`; `device = "metal"` requires
`--features "local-llm metal"`. The feature check happens when an agent resolves the
model. Enabling `cuda` or `metal` without `local-llm` emits a build warning and has no
effect. outrig does not fall back to CPU if the requested backend is unavailable. With
CUDA, `cuda:N` selects the base device for mistralrs's automatic mapper; it is not an
exclusive single-device sharding directive. `outrig run --device <device>` overrides this
field for one run without editing config.

Metal is only usable on macOS targets. Non-macOS builds can compile with the `metal`
feature for feature-matrix coverage, but trying to instantiate a Metal device fails with
a platform error.

## `[agents.<name>]`

An agent is the runnable unit: a model plus a system prompt, optionally bound to an image-config
so `outrig run --agent <name>` knows which sandbox to use.

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
- `preamble` (string, optional, default: minimal default): system prompt for this agent.
- `image` (string, optional, default: `default-image`): default image-config
  to launch.
- `temperature` (float, optional, default: provider default): sampling temperature.
- `max-tokens` (integer, optional, default: provider default): output token max per turn.
- `tool-call-max` (integer, optional, default: top-level value or `50`): tool calls per turn.
- `tool-result-max` (integer, optional, default: top-level value or `262144`): bytes per result.

If `model` is omitted, outrig falls back to the top-level `default-model`; an error if neither is
set, except `outrig run --model <name>` may supply the selected agent's model for that run. When
`outrig run --agent <a>` runs, the chosen image-config is `--image` if given, otherwise
`agents.<a>.image` if set, otherwise `default-image`.
`tool-call-max` is per turn, not per session; follow-up prompts start a fresh count.
`tool-result-max` is per result and applies equally to successful MCP results and MCP error
messages.

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

- `host-path` (path, optional, default: `"."`): primary workspace host path,
  relative to the repo root.
- `container-path` (path, optional, default: `"/workspace"`): where the primary
  workspace is mounted in the container.
- `workspace.mounts` (array, optional, default: `[]`): extra directory bind-mounts.
- `mounts[*].host-path` (path, required): host directory to mount. Relative paths
  resolve against the repo root.
- `mounts[*].container-path` (path, required): absolute in-container mount point.
- `mounts[*].access` (string, optional, default: `"read-only"`): either
  `"read-only"` or `"read-write"`.

The primary workspace bind-mount is always read-write and uses `--userns=keep-id` so files
written inside the container appear with your host UID/GID. Extra mounts default to read-only;
set `access = "read-write"` only for directories the agent should be able to modify. See
[Concepts -> Workspace](../concepts/workspace.md).

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

- `dockerfile` (path, required\*): path to the Dockerfile, relative to the repo root.
- `context` (path, required\*): path to the build context, relative to the repo root.
- `build-args` (table str->str, optional, default: `{}`): extra Dockerfile `ARG`s.
  Keys are ARG names. Values are either literal strings or `${VAR}` references resolved
  from the host environment at `outrig build` time; see the MCP `env` value syntax
  below.

### Use-existing-image (new form)

```toml
[images.scratch]
image-name = "docker.io/library/ubuntu:24.04"
```

- `image-name` (string, required\*): image reference passed to `podman pull` / `podman run`.
  Accepts any ref form podman supports: `name:tag`, `registry/name:tag`, `name@sha256:...`.

\* Exactly one of the two shapes must be set. Setting `image-name` alongside `dockerfile`,
`context`, or `build-args` is an error. Setting neither is also an error.

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

Capability names may be written as `NET_RAW` or `CAP_NET_RAW`; outrig normalizes to the
podman form without the `CAP_` prefix. The existing `--security-opt=no-new-privileges`
setting is always applied. This section does not configure seccomp, AppArmor, SELinux,
read-only roots, mount policy, or network egress filtering.

### `[images.<name>.mcp]`

Map of MCP server entries, **keyed on server name**. Each entry is one of two shapes via a
serde-untagged dispatch:

```toml
[images.coding.mcp]
# Short form -- array of strings, becomes { command = [...] }
shell = ["bash", "-lc", "exec shell-mcp-command"]

# Full form -- table with command + optional env
fs = { command = ["mcp-server-filesystem", "/workspace"] }
build = { command = ["cargo-mcp"], env = { CARGO_HOME = "/workspace/.cargo" } }
```

`shell-mcp-command` is a placeholder. Replace it with the shell MCP server you install in the
image, or declare any other MCP command that should run inside the container.

- `command` (array of strings, required unless using short form): argv of the MCP server.
- `env` (table str->str, optional, default: `{}`): env vars set on the `podman exec`
  invocation.

Notes:

- The first element of `command` must be on `$PATH` inside the container, or absolute.
- The map key (e.g. `fs`, `shell`, `build`) is the server name. It must match
  `^[a-zA-Z][a-zA-Z0-9_-]*$` and be unique within an image-config.
- The server name appears in `outrig logs <session> <server>` and as the prefix on every tool
  the server advertises (`<server>__<tool>`).
- Each `env` value is either a literal string forwarded verbatim or a `${VAR}` reference
  resolved from the host environment at MCP startup -- see the subsection below.
- Images can provide the same table via their `org.outrig.mcp` OCI label. Repo config entries
  override image entries by server name; see
  [Concepts -> MCP Servers](../concepts/mcp-servers.md#embedding-mcp-config-in-the-image).

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

`[workspace]` primary fields are repo-owned: the repo config's `host-path` and
`container-path` win as a block. Extra `workspace.mounts` are combined instead of replaced:
global mounts are kept first, followed by repo mounts. Duplicate final `container-path` values
are rejected during validation.

`[network].mode` follows repo precedence when the repo config declares the table. If the repo
omits `[network]`, the global mode remains in effect. This matters when global config enables
audit or filter mode and a repo explicitly sets `mode = "default"`. Network policy keys are
global-only; repo config cannot set `network.default`, `network.allow`, or `network.deny`.

## Full examples

### Global `~/.outrig/config.toml`

```toml
default-model    = "fast"
session-root     = "/var/lib/outrig/sessions"   # optional; default = XDG data dir
model-cache-root = "/var/cache/outrig/models"   # optional; default = XDG cache dir
tool-call-max    = 100                           # optional; default = 50
tool-result-max  = 262144                        # optional; default = 256 KiB

[network]
mode = "default"                                 # optional; default, audit, or filter
default = "deny"                                 # optional for filter mode
allow = ["github.com:443", "*.npmjs.org"]        # optional; global only
deny  = ["*:22"]                                 # optional; global only

[providers.openai]
style    = "openai"
base-url = "https://api.openai.com/v1"
api-key  = "${OPENAI_API_KEY}"

[providers.local]
# requires `cargo build --features local-llm` to actually use, but always parses.
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
device     = "cpu"
```

### Repo `.agents/outrig/config.toml`

```toml
default-image = "coding"
default-agent     = "coding"

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
```

## Validation rules

- `default-image` must name an existing `[images.<name>]` block.
- `default-agent` must name an existing `[agents.<name>]` block.
- Every `agents.<name>.model` (if set) must name an existing `[models.<name>]`. If `model` is
  omitted, `default-model` must be set and must name an existing `[models.<name>]`.
- Every `models.<name>.provider` must name an existing `[providers.<name>]`.
- Every `agents.<name>.image` (if set) must name an existing `[images.<name>]`.
- Every `providers.<name>.style` must be one of `{"openai", "mistralrs"}`. Other styles are
  reserved for future Rig adapters and listed as TODO in the providers concept page. The
  build-time feature gate (`--features local-llm`) is **not** checked at validate time --
  see "Always parses, even without `--features local-llm`" above.
- Every `providers.<name>.api-key` (on `style = "openai"`) must match
  `^\$\{[A-Z_][A-Z0-9_]*\}$`.
- Every `[models.<name>]` whose provider has `style = "openai"` must set
  `identifier` and must not set any of `model-id`, `model-path`, `model-file`,
  `revision`, `context-length`, `device`.
- Every `[models.<name>]` whose provider has `style = "mistralrs"` must set
  exactly one of `model-id` / `model-path`. When `model-id` is set, `model-file`
  is **required** -- mistralrs's GGUF loader needs a specific filename and HF
  repos typically hold many quantizations. `model-file` accepts either a
  single string (one GGUF file) or an array of strings (a multi-shard
  quantization, e.g. `*-00001-of-00003.gguf`). `revision` is optional and
  only meaningful with `model-id`. A `model-path`, if set, must exist on
  disk relative to the repo root (or be absolute). `identifier` is not
  allowed on mistralrs models. `device`, if set, must be one of `cpu`, `cuda`,
  `cuda:N`, or `metal`.
- `model-cache-root`, if set, must be an absolute path; outrig creates it if missing.
- `tool-call-max`, if set at the top level or on an agent, must be between `1` and `2000`.
- `tool-result-max`, if set at the top level or on an agent, must be between `1024` and
  `16777216` bytes.
- `[network].mode`, if set, must be `default` or `audit`.
- Every server name in `[images.<name>.mcp]` must match `^[a-zA-Z][a-zA-Z0-9_-]*$` and be
  unique within its image-config.
- Every `command` array must be non-empty.
- `dockerfile` and `context` must exist on disk relative to the repo root (build path only).
- Each `[images.<name>]` must set exactly one of: `image-name`, or `dockerfile` + `context`.
  Setting both shapes, neither, `image-name` with `build-args`, or only one of
  `dockerfile`/`context` without the other is an error.
- `image-name` must not be empty.
- Every `[images.<name>.security].capability-profile`, if set, must be one of
  `default`, `no-net-raw`, or `drop-all`.
- Every capability name in `cap-drop` or `cap-add` must be non-empty and match
  `^[A-Z0-9_]+$` after optional `CAP_` stripping.
- Capability names must not be duplicated within `cap-drop` or within `cap-add`, after
  optional `CAP_` stripping.
- The same normalized capability name must not appear in both `cap-drop` and `cap-add`.
- `session-root`, if set, must be an absolute path; outrig creates it if missing.
- Every `workspace.mounts[*].host-path`, if validated with a repo root, must exist and be a
  directory. Relative host paths resolve against the repo root.
- Every `workspace.mounts[*].container-path` must be absolute and must not be `/`.
- Extra workspace mount `container-path` values must be unique, including no collision with the
  primary workspace `container-path`.
- Every `workspace.mounts[*].access`, if set, must be either `read-only` or `read-write`.
- Unknown keys at any level are rejected.

## See also

- [Concepts](../concepts/README.md) -- narrative explanations of what these keys mean.
- [Reference -> CLI](cli.md) -- flags that override or interact with config.
