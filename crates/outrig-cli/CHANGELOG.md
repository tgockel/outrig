# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- **The `outrig run` banner names the endpoint.** A `[outrig] base-url:` row follows the
  `agent:`/`model:` line for an `openai` or `anthropic` provider, so where the session's key goes
  is on screen before the first turn; the provider *style* on the line above reads the same for
  any two endpoints speaking one wire format. Credentials embedded in the URL
  (`https://user:secret@proxy.example`) print as `***@`. An in-process `mistralrs` model prints
  no such row.
- **The `outrig run` prompt is a line editor.** On an interactive terminal it has readline-style
  editing -- `Ctrl-A`/`Ctrl-E`, `Ctrl-W`, `Ctrl-K`, word motion -- and `Up`/`Down` recall of the
  prompts typed earlier in the session, with `Ctrl-R` to search them. Recall is in memory for the
  length of the session: **outrig writes no history file**, so nothing typed at the prompt is
  persisted. `/reset` still clears conversation history and does not touch prompt recall.

  The prompt and the echo of what is typed now go to the controlling terminal rather than to
  stderr. That is what keeps `outrig run > out.txt` capturing only the model's replies, and it
  additionally means `outrig run 2> err.txt` no longer swallows the prompt.

  Two behavior changes to know about. A multi-line paste now arrives as **one** prompt with the
  line breaks intact, where it previously became one turn per line. And `Ctrl-C` at the prompt
  discards the typed line rather than raising a signal; a second `Ctrl-C` with nothing entered in
  between still exits, and `Ctrl-C` during a turn is unchanged. `Ctrl-C` during a slash command
  still cannot interrupt it (#345).

  Scripted use is unchanged: piped stdin, a redirected file, a terminal on stdin other than the
  one you are typing at, and `TERM` set to `dumb`, `cons25`, or `emacs` all keep the previous
  line-at-a-time reader. `RUST_LOG=debug` does not record what you type: the editor's own debug
  output stays at `warn` unless the filter names `rustyline`. See
  [Line editing and history](../../doc/usage/run.md#line-editing-and-history).
- **`call-timeout-secs` and `mcp-call-timeout-secs` set how long a tool call may run.** A
  `tools/call` that outlives its deadline is cancelled at the server and comes back to the model
  as a timeout error; the session goes on. The deadline is the server's own `call-timeout-secs`
  (on the table form of an `[images.<name>.mcp]` entry, or in an image's `org.outrig.mcp`
  label), else the top-level `mcp-call-timeout-secs` (either config file; repo wins), else 600
  seconds. Both accept 1 to 3600. Raise it on a server whose tools run builds or test suites. A
  primary-hosted entry's key is part of the image's stamped label, so changing it rebuilds a
  repo-built image, as changing its `env` does. See
  [What if a server hangs?](../../doc/concepts/mcp-servers.md#what-if-a-server-hangs) (#338).

### Changed

- **A cache miss during `outrig run` and `outrig mcp` startup shows the build as it happens.**
  `[outrig] ensuring image <tag>` used to be followed by silence for as long as `buildah build`
  took, which on a first run looked like a hang. The build's output (or the `podman pull` for an
  `image-name` config) now streams to stderr as `[buildah]` / `[podman]` lines between
  `ensuring image` and `image ready`, without `--verbose`; `-v` still adds the command lines and
  `container.log`, and each line appears once. Sidecar images report `ensuring sidecar image
  <name>` / `sidecar image ready: <tag> (..)` and are ensured one at a time so two builds never
  interleave, while their label reads stay concurrent. stdout is untouched, so `outrig mcp`'s
  JSON-RPC channel is unaffected, and a failed build no longer repeats its output inside the
  error. (#478)

### Fixed

- **A name a container sends in more than one piece is still read.** Under `--network audit` or
  `filter`, the interceptor read a connection's opening bytes once, so a TLS `ClientHello` or
  HTTP request head that arrived in pieces lost its name: a `deny` entry only that name matched
  let the connection through, and its `network.jsonl` record showed `service` `"-"` and no host
  -- or, for a `Host:` line cut short, just the part of the name that had arrived. The
  interceptor now reads until the record or the head is complete, for at most 750 ms after its
  first bytes, before it decides or forwards anything. A client whose first bytes begin a request
  head it never finishes, such as an inline `GET key` to Redis, now waits up to those 750 ms.
  Fixed in `outrig` (#353).

- **A misspelled key in an `[images.<name>.mcp]` table entry is an error.** Every other table
  rejected unknown keys, but this one dropped them: `enviroment = { ... }` started the server
  with no environment, and a misspelled `command` beside `sidecar` or `image` quietly ran the
  container's ENTRYPOINT as the server instead. Loading now fails at the misspelled key and
  lists the keys an entry accepts. The same holds for an image's `org.outrig.mcp` label and a
  standalone image's `image.toml`, and `get_config_schema` now says so. Fixed in `outrig`
  (#340).

- **An MCP server whose `tools/list` pages never end fails startup at once.** A server that kept
  handing back a `nextCursor` -- the one it was sent, a cycle of them, or a fresh one forever --
  held `outrig run`, `outrig mcp`, and `outrig image build`'s live test for the full 120-second
  `tools/list` bound while its listing grew. A cursor an earlier page already sent, or a listing
  still paging after 1,000 pages, now fails startup straight away with the server's stderr in
  the error. Fixed in `outrig` (#339).

- **With the host's resolver down, a container's lookups fail instead of hanging.** Under
  `--network audit` or `filter`, a lookup no host resolver answered got no reply at all, so every
  name a tool looked up waited out its resolver library's own timeouts and retries before failing.
  It is now answered SERVFAIL, at once when the resolver refuses (systemd-resolved stopped, say),
  and `outrig` warns about the first one in each container. A host `/etc/resolv.conf` listing an
  IPv6 nameserver first on a kernel booted with `ipv6.disable=1` failed every lookup; that
  resolver is now passed over. Fixed in `outrig` (#354).

- **An MCP server that never answers no longer holds a session forever.** A server that took
  stdin and never spoke MCP -- `fs = ["cat"]` was enough -- kept `outrig run`, `outrig mcp`, and
  `outrig image build`'s live test at `MCP <name>: initializing` until interrupted, and a tool
  that blocked held its turn the same way. `initialize` and `tools/list` now fail after 120
  seconds each, with the server's exit status and stderr in the error, and a tool call after its
  deadline (above). A call Ctrl-C abandons mid-turn, and a call an `outrig mcp` client cancels,
  are now cancelled at the backing server with `notifications/cancelled` instead of left
  running; the server may still finish the work, since cancellation is advisory (#338).

- **The session root is the global config's, from every directory.** `outrig ls`, `logs`, `discard`,
  and `clean` read `session-root` from the repo config found from the working directory, unvalidated
  and as written. A relative value named a different root from each directory, `~` was a directory
  literally named `~`, and a value `outrig run` refused as not absolute was obeyed. `run` and `mcp`
  let a repo value win as well, so a committed config chose where this machine writes records, where
  `clean` and `discard` delete, and which containers `clean` takes for strays. `session-root` and
  `model-cache-root` are now global-only: a repo config that sets either is rejected at load with an
  error naming the key. In the global config a relative value resolves against that file's
  directory, and a leading `~` is your home directory. The session commands read only the global
  config, so they answer the same from any directory, no longer fail beneath another user's
  `.agents/outrig/config.toml`, and ignore `--config`. To migrate, move the key to the global
  config; records under a root a repo chose stay reachable with `--session-root`. `model-cache-root`
  is deprecated along with `style = "mistralrs"`, and a repo config that sets it fails to load too
  (#336).

- **A repo config may not declare a provider that carries an `api-key`.** `outrig run`,
  `outrig mcp`, and `outrig build` refuse a `.agents/outrig/config.toml`, or a `--config` file,
  that declares an `openai` or `anthropic` provider, naming the provider and the variable. A repo
  entry used to replace the global provider of the same name wholesale, so a cloned repo -- or
  one whose config the agent edited during the previous session -- could send the operator's key
  to a `base-url` of its choosing, and the banner showed nothing different. The repo phase of
  `outrig init` and the `outrig image add` bootstrap no longer offer to define a provider: a
  model there names one from the global config, and an unknown name is asked again. Move any
  remote provider a repo config declares to `~/.outrig/config.toml` or a `--global-config` file;
  the repo's models keep naming it. Fixed in `outrig`; see that crate's changelog.
- **Ctrl-C during a turn no longer takes the session's tools with it.** The terminal sends
  `SIGINT` to its whole foreground process group, and every `podman exec` transport to an MCP
  server was in outrig's group. The podman client exited on it, so after `[outrig] interrupted`
  every tool call failed with `Transport closed` until the session ended. These transports now
  run in a process group of their own, so the next prompt can call tools again, as
  [Interrupting and exiting](../../doc/usage/run.md#interrupting-and-exiting) describes. The same
  applies to the `podman start --attach` clients of entrypoint-stdio sidecars and to the
  `podman events` watcher. A Ctrl-C used to end that watcher, which silently stopped sidecars
  being reaped when the primary died. Ctrl-C still reaches one-shot commands such as a
  `buildah build`.
- **Ctrl-C during `outrig image build`'s validation removes the validation container.** The
  command left SIGINT at its default, so a Ctrl-C killed outrig before anything could clean up.
  The container it had started to test the image kept running `sleep infinity` until it was
  removed by hand, and `outrig clean` skips a running container. A server that never answers
  `initialize` is the usual reason to press Ctrl-C there. SIGINT, SIGTERM, and SIGHUP now stop the
  probe and remove the container before outrig exits with `130`, `143`, or `129`. The build step
  is unchanged: there buildah gets the Ctrl-C itself and removes its own working containers.
- **`outrig` raises its soft open-file limit to the hard limit when it starts.** The soft limit is
  commonly 1024. Under `--network audit` or `filter` each connection a container makes holds two
  of outrig's descriptors, so a few busy containers could exhaust the limit together. LLM calls
  and `podman exec` pipes then failed with the container's connections. The library's
  per-container cap of 256 connections keeps one container from doing this alone; the higher
  limit covers several. A limit that cannot be raised is left as it was.
- **Every `5xx` from an LLM endpoint is retried, as documented.** Only `500`, `502`, `503`, and
  `504` were. Any other `5xx` -- Anthropic's `529` "overloaded", Cloudflare's `520`-`527` in
  front of a proxied endpoint -- ended `outrig run` on its first occurrence and tore the session
  down. These are now retried within `retry-budget-secs` like the rest. If the endpoint never
  recovers, the turn ends with the usual notice and the conversation is kept. A failover chain
  also counts these failures as recoverable now.
- **`outrig__wait_results` no longer waits on a subagent you already collected that is idle.**
  Its description told the model that a collected name came back again immediately. In fact a
  collected name counted only once the subagent reported again, and an idle subagent reports again
  only after `outrig__subagent_send`, which the parent cannot call while it waits. So the natural
  pattern -- wait for one, read it, wait on every name again -- blocked until Ctrl-C, since the
  default `min_count` needed every name. Such a subagent is now left out of the wait: it does not
  count toward `min_count` and is not listed, and a call naming only such subagents fails with an
  error that says to send them more work. One still working after its result was collected is
  waited on as before. The description now says so.
- **The walk up from the current directory takes only a config you own.** It took the first
  `.agents/outrig/config.toml` above the working directory, whoever had written it. So on a
  shared host, a config another user planted in `/tmp` or a group-writable directory became the
  config of every `run`, `mcp`, `build`, `image add`, `ls`, `logs`, `discard`, and `clean` below
  it that had none of its own. It could mount your home read-write, run MCP commands as you,
  resolve build-args from your environment, and send your API key to a host of its choosing. A
  config found by the walk is now refused, with an error naming it, unless you own it, the
  `.agents/` and `outrig/` directories above it, and the directory they sit in; symlinks are
  judged as themselves. As with git's refusal of another user's repository, this is an error
  rather than a fall back to a config-less run. `--config` names a file on purpose and is not
  refused. `run` and `mcp` now name the config file they read, and the workspace they mount,
  before anything is built or started.
- **A signal ends `outrig run` and `outrig mcp` through teardown.** A Ctrl-C before the REPL's
  first prompt, or a SIGTERM or SIGHUP at any time -- `kill <pid>`, a closed terminal, a service
  manager or CI timeout -- killed outrig outright. The containers ran on, and the record never
  ended, so `outrig discard` refused it as still running and `outrig clean` skipped it, leaving
  `podman rm -f` as the only way out. `outrig mcp` did the same during startup, and while serving
  it ignored SIGHUP. Both now stop what the session started, finalize its record, and exit with
  128 plus the signal number: `130`, `143`, or `129`, which the record keeps too. `outrig mcp`
  still exits `0` on a signal once it is serving. A SIGINT or SIGTERM while containers are being
  stopped ends the wait, and the rest are force-removed in the background.
- **A session that ends with stdin still open exits once it is torn down.** `outrig mcp` over
  stdio, stopped by a signal while its client held the pipe open, and `outrig run` left by a
  second Ctrl-C at the prompt both finished teardown and then waited -- for the client to close
  the pipe, or for someone to press Enter. The read in flight on stdin no longer holds the exit.
- **`outrig discard` and `outrig clean` remove only what outrig wrote.** `--session-dir` took any
  existing directory, and discard and clean later removed it recursively with the session, so
  `--session-dir .` in a checkout, or a directory of notes, went with it. `--session-dir` now has
  to be empty or not exist yet -- outrig creates it -- and one that holds anything, even a
  `.gitkeep`, is refused. Removal deletes `session.json`, `logs/`, and `outrig-enter`, then the
  directory only if that empties it; a directory that holds anything else stays, and the command
  names what it kept. That covers sessions already written into such a directory, except that a
  `logs/` the directory had before the session goes with the session's logs. And `discard
  --session-dir` given a session's link under the session root removes the directory the link
  names; it removed only the link, and reported the directory removed.
- **`host.containers.internal` reaches the host under `--network audit` and `filter` on podman
  5.** A tool reaching a service on the host by that name, or `host.docker.internal`, timed out
  after 20 seconds once interception was on, and the connection was recorded as allowed with zero
  bytes. Podman 5's pasta maps the alias to `169.254.1.2`, an address only the container
  understands, and the interceptor now dials it where pasta would. Its record still names
  `169.254.1.2`, so a filter entry for the host names that address; see the `[network]` reference.
  Fixed in `outrig`; see that crate's changelog.
- **A local model loaded by `model-path` answers prompts.** The model loaded, and then every
  request to it failed with `ModelNotFound("<file name>")`, including a subagent's and a local
  fallback's in an alias chain. The request named the GGUF file, and mistralrs had registered the
  model under its directory. A request now names no model, and the engine, which holds only that
  one, routes it there. A `model-id` model was unaffected.

## [0.2.1](https://github.com/tgockel/outrig/releases/tag/outrig-cli-v0.2.1) - 2026-10-04

The first patch release since 0.2.0: fixes, and one change to the MCP revision `outrig mcp`
negotiates. Two of the fixes are worth knowing before upgrading. A session whose default
workspace would be your home directory, or a directory above it, now refuses to start rather than
mount it read-write; declare `[workspace] host-path` to mount it on purpose. And every build image
whose context is in a git repository gets a new cache key, so each rebuilds once.

### Changed

- **`outrig mcp` and `outrig mcp self` answer an `initialize` asking for `2026-07-28` in
  `2025-11-25`.** That revision replaced the handshake with per-request metadata, so an
  `initialize` cannot grant it; the MCP SDK enforces that since the move to rmcp 3.4. A client
  speaking `2026-07-28` names it in every request's `_meta` and is answered in it. A revision
  named that way that outrig does not serve is refused with an error listing the ones it does.

### Fixed

- **`--network filter` with `default = "deny"` let any UDP tool through.** The interceptor's
  redirects carry TCP and DNS; a datagram of any other kind -- QUIC and so HTTP/3, ICMP, anything
  else -- left by podman's default route, unfiltered and unrecorded. It is now dropped in the
  kernel under a deny default, and the sending tool fails at once with "Operation not permitted".
  Fixed in `outrig`; see that crate's changelog for what the chain accepts. A dropped datagram
  still writes no `network.jsonl` record; recording them is #419.
- **A `${VAR}` build-arg or MCP `env` value stays out of outrig's output and `ps`.** A config's
  `build-args` or `env` entry, or an `--env [SERVER:]KEY=${VAR}`, was resolved before podman or
  buildah was called and printed as its value wherever the command was. A failed `outrig build`
  or entrypoint-sidecar create put it on stderr with no flag given -- in CI, into the job log.
  `-v` wrote it to `container.log` and the terminal, `RUST_LOG=debug` traced it, and every
  exec-stdio server's `podman exec` carried it on a command line any local user could read for
  the whole session. Each is now shown as `KEY=${VAR}`, and podman and buildah get the value
  through their environment instead. A key they read themselves, such as `HOME`, `TMPDIR`, or
  `HTTPS_PROXY`, keeps its value on their command line, except a proxy variable referencing the
  variable of its own name; the config reference's MCP `env` value syntax lists them. buildah
  still records a build-arg's value in the image's history.
- **A workspace outrig picks by default is never your home directory.** With no config declaring
  `[workspace] host-path`, the workspace is the repo root, and nothing checked what that was.
  `outrig run` or `outrig mcp` from `~` with no repo config, an MCP client that started outrig in
  `/`, or a stray `~/.agents/outrig/config.toml` above the working directory mounted the home
  directory, or one above it, read-write at `/workspace`, with `~/.ssh` and `~/.gnupg` inside.
  Such a session now refuses to start, naming the directory and whether a repo config or the
  current directory chose it. Declaring `host-path`, such as `"~"`, still mounts it on purpose, as
  `--volume` does.
- **`--config` reads the file it names.** `outrig run`, `outrig mcp`, and `outrig build` took the
  directory three levels above the path as the repo and read that repo's
  `.agents/outrig/config.toml`, never the named file. The two agree only for a path that already
  ends that way. Any other was ignored without a word: the command ran on whatever config sat
  three levels up, or none, and for `run` and `mcp` that directory -- for an absolute path, an
  ancestor such as your home directory -- was mounted read-write as the workspace. The named file
  is now the one read. One at `<repo>/.agents/outrig/config.toml` still means that repo; any other
  is read for the repo found from the current directory, as without the flag, and its relative
  paths resolve beside it, as a `--global-config` file's do. A `--config` that is not an existing
  file is now an error wherever the flag is read, where `run` and `mcp` started config-less and
  `ls`, `logs`, `discard`, and `clean` went on without it.
- **A tool result that is JSON reaches the model as the tool returned it.** rig, which runs the
  agent loop, read every tool result for structure. One that was a JSON object with a top-level
  `response` key reached the model as that value alone, with nothing to say the rest was missing:
  a WireMock stub mapping read through the `fs` server came through without the `request` it
  matches, and an Ollama reply without the `done_reason` that says it was cut off. One carrying an
  image -- in a `parts` list, or shaped as one -- was sent as that image. An OpenAI-style provider
  cannot be sent an image there, nor the native Anthropic one an image given by URL, so the
  request failed to build and `outrig run` ended, or a subagent's round failed. Every tool result
  now reaches the model as the text the tool returned.
- **A subagent round that ends early says so under the subagent's name, without advice meant for
  your own turn.** When a subagent's round hit its tool-call max or the repeat breaker, or a model
  call in it failed for good, stderr got the lines printed when the primary's turn ends early,
  with no label: `[outrig] <reason>; ending turn`, then advice to send the prompt again or
  `/quit`, to send `continue`, or to `/reset`. That advice acts on the primary's conversation:
  `/reset` cleared the primary's history, and resending reran the primary's turn. The reason now
  carries the subagent's label, as its tool calls do, and the advice is left out, since what a
  subagent does next is its parent's call.
- **A follow-up sent to a busy subagent after it reported gets an answer.** `outrig__subagent_send`
  to a subagent whose round had already called `outrig__set_result` reached its model on the next
  tool result, but the round still counted the earlier report as its answer. When the subagent
  then ended the round without reporting again, `outrig__get_result` for a parent that had read
  the report waited until interrupted, and `outrig__wait_results` never listed the subagent. The
  parent now reads `subagent stopped without calling outrig__set_result`, as it does for a
  follow-up that started a round of its own; a parent that had not read the report yet gets the
  report first.
- **A subagent's report outlasts a failure later in its round.** When a subagent reported with
  `outrig__set_result` and a model call after that failed for good -- a rate limit or outage that
  outlasted `retry-budget-secs`, every candidate in an alias chain failing, a response outrig
  could not use, a request the endpoint refused -- `round failed: <reason>` was published over
  the report. A parent that read once the round was over was told the round failed, and the report
  was gone from the inbox and from the transcript's outcome; one that read sooner got the report,
  then the failure. The report now stands, and the failure is noted beside it, on stderr and in
  the subagent's transcript. A message the parent sent after the report is still answered with
  the failure, since the report could not have answered it.
- **A turn that produced only reasoning reaches the next request on an OpenAI-style provider.**
  With `style = "openai"`, a turn whose reply was nothing but reasoning, such as one cut off at
  the output-token ceiling, stayed in the conversation but was left out of every later request,
  a subagent's later rounds included. The prompts on either side of it reached the model as two
  user messages in a row: the model answered without knowing it had taken that turn, and an
  endpoint that requires alternating roles refused the next prompt with a `400` that ended
  `outrig run`. The turn is now sent as an assistant reply with no text and its reasoning in
  `reasoning_content`, as reasoning beside text already was. The advice printed after such a turn
  no longer asks you to restate what you need rather than refer back to it; it says instead that
  the model may not see the reasoning printed above it.
- **A message sent to a busy subagent no longer fails its round.** `outrig__subagent_send` to a
  subagent partway through a round put the message between the subagent's latest tool call and
  that call's result, which OpenAI and Anthropic refuse. Every model call but a round's first
  follows a tool call, so nearly every such message failed the round: the parent read
  `round failed`, and the subagent's history lost the round's work. The message now reaches the
  subagent appended to its next tool results, and stays there in its history.
- **A subagent result a turn read but did not keep can be read again.** `outrig__get_result` moves
  the parent's read position past the result it returns as it runs, but the result reaches the
  conversation only when the turn keeps it. Ctrl-C while another tool call made alongside the read
  was still running dropped the result with that call -- and a subagent round that ended in an
  error dropped every call it made -- but the read position stayed moved, so the next
  `outrig__get_result` for a subagent with nothing newer to report blocked until interrupted, and
  `outrig__wait_results` never listed it. A read the turn does not keep now moves the read position
  back, and the next read returns that result; one the conversation keeps stays consumed.
- **A turn that fails after running a tool call keeps the call and its result.** When a model
  call failed for good -- a rate limit or outage that outlasted `retry-budget-secs`, every
  candidate in an alias chain failing, a response outrig could not use -- the turn ended with the
  conversation as it stood before the prompt, and outrig advised sending the prompt again. Past
  the turn's first model call, that conversation was missing tool calls that had already run, so
  the resend could run them a second time: a file written twice, a command run again. Ctrl-C
  mid-turn dropped them the same way. The tool calls whose results had gone back to the model now
  stay in the conversation with those results, and outrig advises sending another prompt (e.g.
  "continue") instead. A failure on a turn's first model call still leaves the conversation
  unchanged. A subagent round that fails this way keeps its tool calls for the round its parent
  starts next.
- **A local reply cut off inside a multi-byte character warns that it may be incomplete.** With
  `local-llm`, a turn whose `max-tokens` ceiling landed partway through a character such as an
  emoji ended as if it had finished. mistralrs-core 0.8.1 drops that turn's last chunk, and with
  it the finish reason, the cut character, and any text it was still holding back, which is the
  whole reply when it opened like a tool call. When the cut came before anything was sent, the
  empty stream was reported as an error that ended `outrig run`. outrig now warns on stderr that
  the reply may be incomplete, keeps what did arrive, and ends only the turn. The missing text
  cannot be recovered.
- **Subagents sharing a name keep separate transcripts.** A name is unique only among one agent's
  subagents, so two subagents could each launch an `audit`, and a subagent could give a child its
  own name, but every subagent wrote `logs/subagent-<name>.log`. Same-named subagents appended to
  one file, their prompts and outcomes mixed with nothing to tell them apart. A subagent launched
  by another subagent now writes into a directory named for its parent, such as
  `logs/subagent-parent-a/subagent-audit.log`; the primary's subagents keep their file names. Each
  launch starts its transcript with a header naming the subagent's whole path, such as
  `=== subagent parent-a/audit ===`, including a launch that named no model, which used to write
  no header. `outrig logs <session>` lists the nested transcripts too.
- **`--config .agents/outrig/config.toml` takes `.` as the repo root**, as its `./`-prefixed
  spelling always did. A relative path of exactly three components derived the empty path
  instead, so a bare `model-path = "local.gguf"` validated clean and then reached the mistralrs
  loader with an empty directory, which it looks up on Hugging Face rather than opening.
- **`outrig config init` says what a relative `model-path` is relative to.** The local-path
  prompt now reads `Local model-path (absolute, or relative to the repo root)`, and its `?` help
  recommends an absolute path for the global config, which serves every repo. An answer relative
  to the directory `config init` ran in wrote a config that failed validation naming a file that
  exists. The answer is still stored as typed.
- **A subagent's children inherit its model, not its parent's.** A subagent launched with
  `model = "fast"` gave any child that named no model the model of the agent that launched *it*,
  with that model's provider and `max-tokens`, and its own `outrig__subagent` schema offered that
  model as "yours". Both now follow the model the subagent itself runs under.
- **A vetoed built-in default names the block that vetoed it.** With no image named, a repo
  declaring `[images.outrig-default-fs]` or `[images.outrig-default-shell]` ended startup with
  an error blaming a `[sidecars.<name>]` block, contradicting the note just above it. The error
  now names the declared block, the same one the note names, and says it leaves no
  `[images.outrig-default]` to fall back to.
- **Ctrl-C during a turn keeps the conversation.** Interrupting a prompt in `outrig run` emptied
  the session's history, so every later prompt reached the model with no earlier context, and
  nothing said so. The conversation as it stood before the interrupted prompt now survives, as
  the docs always promised; a turn interrupted before it finished is still not added to it.
- **A local model's reasoning-only turn is reported as reasoning.** With `local-llm`, a model
  whose chat template marks out its reasoning, as a `<think>` block does, lost that reasoning on
  the way in. A turn that produced nothing else, such as one cut off at `max-tokens` mid-thought,
  was reported to the user and to a subagent's parent as having produced no content at all. It
  is now reported as hidden reasoning, most likely cut off at the output-token ceiling, with the
  reasoning printed after it on stderr. Reasoning is still not streamed, so stdout carries only
  the reply.
- **An Anthropic model is sent back only the reasoning it issued.** An alias chain keeps one
  history across its candidates, so a turn another candidate answered stays in it. Reasoning in
  such a turn, like an OpenAI-style endpoint's `reasoning_content`, reached the Anthropic API as a
  `thinking` block with no signature, which it refuses. After one move, a chain headed by an
  Anthropic model failed at its head on every later call and stayed on its fallback. Reasoning
  Anthropic did not issue, a local model's included, is now left out of what it is sent; text,
  tool calls, and Anthropic's own thinking are sent as before.
- **The GGUF picker lists files in a repo's subdirectories.** With `local-llm`, the picker that
  `outrig config init` and `outrig init` offer after a Hugging Face `model-id` listed only the
  repo's top level, so a quantization kept in a directory of its own was never offered. A repo
  holding only those, like `unsloth/DeepSeek-R1-GGUF`, was refused as holding no `.gguf` files,
  and setup ended there without writing a config. The picker now lists the whole repo and writes
  each pick's path inside it, directory included. Enter takes the first file that is a whole
  model by itself: never one shard of a split quantization, which would download in full and
  then fail to load, and nothing at all when the repo holds only shards. A listing with no
  `.gguf` file now drops to the manual `model-file` prompt instead of ending setup. A revision
  containing `/`, like the `refs/pr/N` Hugging Face gives a pull request, now lists too; it used
  to fail and fall back to that prompt.
- **`outrig mcp --listen` no longer strands a client whose `initialize` asks for a revision
  outside the served list.** The request was routed by the revision it asked for, so one sorting
  at or after `2026-07-28` took the stateless path and got no `Mcp-Session-Id`, yet was answered
  in `2025-11-25`, a revision that needs one. Its next request was refused with `422`. Every
  `initialize` now opens a session.
- **A message sent to a subagent as it finishes a round reaches it.** `outrig__subagent_send`
  answered `injected into the round in flight` for a subagent whose round had already made its
  last model call, and the message never reached the model. The subagent went idle, and the
  message surfaced only if a later round ran, attached to that round's work. It now runs as a
  round of its own, in the order it was sent, behind any message already waiting. If the round
  failed instead, the message is kept for the parent's next prompt.
- **A subagent with a round waiting no longer reads as stopped.** A subagent whose round ended
  without `outrig__set_result`, and that was already sent another message, went idle between
  the two rounds. `outrig__get_result` or `outrig__wait_results` in that gap, such as one made
  straight after the send, answered with how the old round stopped instead of waiting for the
  new one. A subagent now stays working while a round is waiting, and a message sent to an idle
  one ends that stop at once.
- **A subagent the repeat breaker stopped can still be redirected.** The breaker ends a
  subagent's round once the same tool call has failed four times in a row, and it left that
  fourth call without a result in the subagent's history. OpenAI and Anthropic reject a history
  in that shape, so every later round on the subagent failed, including one started by
  `outrig__subagent_send`, until the subagent was released. The fourth call now gets its result,
  with a note that the round ended there, before the round stops.
- **A run refused for an unknown `--env` server no longer lists as running.** `outrig run` and
  `outrig mcp` refused a `--env SERVER:KEY=VALUE` whose `SERVER` the image does not declare only
  after writing the session record and starting the container, and then exited without
  finalizing the record: `outrig ls` showed the failed run with no exit code and a duration that
  kept growing, and its containers were left to a detached removal. The name is now checked as
  soon as the MCP table is merged, before any sidecar starts, and the refusal ends the session
  like any other startup failure: exit code 1, containers stopped, and a container borrowed with
  `--attach` left running. `outrig mcp show-merged`, which ignored such a name, now refuses it.
- **The `rust` toolchain `outrig image add` generates works as you.** The generated Dockerfile
  installed rustup into root's home and put only `/root/.cargo/bin` on `PATH`, but outrig execs
  nothing as root: every exec gets your UID and `HOME=/home/<user>`. `/root` is `0700`, so
  `cargo` was `Permission denied`, and where it could be reached, rustup looked for its
  toolchains under that `HOME` and reported that it could not choose a version of cargo to run.
  The image built cleanly either way. The toolchain now installs where the official `rust` images
  put it, with `RUSTUP_HOME=/usr/local/rustup` and `CARGO_HOME=/usr/local/cargo` both writable,
  so cargo can fill its registry cache as you. It is also the `minimal` profile plus `rustfmt`
  and `clippy`, the components `image add` lists, where it was the `default` profile: the image
  no longer carries the offline docs `rustup doc` opens, about 0.9 GB. `outrig init`, which runs
  `image add`, gets the same toolchain. `outrig design prompt` shows it in both Rust examples, and
  its rules now say to install tools outside root's home. A Dockerfile generated earlier keeps its
  old lines; replace its `# rust toolchain` section with the one in `doc/usage/image.md`.
- **`outrig image add` takes an inline `images` table.** A repo config spelling `images` as an
  inline table, `images = { base = { ... } }`, which outrig loads like any other, made `image add`
  panic after it had written the new Dockerfile. The config was left without the block, and the
  Dockerfile then refused a plain retry, while `--force` panicked the same way. `outrig init`,
  which runs `image add`, did too. The inline table is now rewritten as a standard `[images]`
  table holding the same entries, with the new `[images.<name>]` block after it; comments between
  the entries of a multi-line inline table are not kept. An `images` that is not a table at all,
  such as `images = "legacy"` or `[[images]]`, is refused before the first prompt, where it
  panicked too. Nothing is written until every prompt is answered, and a config that can't be
  written leaves the Dockerfile unwritten too.
- **`outrig image add` refuses a name it can't build.** The name was used as given, while the
  block it writes is held to the build-image rule when the config loads. `outrig image add
  RustDev` wrote a Dockerfile and an `[images.RustDev]` block that `outrig build` and `outrig run`
  then refused, and every image-config in the file stopped loading with it. A name like `../x` or
  `/tmp/x` put the Dockerfile outside `.agents/outrig/images/`. The name must now be one the image
  can be built under: lowercase letters and digits, separated by one `.`, one or two `_`, or a run
  of `-`. One passed as `<name>` that isn't is refused before the first prompt, even with
  `--force`, and nothing is written. One typed at the prompt, `outrig init`'s included, is asked
  for again. Either way the error is the one loading a config with that block gives, and says
  what is wrong with the name. The prompt's `?` help used to advertise a rule that allowed
  uppercase. A repo folder whose name holds a letter outside ASCII now suggests `standard` rather
  than a name Enter could never get past.
- **`outrig image init` refuses a name its image can't be tagged with.** The directory name
  becomes the image's ref, which podman requires be lowercase, yet `RustDev` passed, and
  `outrig image build` then failed to tag the image. The name now follows the same rule as `image
  add`'s. That also admits names the old rule refused, such as `rust.dev` or `2024-tools`; the
  generated README quotes a dotted name in its `[images."rust.dev"]` header. A name of 64 hex
  digits is refused, whatever it starts with: the ref carries no tag, and podman reads it as an
  image ID.
- **The `go` toolchain `outrig image add` generates runs on AArch64.** The generated Dockerfile
  downloaded Go's x86-64 archive on every machine, and an archive unpacks whatever it holds: on
  an AArch64 host the image built cleanly, and `go` then failed with `exec format error` the
  first time anything ran it. The archive is now chosen at build time for the architecture the
  image is built on, x86-64 or AArch64, and checked against its published SHA-256 before it is
  unpacked; on any other architecture the build fails naming it. The toolchain is also
  Go 1.27.1, where it was 1.22.0, which Go stopped supporting in February 2025. `outrig init`,
  which runs `image add`, gets the same toolchain. `doc/concepts/containers.md` and the rules
  `outrig design prompt` gives now say to download a prebuilt binary for the image's
  architecture. A Dockerfile generated earlier keeps its old lines; replace its `# go toolchain`
  section with the one `image add` writes now.
- **`outrig mcp self` refuses `--env`, `--network`, and `--volume`.** It started with any of them
  and ignored them, so a client configuration carrying one looked correct, and an `--env` every
  other `outrig mcp` path refuses as malformed was taken too. Each now exits with an error naming
  the option to remove, as `--image`, `--session-dir`, `--attach`, and `--listen` already did.
- **`outrig image add <name>` in a fresh repo makes `<name>` its `default-image`.** Where no
  `.agents/outrig/config.toml` exists yet, `image add` first sets one up through `outrig init`'s
  prompts, which asked for an image-config name of their own and wrote it as `default-image`;
  the image-config itself was scaffolded under `<name>`. Accepting the suggested
  `<repo-folder>-standard` left a `default-image` naming no image-config, so `outrig build` and
  `outrig run` refused the config. Given a `<name>`, the setup now writes it as `default-image`
  without asking for one; given none, it asks, and the answer names both. A repo that already
  has a config keeps its `default-image`.
- **A `[workspace] host-path` that doesn't exist is refused when the config loads.** A typo in
  it validated, and `outrig run` failed only at `podman run`, with podman's `statfs` error, which
  names the resolved path but neither the key nor the file. `outrig run`, `outrig mcp`, and
  `outrig build` now refuse it at load, as they already refused an extra mount's missing
  `host-path`, with an error naming the value as written and the config file that declared it.
  A `host-path` that names a file rather than a directory is refused the same way. The default
  `.` is checked too, as the repo root, so `--config <root>/.agents/outrig/config.toml` with a
  `<root>` that doesn't exist now stops at load rather than at `podman run`.
- **A `~` at the start of a config path is your home directory.** The sidecar example in the
  config reference and the containers page mounts `host-path = "~/.cache/example"`, and a config
  holding it could never load: `~` was taken as a directory of that name beside the config, and
  the error said `"~/.cache/example" does not exist` even when it did. `~` alone, or as the first
  component of a path, now stands for your home directory in `dockerfile`, `context`,
  `model-path`, and every `host-path`, from either config file, and in `--volume` and a
  standalone `image.toml`'s `[build]` paths. `~user/...` is not expanded, and neither is
  `${VAR}`.

## [0.2.0](https://github.com/tgockel/outrig/releases/tag/outrig-cli-v0.2.0) - 2026-09-23

The first release since 0.1.0. Everything below is measured against **0.1.0**, and
**Migrating from 0.1** is the ordered list of what a 0.1 config or command line has to change
-- including the two breaks that announce themselves nowhere.

### Migrating from 0.1

Everything a 0.1 config or command line has to change, by name. Most announce themselves --
the config fails to parse, fails to validate, or the session says so on stderr. Two do not and
want checking by hand: a `[sidecars.<sc>]` block that no `[mcp]` entry names now simply does
not start, with nothing said about it, and an agent that omits `preamble` stops sending the
sentence 0.1 supplied on its behalf.

- **Toolchain and platform.** The minimum supported Rust version (MSRV) is 1.88, up from
  1.87. outrig builds for Linux on x86-64 and AArch64 only; any other target stops at a
  `compile_error!` rather than failing somewhere in the container plumbing, and on Windows the
  supported arrangement is WSL2. podman 4.3 or newer is required at run time. Building
  `view = "primary"` sidecars additionally needs the matching `<arch>-unknown-linux-musl`
  target installed -- without it the built-in default degrades rather than failing, with `fs`
  falling back to a read-write bind mount of the workspace and `shell` unavailable.

- **`outrig design` is `outrig design prompt`.** The bare form was never a command and exits
  with a usage error.

- **Sidecars are declared at the top level** as `[sidecars.<sc>]` rather than inside an
  image-config, and one starts only when some `[images.<name>.mcp]` entry names it. Declaring
  a block instantiates nothing on its own, which retires two shapes: a sidecar hosting no MCP
  servers at all, and one whose servers came only from its own image's `org.outrig.mcp` label.

- **An agent that omits `preamble` sends no system prompt.** 0.1 substituted a fixed sentence
  that appeared in no config and could not be turned off -- `You are a careful assistant whose
  tools run inside a sandboxed container.` Paste it into `preamble` to keep it. Agents that
  set `preamble` are unaffected, and so are subagents.

- **An alias fails over mid-turn.** A `[models.<name>]` entry naming other models is no longer
  fixed to one endpoint at startup: a failing request moves to the next candidate under a
  budget shared across the chain. If you relied on a name resolving to exactly one endpoint,
  declare it with a single target, which takes the pre-failover path unchanged.

- **A repo config may declare only `mode` under `[network]`.** `default`, `allow`, and `deny`
  in a repo file are a validation error. The rule predates this release, but it was applied by
  scanning the config text and a differently formatted table could slip past it; it now reads
  the parsed value, so a repo policy that previously went through is refused. A `[network]`
  table with no `mode` inherits the global mode instead of resetting it to `default`.

- **`style = "mistralrs"` is deprecated, not removed**, along with the `local-llm`, `cuda`, and
  `metal` features, the six `[models.<name>]` weight keys, `model-cache-root`, and
  `outrig run --device`. Everything still parses, validates, and runs. Two things do change:
  a provider block carrying a key this style never read -- `base-url`, `request-timeout-secs`,
  `retry-budget-secs`, or an outright typo -- now fails to load instead of being discarded,
  and a relative `[models.<name>].model-path` is opened against the repo root rather than the
  process's working directory, so one written in a *global* config follows whichever repo is
  current. Give that one an absolute path. The replacement is an OpenAI-compatible server on
  `localhost` behind a `style = "openai"` provider.

Config file names and locations are unchanged, and no key not listed here changed spelling.

### Added

- **`outrig clean --session <id>`**, which narrows both sweeps to one session. Only that
  session's record is considered for removal, and only containers labeled
  `org.outrig.session=<id>` are stray candidates. The sweep is machine-wide without it, and a
  stray is defined by the *absence* of a record -- so a container whose record lives under a
  different `--session-root` (another checkout, a parallel CI job) has no record the sweep can
  see and is removed as a stray. The default is unchanged.

- **`[<...>.security]` accepts `unmask`**, so a session container can host a container runtime
  of its own. `outrig run` carries the key from the selected image-config onto the primary and
  from each `[sidecars.<sc>.security]` block onto that sidecar. Combined with
  `cap-add = ["SYS_ADMIN"]` and `devices = ["/dev/fuse", "/dev/net/tun"]`, `unmask = ["/proc/*"]`
  is what lets an agent build an image or run a throwaway container inside its own session --
  see [Nested container runtimes](../../doc/concepts/containers.md#nested-container-runtimes),
  which also retracts the `newuidmap` rationale that page previously gave for
  `no-new-privileges`.

- **`[models.<name>]` entries can be aliases**, so `--model`, `default-model`,
  `[agents.<name>].model`, and a subagent's `model` argument all accept a name that stands
  for one other model or for an ordered set of provider-equivalent ones. The session picks
  the first candidate this build can reach -- provider defined, style compiled in, `api-key`
  variable set and non-empty -- which is what lets one committed config serve a laptop with
  `ANTHROPIC_API_KEY` and a CI runner holding a Bedrock role.

  Selection happens at startup and answers "am I configured for this" rather than "is this
  endpoint up": building a remote client does no network I/O, so nothing here can tell a
  rate-limited endpoint from a healthy one. Whether an endpoint answers is settled per
  request instead, by the chain failover below. An alias none of whose candidates this build
  is configured for ends the session listing every candidate and the distinct reason each was
  skipped.

  Attribution follows the name: the banner, the subagent launch trace, the subagent tool
  result, and the transcript header all render `alias -> concrete` when a hop was taken. A
  direct model name prints byte-for-byte what it printed before.

  `--device` is refused for an alias spanning more than one candidate -- it selects hardware
  for one in-process model, and an alias may span styles. A single-target alias takes it.

- **An alias fails over mid-turn.** A request that fails against one candidate moves to the
  next from inside the same model call, so the turn continues rather than ending. The move is
  announced on stderr (`[outrig] model <a> failed (<reason>); trying <b>`), and a chain with
  nothing left reports every candidate and the distinct reason each failed -- ending the turn
  if any of those reasons was recoverable, and the session only if all of them were terminal.

  Three properties govern what it costs. The retry budget is **shared across the chain**: one
  deadline is armed per model call and every candidate's retry loop is bounded by what remains
  of it, so three candidates cannot each spend a full `retry-budget-secs` against one outage.
  A fresh request **restarts at the head** of the chain, because the list is a preference order
  and a single rate-limit window should not quietly demote it for the rest of the session. And
  **completed tool calls are never replayed**: tools run between model calls and never inside
  one, so moving candidates within a call re-runs nothing that already happened in a container.

  A chain of one takes the pre-failover path byte for byte -- same retry stack, same errors,
  same output. The banner lists the fallbacks up front, so the ordering is visible before a
  turn needs it.

  The chain's `retry-budget-secs` comes from the first *remote* candidate's provider, which is
  not always the first candidate: the lookup skips `style = "mistralrs"` rows, because an
  in-process provider has no such key and rejects one. So a local-first alias takes its budget
  from the first OpenAI- or Anthropic-style row after the local ones, and writing
  `retry-budget-secs` onto the local provider to influence the chain does not configure it --
  it fails to load.

- **Subagents.** An agent can launch another agent in the same session through the built-in
  `outrig__subagent` tool, which occupies a reserved `outrig__` namespace alongside the
  documentation tools rather than coming from any MCP server. A subagent gets its own turn
  loop and reports back through a `set_result` protocol fragment composed into its preamble.
  `subagent-depth-max` bounds how deep the nesting goes and `subagent-width-max` how many one
  agent may hold at once; the launching agent may name the `model` its subagent runs under,
  and a release list naming an unknown subagent is rejected whole rather than half-applied.

- **A built-in default image-config**, so a session that names no image no longer fails.
  `--image`, `agents.<n>.image`, and `default-image` gain a fourth rung below them, which
  makes `outrig run` work in a repo with no `.agents/outrig/config.toml` at all -- a global
  config that resolves a model is all it needs. The two errors it replaces
  (`no --image or default-image configured` and its three-rung sibling) are gone. What gets
  injected is ordinary config, at the bottom of the precedence order:

      [images.outrig-default]
      image-name = "docker.io/library/buildpack-deps:bookworm-scm"

        [images.outrig-default.mcp]
        fs    = { sidecar = "outrig-default-fs" }
        shell = { sidecar = "outrig-default-shell" }

  `buildpack-deps:bookworm-scm` is the smallest `docker.io/library` image with `git`, `curl`,
  and `ca-certificates` that sets no `ENTRYPOINT` and bakes in no user. Both servers run as
  `view = "primary"` sidecars, so the commands `shell` spawns resolve in the primary's
  filesystem -- which is why the primary is the container that needs `git`. `fs` is pulled
  from `docker.io/mcp/filesystem:latest`; `shell` is built from a Dockerfile outrig writes
  into the user cache directory, never into your repo. All three are reachable by name from
  both `--image` and `outrig build`, so the first-run cost can be paid deliberately:
  `outrig build --image outrig-default`, then `--image outrig-default-fs`, then
  `--image outrig-default-shell`.

  `outrig-default`, `outrig-default-fs`, and `outrig-default-shell` are reserved names.
  Declaring any one of them shadows the built-in entirely -- injection is all-or-nothing,
  since a half-injected set is a broken config -- and outrig says which file took the name.
  `outrig build --all` skips the built-in: it means the image-configs you declared.
  `default-image = "outrig-default"` remains an error, because that key is validated before
  the built-in is injected; you never need to write it.

  Without the `outrig-enter` launcher (a build with no `<arch>-unknown-linux-musl` target)
  the built-in degrades instead of failing: `fs` switches to `workspace = "rw"`, giving the
  same file tools over a bind mount, and `shell` is dropped. A bind-mounted shell would see
  none of the primary's toolchain, so reporting a different environment than the one you have
  is worse than reporting none.

- **outrig's own documentation tools in a built-in-default session.** The eight tools
  `outrig mcp self` serves -- `outrig__list_docs`, `outrig__get_doc`,
  `outrig__get_config_schema`, `outrig__list_base_images`,
  `outrig__list_mcp_server_suggestions`, `outrig__validate_dockerfile`,
  `outrig__validate_config`, `outrig__validate_image_toml` -- are offered to the agent
  directly, beside `outrig__subagent`. They are pure host-side functions over embedded data,
  so there is nothing to install in an image. They appear **only** when the session fell
  through to the built-in default, which is exactly the user who has not written a config
  yet; a configured repo's tool list is unchanged. `outrig mcp self` is untouched and remains
  the surface for external authoring tools.

- **`args` for entrypoint-stdio MCP servers**, on an `[images.<name>.mcp]` entry or on the
  sidecar block it names, so an off-the-shelf image that takes its configuration positionally
  (`docker.io/mcp/filesystem` and most of the MCP catalog) runs from config alone:

      [images.coding.mcp]
      fs = { image = "docker.io/mcp/filesystem:latest", args = ["/workspace"] }

- **A named sidecar can be an entrypoint host** -- omit `command` on the entry naming it and
  that container's ENTRYPOINT is the server, which is how such a server gets a workspace view,
  mounts, or its own security policy.

- **Agents can run against Anthropic's native Messages API** by selecting a
  `style = "anthropic"` provider. Turns take the same non-streaming path as `openai`, with
  the MCP tool loop, tool-call cap, tool-result truncation, conversation history,
  sidecar-added tools, and subagents all unchanged; only the wire format differs. The banner
  reports `provider: anthropic`.

  Anthropic requires an output-token ceiling on every request, and outrig always sends one:
  `max-tokens` from the agent or the model if set, otherwise the published ceiling for a
  Claude identifier it recognizes, otherwise a fallback of `32768` announced once on stderr.
  That last tier covers an older model, a proxy's own naming, and any Claude newer than the
  pinned rig release; it errs high on purpose, since a model whose real limit is lower rejects
  the request and names that limit, whereas too low a ceiling truncates replies with nothing
  logged. `outrig config init` offers `anthropic` as a provider style -- defaulting to
  `https://api.anthropic.com` and `ANTHROPIC_API_KEY` -- and prompts for `max-tokens`
  alongside the model identifier, so a generated config carries an explicit ceiling anyway.

  Where a missing ceiling does still surface as an error, outrig words it itself rather than
  passing through the provider's `` `max_tokens` must be set for Anthropic ``: the key in
  config is `max-tokens`, under `[models.<name>]` or `[agents.<name>]`, and the message says
  so.

- **`retry-budget-secs`**, at the top level and on any remote provider, bounds how long a
  transiently-failing LLM call keeps retrying. Defaults to `600`; `0` disables retries; the
  ceiling is `3600`. A provider's own value wins over the top-level one, which wins over the
  default -- rate limits belong to the endpoint, so per-provider is usually the right place.

- **MCP sidecar containers** -- an `[images.<name>.mcp]` entry can declare `sidecar = "<sc>"`
  to run the server in a sidecar declared at the top level as `[sidecars.<sc>]`, or an inline
  `image` to give it a dedicated anonymous one. A declared sidecar carries its own image,
  workspace access, mounts, and security; a referenced `start = "auto"` sidecar comes up with
  the session, and `start = "manual"` waits for `/sidecar add`. Declaring a block starts
  nothing on its own -- see **Changed**.
- **Entrypoint-stdio servers** -- an `image` with no `command` runs that image's `ENTRYPOINT`
  as the MCP server, so off-the-shelf MCP images work without repo-side command knowledge.
  The `--env` overlay applies to them at container-create time.
- **`/sidecar` in the REPL** -- `/sidecar list` shows the declared sidecars and their status;
  `/sidecar add <name>` starts a `start = "manual"` sidecar mid-session, and its tools join
  the running agent.

### Changed

- **The `outrig__subagent` tool no longer advertises a model whose `api-key` variable is
  unset or empty.** The schema's `enum` and the alias selector are now one predicate, so a
  name is offered exactly when a launch could reach it. Previously an unset key was
  advertised and failed at launch. An alias is offered when *any* of its candidates is
  reachable, which is what lets `alias = ["opus-local", "opus-anthropic"]` stay launchable
  in a build without `--features local-llm` where naming `opus-local` directly would not be.

- **An endpoint that never answered gets a much shorter leash.** While every attempt against a
  candidate has failed to connect -- nothing has come back from it at all -- the retry loop is
  bounded at 30 seconds rather than by the whole `retry-budget-secs`, and each attempt's
  connect phase is capped at 10. A misconfigured `base-url` or an unreachable host now reports
  in well under a minute instead of spending the full budget on a socket that was never going
  to open. The short bound lifts the moment the endpoint answers anything, an error included,
  because at that point the failure is the provider's and the configured budget is the right
  one. It is also what makes an alias chain cheap to walk past a dead candidate.

- **A dropped future no longer leaves its subprocess running.** Interrupting a session used to
  leave podman and buildah children behind to finish on their own, so a cancelled build could
  still be writing layers after the command that asked for it returned. Cancellation now
  reaches the subprocess, and a command cut short reports `OutrigError::Canceled` naming the
  program and argv rather than an exit status it never collected.

- **A repo config may declare `mode` under `[network]`, and nothing else.** `default`, `allow`,
  and `deny` in a repo file are a validation error naming the key. The rule is not new, but it
  used to be applied by scanning the config text, which a differently formatted table could
  slip past; it now reads the parsed value, so a policy that a repo file previously smuggled
  through is refused. A `[network]` table that declares no `mode` also inherits the global one
  instead of resetting it to `default`.

- **The MCP SDK moved to rmcp 3.1**, two majors on from the 1.x this project shipped in 0.1.0;
  the intermediate 2.x step is folded into it. The revisions `outrig mcp` and `outrig mcp self`
  advertise are an explicit list of outrig's own rather than whatever the SDK knows, so an SDK
  bump cannot silently move the ceiling out from under a client; see **Fixed**.

- **`validate_dockerfile` now flags `ENTRYPOINT` rather than a missing `CMD`** (`outrig mcp
  self` and the `rig` self tool). OutRig appends `sleep infinity` after the image reference, so
  the container's command comes from OutRig and the image's `CMD` is overridden -- a Dockerfile
  that sets a different one, or none at all, was never the problem the old warnings described.
  The `cmd_missing` warning is gone, `cmd_may_exit` becomes `cmd_ignored` and says the `CMD` has
  no effect rather than that the container may exit, and a new `entrypoint_takes_args` warning
  covers the instruction that does break a primary image: podman appends trailing arguments to
  an exec-form `ENTRYPOINT` instead of replacing it, so such an image runs
  `<entrypoint> sleep infinity`. The documentation and `outrig design prompt` carried the same
  mistake and are corrected to match.

- **`outrig run` no longer needs an agent.** With neither `--agent` nor `default-agent`, the
  session starts against `--model` (or `default-model`) and `--image` (or `default-image`) and
  sends no preamble -- the same shape `outrig mcp` has always had, now with an LLM attached.
  Every agent-level knob falls through to its top-level default, subagents stay enabled, and
  the banner leads with `model:` in place of the usual `agent:` line. Declaring
  `[agents.<name>]` is still how you attach a preamble or per-agent limits; it is just no
  longer the price of admission. A `default-agent` that names nothing remains an error.

- **Breaking (config):** sidecars are declared at the top level as `[sidecars.<sc>]`, not
  `[images.<name>.sidecars.<sc>]`. Move the blocks up a level; the keys are unchanged. The old
  form is now an unknown-field parse error. One block can be shared by several image-configs,
  and the global config can declare sidecars a repo references.
- **Breaking (config):** a sidecar starts only when an `[images.<name>.mcp]` entry names it.
  Declaring a block no longer starts it -- with blocks shared and global, it cannot.

- **Breaking (config): an agent that omits `preamble` now sends no system prompt.** outrig used
  to fill the gap with a fixed sentence ("You are a careful assistant whose tools run inside a
  sandboxed container."), which appeared nowhere in config and could not be turned off. Unset
  now means unset, so an agent that was relying on that text needs to spell it out. Agents that set
  `preamble` are unaffected. Subagents are unaffected too -- their preamble is composed from the
  `set_result` protocol fragment plus whatever the parent passes.

- **Transient LLM failures are retried against a time budget, and honor `Retry-After`.**
  Retrying used to mean two attempts roughly a second apart with the server's own guidance
  ignored, which is not enough for a rate limit measured in minutes. outrig now retries until
  `retry-budget-secs` runs out, waiting exactly as long as a `Retry-After` header asks (both
  delta-seconds and HTTP-date forms, clamped at 300s) and falling back to jittered exponential
  backoff otherwise. Each retry prints the wait and the budget spent so far. The retried set is
  unchanged: `408`, `425`, `429`, `5xx`, timeouts, and connection errors.

  Reading the header meant moving the retry into outrig's own `http_client` implementation:
  rig's error type keeps a status and a body and drops every header, so nothing above that
  layer can see it. Retries still replay exactly one HTTP request, so no already-executed
  container tool call is repeated.

  A side effect worth having: the old wrapper cloned the whole `CompletionRequest` -- chat
  history, every tool definition and its JSON schema -- on *every* model call, including the
  overwhelmingly common one that succeeds first try. Replaying at the HTTP layer clones a
  header map and bumps a refcount on the serialized body instead.

- Upgraded the LLM/agent stack: `rig-core` 0.39 -> 0.40, plus routine dependency bumps
  (anyhow, ignore, jiff, rand, toml). rig 0.40's `max_turns` now
  counts total model calls rather than tool-call rounds; outrig compensates so the per-turn
  tool-call limit behaves as before.
- Egress policy and the audit log cover every sidecar container, not just the primary.
- `outrig clean` reads one `podman ps -a` instead of a `podman inspect` per aged session, and
  coalesces stray-container removal into a single `podman rm -f`.
- Slash commands run through one dispatcher. `/help` output is unchanged, but two edge cases
  of the old exact-match arms are gone: a trailing-space `/quit ` now executes, and
  tab-separated `/sidecar` arguments parse.

### Deprecated

- **The `local-llm` Cargo feature and the `style = "mistralrs"` provider**, along with the
  `cuda` and `metal` features that select a backend for them, the six `[models.<name>]` weight
  keys (`model-id`, `model-path`, `model-file`, `revision`, `context-length`, `device`), the
  top-level `model-cache-root`, and `outrig run --device`. They will be removed in a future
  release -- not this one. This release *carries* the deprecation: the style parses, validates
  and runs as it always has, and the two defects it had been shipping are fixed rather than
  left standing for a surface on its way out. The earliest a removal can land is the release
  after the one that first puts this warning in users' hands.

  **Nothing changes today.** A build with `--features local-llm` still runs in-process models,
  every config still parses and validates, and no key changed spelling. What changed is that
  outrig now says the feature is going away, at three moments: `cargo build --features
  local-llm` emits a build warning, loading an in-process model prints a one-line warning per
  model, and the error a default build already raised for `style = "mistralrs"` now names the
  deprecation and the replacement alongside the `--features local-llm` flag that still works.

  Run local models under an OpenAI-compatible server -- [Ollama](https://ollama.com), vLLM, or
  `llama.cpp`'s server -- and point a `style = "openai"` provider at its `localhost`
  `base-url`. Migration, before and after, is in
  [In-process LLMs](../../doc/concepts/in-process-llm.md).

  Two consequences worth stating plainly. Running a local model well is a problem with good
  dedicated tools, and outrig was a worse place to solve it: the backend roughly triples the
  dependency count, and outrig's config duplicated weight, quantization, and device knobs those
  servers expose better. And outrig is **giving up a property it claimed** -- that a question
  never crosses a process boundary. A localhost server does serialize payloads over a socket.
  That trade is deliberate, and the documentation retracts the argument it made against
  localhost servers rather than quietly dropping it; the unimplemented egress filter, tool-use
  filter, and prompt-injection scanner that were the stated consumers of the property will have
  to answer the question on their own terms.

### Removed

- **The `OUTRIG_BOOTSTRAP` environment variable**, along with the `podman exec` user-bootstrap
  fallback it selected. The runtime user is written into the container from the host, and that
  is now the only path.
- **The `user_bootstrap_package_missing` warning** from `validate_dockerfile` (`mcp self` and
  the `rig` self tool). It advised installing `passwd`/`shadow` on hosts that would fall back
  to `useradd`/`groupadd`; with no fallback there is no such host. The tool no longer runs a
  `podman info` probe to decide, so it answers without touching podman at all.

- **Breaking (library):** this crate's internals are no longer a public path. `builtin_tool`,
  `cli`, `config_init`, `error`, `hf`, `image_setup`, `init`, `llm`, `mcp_self`, `repl`,
  `rig_tool`, `session`, `session_tool`, and `subagent` were `pub` only so the integration tests
  in `tests/` could reach them, which the crate's own module doc has always said. They are now
  crate-private unless the `internal-test-api` feature is on, which only those tests enable.
  `CliError`, `LlmResolveError`, `ResolvedProvider`, `ResolvedAgent`, `MistralrsWeights`,
  `RigAgent`, and `resolve_agent_with_overrides` were the growth points this closes -- all of
  them gained fields, variants, or parameters during `0.2`.

  The published surface is now `outrig_cli::run() -> ExitCode`, which runs the CLI and returns
  its exit code. Nothing else is covered by SemVer. Depend on the `outrig` crate for a supported
  Rust API; **the `outrig` command-line interface itself is unaffected.**

### Fixed

- **The session watcher reaped sidecars by container name.** When the primary container dies
  out from under outrig, the watcher removes that session's sidecars -- and it asked podman for
  them by name. Sidecars die at the same moment the primary does and `--rm` frees a name on the
  spot, so a removal resolving a name a moment later could reach whatever had taken it. Each
  sidecar now carries an `org.outrig.instance` label, unique per container and per session, and
  the reap selects on that. A failed reap is now reported rather than discarded, the way
  `outrig clean` reports one. (`outrig clean`'s own stray sweep still removes by name, and
  still has to: a stray's session is over, so nothing is left holding the label it was
  started with.)

- **`outrig clean` reported removing stray containers it had not removed.** The sweep coalesces
  its removals into one `podman rm -f <name>...`, and printed `removed container <name>` for
  every name in the batch on the strength of that one exit status. podman exits zero having
  skipped a container another process is tearing down at the same moment, so the line was a
  claim about the batch rather than about the container -- and it was self-perpetuating: the
  container clean said it had removed joined the next sweep's batch and was skipped again.

  Each name's outcome now comes from re-reading the container list. A name the batch skipped is
  retried on its own, which removes it; one that is still there afterwards is reported as
  `could not remove container <name>` and makes `outrig clean` exit **1** instead of 0, so a
  script checking the status no longer reads a partial sweep as a complete one. The batch
  remains the common path and the per-name retry runs only when something survived it.

- **`--network audit` recorded nothing and `--network filter` refused nothing** on hosts with
  nft 1.0.9, which is what Ubuntu 24.04 ships. The interceptor's redirect table was created
  empty, so no container traffic ever reached it. Fixed in `outrig`; see that crate's
  changelog for what the script does now and why no unit test could have caught it.

- **`style = "mistralrs"` rejects unknown keys**, closing the one hole in this schema's
  "unknown keys are an error" rule. The provider was a serde *unit* variant, so
  `deny_unknown_fields` had no field set to check against, and every key written on one of
  these blocks -- `retry-budget-secs`, `request-timeout-secs`, `base-url`, an outright typo --
  parsed clean and was discarded. Aiming a remote-only setting at an in-process provider
  through TOML was the last surviving way to have one vanish rather than be refused; the
  Rust-side spelling had already gone.

  The provider table is unchanged -- `style` and nothing else -- so the only configs this stops
  loading are ones that were carrying a key outrig never read.

- **A relative `[models.<name>].model-path` is opened against the repo root**, which is the base
  it has always been *validated* against. The resolver copied the row's text into the runtime
  weights unjoined and the mistralrs loader opened it relative to the process's working
  directory, so a config that validated clean named a different file -- usually no file --
  whenever `outrig` was started from anywhere but the repo root. Both halves now go through one
  library call, `Model::resolved_model_path`.

  This stays the one path in the schema that is repo-relative rather than relative to the file
  that declared it. A *global* `[models.<name>]` with a relative `model-path` therefore follows
  whichever repo is current; give that one an absolute path.

- **A hostname allow-rule grants only against a bound destination.** Under `mode = "filter"`
  with `default = "deny"`, a rule like `allow = ["allowed.example:443"]` was matched against
  the name the *client itself* announced in `Host:` or SNI as well as against the address the
  connection was going to. The client half is attacker-controlled, so a container could open a
  connection to an unrelated address, claim to be `allowed.example`, and be bridged to it --
  the enforcement half of the interceptor did not hold the property it advertised, and
  `SECURITY.md` names failure to enforce a host:port policy as in scope.

  A name now authorizes a destination only when this attachment's own DNS listener validated
  it for that address. What the client claims is kept apart from what was resolved and is
  consulted on the deny list only: a claim may cost a client its own connection and may never
  buy it one. The `ip` and `cidr` allow forms lost the same client-asserted disjunct.

  The bindings behind it are per attachment rather than session-global, so one container's
  lookup no longer grants another authority over an address; they are keyed address to name to
  expiry, so shared hosting keeps every name rather than the latest lookup erasing the rest;
  their TTLs come from the answering record, clamped to between 30 seconds and an hour; and
  the table is capped. DNS answers are validated before they bind -- right resolver,
  transaction id, question, and QR bit -- addresses are attributed through the CNAME chain to
  the name that was queried, and decoded names are checked, since a wire label may legally
  contain a `.` and an unchecked one could forge a parent domain.

- **A provider response outrig cannot use ends the turn, not the session.** A reply that
  decodes into nothing outrig can turn into a turn is retried a couple of times and then
  reported as `the model returned a response outrig could not use (<detail>)`, leaving the
  history untouched so the prompt can simply be sent again. This is a different class from the
  rate-limited or unreachable provider the retry stack already handled -- that one is a
  transport or status failure, this one is a well-formed response with an unusable body
  -- and it used to take the whole session down.

- **A turn that produced only reasoning is reported rather than swallowed.** A response with
  no text and no tool calls, but with reasoning content, reached the user as pure silence: the
  agent layer concatenates the final turn's text parts, which is the empty string here, and
  the REPL printed nothing at all. A minute of billed generation was indistinguishable from
  outrig ignoring the prompt. The structured turn is now salvaged and shown, the streaming
  path is covered too, and a turn that genuinely finished with nothing to display says so and
  names the finish reason and any ceiling in force.

- **A session record outlives the schema that wrote it.** `outrig ls` failed outright with
  `missing field image_config_name` against any session started before that field was renamed,
  and `clean`, `logs`, and `discard` broke identically because all four read the same listing.
  The field is now optional and accepts its old name, so an old record keeps its real value
  rather than degrading to a blank. A `session.json` that still cannot be read costs its own
  row and a report instead of the whole command: `ls` distinguishes an empty session root from
  one where nothing parsed and exits 0 either way, `clean` counts an unparsable entry as
  surviving rather than treating its container as record-less and force-removing it, and
  naming one broken session by id still fails, because that is a request for that session.

- **A subagent launched under another model takes that model's `max-tokens`.** The launching
  agent's ceiling used to be copied onto the subagent whatever model it named, so an expensive
  parent delegating to a cheap one sent a ceiling the cheap model refuses -- and every turn of
  that subagent failed, not merely long ones. That is precisely the case the `model` argument
  exists for: cheap models generally serve fewer output tokens than expensive ones. The ceiling
  now follows the model, resolved the way any other turn resolves it
  (`[agents.<name>].max-tokens`, else the named model's `[models.<name>].max-tokens`).
  `temperature`, `tool-call-max`, and `tool-result-max` still come from the launching agent --
  those are agent knobs, where an output-token ceiling is a model one. A launch that names no
  model is unchanged.

  Relatedly, the stderr line that names the ceiling when `set_result` is cut off used to print
  the *parent's* number for such a subagent, which was never the one in effect.

- **A configured `max-tokens` above an Anthropic model's published ceiling is lowered to it**
  rather than sent and refused. The Messages API rejects an over-ceiling request outright, so the
  whole turn failed where a capped one runs. Only applies to identifiers outrig recognizes a
  ceiling for; for the rest the configured value still travels whole, since there is nothing to
  cap against and guessing would be wrong for exactly the newest models.

- **A rate-limited or unreachable provider ends the turn, not the session.** A transient
  failure that outlived the retries used to escape the REPL loop, tear down the containers,
  and exit `1` with the conversation lost -- so a two-second rate limit cost the whole session.
  It now prints what happened and returns you to the `>` prompt with history unchanged, to
  resend when the window clears. Genuine faults -- a bad API key, a malformed config -- still
  exit `1`. A subagent round that hits this still reaches its parent as a failed round.

- **`outrig mcp` and `outrig mcp self` list their tools again to a client speaking protocol
  revision `2026-07-28`.** That revision requires `ttlMs` and `cacheScope` on every list
  result, both servers omitted them, and the client's answer to a malformed `tools/list` is to
  retry a few times and then give up -- so the session came up clean, reported the servers
  ready, and offered the agent nothing to call. A client on an older revision was unaffected,
  which is what made this look like a client bug rather than outrig's.

  The two servers scope their answers differently. `mcp self` is `public`: its tool set is
  compiled into the binary, identical for every user of a given outrig, and any intermediary
  may cache it. `mcp` is `private`: the list is the union of one session's backing servers.
  Both declare a five-minute window. Both also advertise an explicit list of revisions they
  serve, so a client asking for something newer is negotiated down instead of being answered in
  a revision outrig has never spoken.

  Both also stop answering `resources/list`, `prompts/list`, and `resources/templates/list`,
  which they never had a resource or a prompt to put in. rmcp replied to all three from default
  handler bodies with an empty success -- a surface neither server advertises, malformed on
  `2026-07-28` in the same way `tools/list` was. They now return method-not-found.

## [0.1.0](https://github.com/tgockel/outrig/releases/tag/outrig-cli-v0.1.0) - 2026-06-26

### Added

- **Standalone image projects** -- `outrig image init` scaffolds a project whose build
  output is a reusable container image, `outrig image build` builds and verifies it, and
  `outrig image inspect` (local and `--remote`) reads its OCI labels without pulling
  layers or starting a container. Image config is stamped into labels rather than a baked
  file, and repo-local build images are named after their `[images.<name>]` config.
- **Design helper** -- `outrig design prompt --standalone` generates an AI-assisted prompt
  for standalone image projects.
- **Session management** -- `outrig clean` bulk-removes stopped session records with a
  30-day default retention window, preview/confirm, and `--older-than` / `--yes` controls.
- **Run ergonomics** -- a `run --model <NAME>` override, config-less `run`/`mcp` backed by
  the global config plus an explicit `--image`, and raw local image refs accepted for
  `--image`.
- **Resilient LLM calls** -- transient endpoint failures (request timeouts, dropped
  connections, HTTP 408/429/5xx) are retried with bounded exponential backoff, and the
  OpenAI provider's `request-timeout-secs` is now actually applied.
- Renamed the in-process model build feature from `mistralrs` to `local-llm`.

### Fixed

- Show the final image tag after a build.
- Render prompt help links as the published mdBook URLs while keeping local `doc/...`
  metadata for sync checks.
- Skip agent/model/provider validation during image builds.
- Repair e2e suite rot.

### Changed

- Split the workspace into the `outrig` library and the `outrig-cli` binary; each crate
  ships its own crates.io README.
- Renamed the `[container]` config table to `[image]` and the tool-call/result `cap`
  limits to `max`.
