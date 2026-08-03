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
default-agent = "coding"

# global config (~/.outrig/config.toml):
default-model      = "fast"
session-root       = "/var/lib/outrig/sessions"       # optional; defaults to XDG data dir
model-cache-root   = "/var/cache/outrig/models"       # optional; defaults to XDG cache dir
tool-call-max      = 100                              # optional; defaults to 50
tool-result-max    = 262144                           # optional; defaults to 256 KiB
subagent-depth-max = 3                                # optional; defaults to 3
subagent-width-max = 8                                # optional; defaults to 8
retry-budget-secs  = 600                              # optional; defaults to 600

[network]
mode = "default"                                      # optional: default, audit, or filter
default = "deny"                                      # optional for filter mode
allow = ["github.com:443", "*.npmjs.org"]             # optional; global only
deny  = ["*:22"]                                      # optional; global only
```

| Key                  | Type    | Required               | Where  | Description               |
|----------------------|---------|------------------------|--------|---------------------------|
| `default-image`      | string  | no                     | repo   | Default `--image`.        |
| `default-agent`      | string  | no                     | repo   | Default `--agent`.        |
| `default-model`      | string  | if agent omits `model` | global | Fallback model name.      |
| `session-root`       | path    | no                     | global | Sessions root dir.        |
| `model-cache-root`   | path    | no                     | global | GGUF download cache dir.  |
| `tool-call-max`      | integer | no                     | global | Per-turn tool-call max.   |
| `tool-result-max`    | integer | no                     | global | Per-tool-result byte max. |
| `subagent-depth-max` | integer | no                     | global | Max subagent nesting.     |
| `subagent-width-max` | integer | no                     | global | Max live subagents/agent. |
| `retry-budget-secs`  | integer | no                     | global | LLM retry budget (secs).  |
| `network.mode`       | string  | no                     | either | Network mode.             |
| `network.default`    | string  | no                     | global | Filter fallback action.   |
| `network.allow`      | array   | no                     | global | Filter allow entries.     |
| `network.deny`       | array   | no                     | global | Filter deny entries.      |

`default-agent` is optional. With neither `--agent` nor `default-agent`, `outrig run` starts
with no agent: no preamble is sent, every knob comes from the top level, and the image cascade
is `--image`, then `default-image`, then outrig's
[built-in default](#the-built-in-default-image-config). A model must still resolve from
`--model` or `default-model` -- that is the one thing an agentless session cannot do without.

`default-image` and `default-agent` belong in the repo config -- image-configs and agents are
project-scoped. `default-model`, `session-root`, `model-cache-root`, `tool-call-max`, and the
subagent limits belong in the global config since they're user/machine-level. `tool-result-max`
usually belongs there too, although repo or agent config can tighten it for a noisy project.
`retry-budget-secs` is a default for every remote provider; a `[providers.<name>]` row that
sets its own overrides it, which is usually the better place since rate limits are a property
of the endpoint.
`[network].mode` can live in either file; when both set it, the repo value wins for that repo.
Network policy keys (`default`, `allow`, and `deny`) are global-only because they describe the
machine's egress policy, not a project preference. Each may also appear in the other file; repo
entries override global by name.

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
entries with the same name override globals, replacing the whole entry rather than merging
field by field. The accepted `style` values are `"openai"`, `"anthropic"`, and
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
| `request-timeout-secs` | integer      | no       | `600`   | HTTP timeout for LLM calls.      |
| `retry-budget-secs`    | integer      | no       | `600`   | Transient-retry budget, seconds. |

`request-timeout-secs` bounds each individual attempt, and defaults high enough not to cut
off long reasoning completions. `retry-budget-secs` bounds *all* the attempts together: an
LLM request that fails transiently is retried until it succeeds or the budget runs out, then
ends the turn without ending the session. Which failures count as transient, how the wait is
chosen, and how `Retry-After` is honored are described in
[Concepts -> LLM providers](../concepts/llm-providers.md#transient-failures); this page is
the reference for the keys themselves.

Two things about the budget that belong here, because they are about the keys:

- It is **wall clock from the first attempt**, including time each attempt spends in flight,
  not only time spent sleeping. That matters when it is set near `request-timeout-secs`: one
  attempt that runs the timeout out spends the whole budget and buys no retries. It does not
  matter for a rate limit, which comes back in milliseconds.
- `0` disables retries -- the first failure is final. Useful for scripted runs that would
  rather fail fast than wait.

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

`base-url` is the API root, without the `/v1/messages` path -- outrig appends that. A
trailing `/v1`, `/messages`, or `/v1/messages` is trimmed if you write one anyway, so
`https://api.anthropic.com` and `https://api.anthropic.com/v1` behave identically. Timeout
and transient-retry behavior match `style = "openai"` exactly.

Anthropic requires an output-token ceiling on every request. outrig knows one for the model
identifiers it recognizes; any other identifier needs `max-tokens` on the model or the
agent, or the first turn fails saying so. See
[anthropic models](#anthropic-models).

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

### Remote-provider models

Models on a remote provider -- `style = "openai"` or `style = "anthropic"` -- name their
model with an `identifier`. None of the mistralrs weight fields are allowed.

```toml
[models.fast]
provider   = "openai"
identifier = "gpt-4o-mini"

[models.smart]
provider   = "openai"
identifier = "gpt-4o"
```

| Key          | Type    | Required | Default | Description                                |
|--------------|---------|----------|---------|--------------------------------------------|
| `provider`   | string  | yes      | --      | Name of an entry in `[providers.<name>]`.  |
| `identifier` | string  | yes      | --      | Model id passed to the provider API.       |
| `max-tokens` | integer | no       | --      | Output-token ceiling per turn, see below.  |

`max-tokens` on a model is the fallback for every agent that uses it;
`[agents.<name>].max-tokens` wins where it is set. Leaving both unset lets the provider
decide -- which is fine for OpenAI-compatible endpoints, and is what the next section is
about for Anthropic.

Note the spelling. In config it is `max-tokens`, like every other key; `max_tokens` is
rejected as an unknown field. The underscored form is what the provider API calls it, so it
is what appears in a raw error coming back from one.

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

Tier 3 is a floor under the failure, not a recommendation -- prefer setting the ceiling
yourself, as `[models.older]` does above. The recognized set in tier 2 is whatever the
pinned rig release knows, so an identifier newer than that release lands in tier 3 even
though it is current.

32768 is chosen to fail in the direction you can see. A model whose real limit is *lower*
-- the 3.x families cap at 8192 or 4096 -- rejects the request outright, naming its own
limit, and one config line fixes it. A ceiling set too low instead cuts replies off
mid-sentence with nothing logged, which is much harder to recognize as a config problem.
`outrig config init` prompts for `max-tokens` when it writes an Anthropic model, so a
generated config carries an explicit one either way.

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
- `image` (string, optional, default: `default-image`): default image-config
  to launch.
- `temperature` (float, optional, default: provider default): sampling temperature.
- `max-tokens` (integer, optional, default: the model's `max-tokens`, else the provider
  default): output token max per turn. Worth setting in one place or the other for an
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

- `host-path` (path, optional, default: `"."`): primary workspace host path,
  relative to the repo root. Always the repo root -- the primary `[workspace]`
  fields are repo-owned as a block.
- `container-path` (path, optional, default: `"/workspace"`): where the primary
  workspace is mounted in the container.
- `workspace.mounts` (array, optional, default: `[]`): extra directory bind-mounts.
- `mounts[*].host-path` (path, required): host directory to mount. Relative paths
  resolve against the directory of the file that declared the entry -- see
  [path resolution](#path-resolution). Global and repo mounts are concatenated,
  so one list can hold entries with different base directories.
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
needs, since it appends `sleep infinity` itself and writes the runtime user's `/etc/passwd`
entry at start. Because both servers run with `view = "primary"`, the commands they spawn
resolve in *this* container's filesystem, which is why the primary is the one that needs `git`.

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
  the container, which is what a nested rootless container runtime needs and which also lets
  any setuid-root binary in the image be used; see
  [Containers](../concepts/containers.md#devices-and-privilege-escalation) for the tradeoff.
- `devices` (array, optional, default: `[]`): host device nodes to pass through, emitted as
  one `--device=<path>` per entry in declaration order. Entries are plain absolute paths;
  podman's `<src>:<dst>:<perms>` form is not accepted.

Capability names may be written as `NET_RAW` or `CAP_NET_RAW`; outrig normalizes to the
podman form without the `CAP_` prefix. This section does not configure seccomp, AppArmor,
SELinux, read-only roots, mount policy, or network egress filtering.

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

# Full form with a placement key -- runs in a sidecar container
lint = { command = ["mcp-lint", "--stdio"], sidecar = "tools" }
grep = { command = ["mcp-grep"], image = "ghcr.io/example/mcp-grep:1" }

# entrypoint-stdio -- no command; the container's ENTRYPOINT is the server
fetch = { image = "ghcr.io/example/mcp-fetch:2", env = { TOKEN = "${FETCH_TOKEN}" } }
serve = { image = "docker.io/mcp/filesystem:latest", args = ["/workspace"] }
```

`shell-mcp-command` is a placeholder. Replace it with the shell MCP server you install in the
image, or declare any other MCP command that should run inside the container.

- `command` (array of strings, required unless using the short form or the entrypoint-stdio
  form): argv of the MCP server.
- `env` (table str->str, optional, default: `{}`): env vars set on the `podman exec`
  invocation -- or, for entrypoint-stdio servers, baked in via `podman create --env` (visible
  to `podman inspect` on the host, like exec argv).
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

Notes:

- The first element of `command` must be on `$PATH` inside the container, or absolute.
- The map key (e.g. `fs`, `shell`, `build`) is the server name. It must match
  `^[a-zA-Z][a-zA-Z0-9_-]*$` and be unique within an image-config.
- The server name appears in `outrig logs <session> <server>` and as the prefix on every tool
  the server advertises (`<server>__<tool>`).
- Each `env` value is either a literal string forwarded verbatim or a `${VAR}` reference
  resolved from the host environment at MCP startup -- see the subsection below.
- Images can provide the same table via their `org.outrig.mcp` OCI label (placement keys are
  repo-config-only and rejected in labels). Repo config entries override image entries by
  server name; see
  [Concepts -> MCP Servers](../concepts/mcp-servers.md#embedding-mcp-config-in-the-image).
- Build-from-Dockerfile repo images are stamped with the merged `org.outrig.mcp` label on cache
  misses, so `outrig image inspect <name>:<content-hash>` can show their declared repo-local MCP
  entries without starting a container.

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

`[sidecars.<sc>]` merges by name like the other top-level maps, so a repo image-config can
reference a sidecar the user declared globally.

`[network].mode` follows repo precedence when the repo config declares the table. If the repo
omits `[network]`, the global mode remains in effect. This matters when global config enables
audit or filter mode and a repo explicitly sets `mode = "default"`. Network policy keys are
global-only; repo config cannot set `network.default`, `network.allow`, or `network.deny`.

### Path resolution

A relative path is resolved against the directory of the config file that declared it, not
against whichever repo is current. Provenance is recorded per entry before the merge, so a
concatenated `workspace.mounts` list can hold entries with different base directories.

| Declared in                  | Relative paths resolve against         |
|------------------------------|----------------------------------------|
| `.agents/outrig/config.toml` | the repo root                          |
| `~/.outrig/config.toml`      | that file's directory (`~/.outrig/`)   |
| `--global-config <path>`     | `<path>`'s parent directory            |

Absolute paths are used as-is and ignore the rule entirely. This applies to
`[images.<name>].dockerfile` and `.context`, and to `host-path` in both `[[workspace.mounts]]`
and `[sidecars.<sc>.mounts]`. `[workspace].host-path` is always repo-relative, because the
primary `[workspace]` fields are repo-owned as a block.

The practical effect is that a global `[images.<name>]` can use the build shape: its Dockerfile
and context live beside `~/.outrig/config.toml` and are found from any repo on the machine. One
exception remains repo-relative: `[models.<name>].model-path`, which is documented under
[Validation rules](#validation-rules).

## Full examples

### Global `~/.outrig/config.toml`

```toml
default-model      = "fast"
session-root       = "/var/lib/outrig/sessions" # optional; default = XDG data dir
model-cache-root   = "/var/cache/outrig/models" # optional; default = XDG cache dir
tool-call-max      = 100                         # optional; default = 50
tool-result-max    = 262144                      # optional; default = 256 KiB
subagent-depth-max = 3                           # optional; default = 3
subagent-width-max = 8                           # optional; default = 8
retry-budget-secs  = 600                         # optional; default = 600

[network]
mode = "default"                                 # optional; default, audit, or filter
default = "deny"                                 # optional for filter mode
allow = ["github.com:443", "*.npmjs.org"]        # optional; global only
deny  = ["*:22"]                                 # optional; global only

[providers.openai]
style    = "openai"
base-url = "https://api.openai.com/v1"
api-key  = "${OPENAI_API_KEY}"

[providers.anthropic]
style    = "anthropic"
base-url = "https://api.anthropic.com"
api-key  = "${ANTHROPIC_API_KEY}"

[providers.local]
# requires `cargo build --features local-llm` to actually use, but always parses.
style = "mistralrs"

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

[models.phi3-fast]
provider   = "local"
model-id   = "microsoft/Phi-3-mini-4k-instruct-gguf"
model-file = "Phi-3-mini-4k-instruct-q4.gguf"
device     = "cpu"
```

### Repo `.agents/outrig/config.toml`

```toml
default-image = "coding"
default-agent = "coding"

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

`outrig run` and `outrig mcp` use the full validation path. `outrig build` validates every
image-config in the merged config but does not require agent/model/provider wiring to resolve.

- `default-image` must name an existing `[images.<name>]` block.
- `default-agent` must name an existing `[agents.<name>]` block. Absence is fine; a name that
  matches nothing is not.
- Every `agents.<name>.model` (if set) must name an existing `[models.<name>]`. If `model` is
  omitted, `default-model` must be set and must name an existing `[models.<name>]`.
- Every `models.<name>.provider` must name an existing `[providers.<name>]`.
- Every `agents.<name>.image` (if set) must name an existing `[images.<name>]`.
- Every `providers.<name>.style` must be one of `{"openai", "anthropic", "mistralrs"}`. Other
  styles are reserved for future Rig adapters and listed as TODO in the providers concept
  page. The build-time feature gate (`--features local-llm`) is **not** checked at validate
  time -- see "Always parses, even without `--features local-llm`" above.
- Every `providers.<name>.api-key` (on a remote style) must match
  `^\$\{[A-Z_][A-Z0-9_]*\}$`.
- Every `[models.<name>]` whose provider has a remote style must set
  `identifier` and must not set any of `model-id`, `model-path`, `model-file`,
  `revision`, `context-length`, `device`. The error names the style of the
  provider the model points at.
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
- `subagent-depth-max`, if set at the top level or on an agent, must be between `1` and `16`
  (`1` disables subagents).
- `subagent-width-max`, if set at the top level or on an agent, must be between `1` and `16`.
- `retry-budget-secs`, if set at the top level or on a remote provider, must be at most `3600`
  seconds. `0` is legal and disables retries.
- `[network].mode`, if set, must be `default` or `audit`.
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
- `dockerfile` and `context` must exist on disk, resolved against the declaring file's directory
  (build path only). The error names both the path as written and the file that declared it.
- Each `[images.<name>]` must set exactly one of: `image-name`, or `dockerfile` + `context`.
  Setting both shapes, neither, `image-name` with `build-args`, or only one of
  `dockerfile`/`context` without the other is an error.
- `image-name` must not be empty.
- A build-from-Dockerfile `[images.<name>]` block key must be a valid container image
  repository component -- lowercase alphanumeric separated by `.`, `_`, or `-`
  (`^[a-z0-9]+([._-]+[a-z0-9]+)*$`) -- because it becomes the built image's repository.
  Image-name configs are exempt: their block key is just a label.
- Every `[images.<name>.security].capability-profile`, if set, must be one of
  `default`, `no-net-raw`, or `drop-all`.
- Every capability name in `cap-drop` or `cap-add` must be non-empty and match
  `^[A-Z0-9_]+$` after optional `CAP_` stripping.
- Capability names must not be duplicated within `cap-drop` or within `cap-add`, after
  optional `CAP_` stripping.
- The same normalized capability name must not appear in both `cap-drop` and `cap-add`.
- Every `[images.<name>.security].devices` entry must be non-empty and an absolute path.
  Whether the node exists is not checked -- validation may run on a machine that is not the
  launch host, so podman reports a missing node at launch instead.
- Device paths must not be duplicated within one `devices` list.
- `session-root`, if set, must be an absolute path; outrig creates it if missing.
- Every `workspace.mounts[*].host-path`, if validated with a repo root, must exist and be a
  directory. Relative host paths resolve against the declaring file's directory.
- Every `workspace.mounts[*].container-path` must be absolute and must not be `/`.
- Extra workspace mount `container-path` values must be unique, including no collision with the
  primary workspace `container-path`.
- Every `workspace.mounts[*].access`, if set, must be either `read-only` or `read-write`.
- Unknown keys at any level are rejected.

## See also

- [Concepts](../concepts/README.md) -- narrative explanations of what these keys mean.
- [Reference -> CLI](cli.md) -- flags that override or interact with config.
