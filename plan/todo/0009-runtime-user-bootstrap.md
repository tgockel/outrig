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
