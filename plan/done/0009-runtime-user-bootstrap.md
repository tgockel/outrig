# 0009 -- Runtime user-mapping bootstrap

## Goal

After the container starts (as in-container root), materialize a matching user/group inside it
so subsequent `podman exec --user=$(id -u):$(id -g)` invocations don't trip on missing
`/etc/passwd` entries. Files written under `/workspace` then appear with the host user's
UID/GID. Pattern follows
[tgockel/dev-env](https://github.com/tgockel/dev-env/blob/trunk/dev-env).

## Deliverables

- `Container::bootstrap_user(&self) -> Result<()>` doing this script-equivalent inside the
  container, all execs as in-container root (no `--user` flag):
  ```sh
  gid=<host_gid>
  gname=<host_group_name>
  if ! getent group "$gid" >/dev/null; then
      until groupadd --gid "$gid" "$gname"; do gname="${gname}_"; done
  fi
  uid=<host_uid>
  uname=<host_user_name>
  if ! getent passwd "$uid" >/dev/null; then
      until useradd -u "$uid" -g "$gid" "$uname"; do uname="${uname}_"; done
  fi
  mkdir -p "/home/$uname"
  chown "$uname:$gname" "/home/$uname"
  ```
  Implementation: a sequence of `podman exec` calls with `run_capture`, scripted via
  `process::run_capture`. Append `_` to the name on `useradd`/`groupadd` failure (max ~10
  retries before giving up with a clear error).
- Capture the resolved in-container user name + group name on the `Container` struct so
  subsequent `exec_stdio` calls can reference them (e.g. for setting `$HOME` env).
- `Container::exec_stdio(cmd: &[String], env: &BTreeMap<String,String>) -> Result<Child>`
  shells out:
  ```
  podman exec -i \
    --user=<uid>:<gid> \
    --env HOME=/home/<uname> \
    --env KEY=val ... \
    <name> <cmd...>
  ```
  Returns a `tokio::process::Child` with piped stdio (delegates to `process::spawn_stdio`).
- `tests/runtime_user.rs` (`#[cfg(feature = "e2e")]`):
  - On a fresh `alpine:latest` container (with `shadow` package): `bootstrap_user()` succeeds;
    `exec_stdio("id")` returns the host UID/GID.
  - Write a file at `/workspace/test.txt` from inside; verify host bind-mount file ownership
    matches `id -u`/`id -g`.
  - Pre-populate the container with a conflicting user at the host UID; verify the bootstrap
    re-uses the existing entry without erroring.

## Acceptance

- `cargo test --features e2e runtime_user` passes.
- After bootstrap, files written through `exec_stdio` to `/workspace` show host UID/GID on the
  bind-mount.
- Drop the `> TODO: Incomplete` marker at the top of `doc/concepts/workspace.md`.

## Dependencies

- 0008-container-lifecycle

## Notes

- Don't add passwordless sudo -- not needed for MCP.
- `nix::unistd::User::from_uid(uid)?.name` is a clean way to get the host user name.
- The `_` collision-retry should handle the case where someone has an ancient container image
  with a `tgockel:1000` already; we'd re-use it rather than fail.

## Decisions

- **`bootstrap_user(&mut self)` rather than `&self`.** The deliverable bullet showed `&self`,
  but the same bullet also requires capturing the resolved user/group names back onto the
  `Container` struct, which mandates mutation. Picked `&mut self` over interior-mutability
  fields (RwLock / Mutex) because bootstrap runs once during startup and there's no need to
  carry the synchronization cost forward.
- **`--user=0:0` on bootstrap exec calls, not "no `--user` flag".** The deliverable's pseudocode
  said the bootstrap runs as in-container root via an unscoped `podman exec`. Under
  `--userns=keep-id` (set by 0008's `Container::start`), an unscoped exec defaults to the
  *host* user instead, which can't `apk add shadow` / `useradd` / `groupadd` / write to
  `/home`. Forcing `--user=0:0` explicitly puts us at in-container UID 0, which is what the
  spec actually wanted. The doc comment on `bootstrap_user` and the helper `podman_exec_root`
  both spell out the trap.
- **Probe-or-plant structure for the `bootstrap_reuses_existing_entry` test.** Modern podman
  (5.x) auto-injects the host UID/GID into `/etc/passwd` and `/etc/group` when
  `--userns=keep-id` is in play, so `groupadd --gid <host_gid>` fails with "GID already
  exists" before the test can manually plant a conflicting entry. The test now probes
  `getent` first; if something's already there (auto-injection) it asserts bootstrap reuses
  *that* name; otherwise it plants `preexisting_grp` / `preexisting_usr` and asserts those.
  Either path exercises the same reuse code in `bootstrap_user`.
- **`OutrigError::BootstrapExhausted { kind: &'static str }` rather than overloading
  `Process`.** A "ran out of `_` retries" error has no exit code, no argv to surface, and no
  meaningful stderr tail -- shoehorning it into the `Process` variant would mean synthesizing
  empty fields. A small dedicated variant is clearer at call sites and at error-formatting
  time.
- **Probe + retry factored into `probe_entry` / `create_with_retry` helpers.** The original
  `resolve_or_create_group` and `resolve_or_create_user` were 25-line near-duplicates of each
  other. After review-pass dedup, each is a 12-line wrapper: probe via `probe_entry`, then
  call `create_with_retry` with a closure that builds the right podman argv. The closure
  captures the differences (tool name, `--gid` vs `-u`/`-g`) rather than a parameterized
  arg list -- closures keep the call sites readable.
- **`exec_stdio` panics if `bootstrap_user` was not called first.** The `expect("bootstrap_user
  must be called before exec_stdio")` is a programmer-error guard, not a user-facing error
  path. Considered a `BootstrappedContainer` newtype that statically encodes the precondition
  (consume `Container` -> return `BootstrappedContainer`); rejected as over-engineering for
  v0 -- the misuse is internal-only and an `expect` makes the violation immediately obvious.
