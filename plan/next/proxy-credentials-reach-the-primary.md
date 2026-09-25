# The host's proxy variables, and any password in them, reach the primary

## Context

Found by the review of `0003-05`. podman's `--http-proxy` defaults to true, so `podman run`
copies the host's `http_proxy`, `https_proxy`, `ftp_proxy`, `no_proxy` and their uppercase forms
into the container's environment, and every `podman exec` -- the Python interpreter included --
inherits them. OutRig never passes `--http-proxy` (`crates/outrig/src/container/mod.rs`). A proxy
URL of the form `http://user:password@proxy:3128` is therefore readable from the agent's Python,
by `os.environ` or `/proc/self/environ`.

Checked on this host: with `HTTPS_PROXY=http://user:secret@proxy.invalid:3128` exported, `podman
run --rm alpine env` prints it back.

`0003-05` narrowed its guarantee to credentials from OutRig's own config and named this as an
exception (`doc/reference/cli.md`, `run-new`). It predates the phase: `outrig run`'s containers
have always carried the same variables.

## Shape

Two directions, and they pull against each other:

- **`--http-proxy=false`, with the proxy passed in deliberately.** It closes the leak, but an
  agent behind a proxy then loses its network in subprocesses and `urllib` unless OutRig forwards
  the proxy some other way -- without the userinfo, or through the network interceptor, which
  already sits on the container's egress.
- **Strip userinfo only.** Keeps the proxy's host and port and drops `user:password@`. That works
  for a proxy that authenticates some other way, and breaks one that needs the password.

Whichever is chosen belongs in `security.md`'s boundary rather than in one command, since `run`
has the same exposure.

## Acceptance

- A live test exports a proxy URL with a password and shows what the container's environment
  holds afterward.
- `doc/reference/cli.md` states what reaches the container, without an exception it cannot back.
