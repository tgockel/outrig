# 0008 -- Container lifecycle

## Goal

Start, stop, and reliably clean up a podman container. Make cleanup bulletproof: normal exit,
SIGINT, panic, and last-resort `Drop` all leave no leaked containers.

## Deliverables

- `src/container.rs::Container { name: String, image_tag: ImageTag, host_workspace: PathBuf,
  container_workspace: PathBuf, uid: u32, gid: u32, ... }`.
- `Container::start(image: &ImageTag, host_ws: &Path, ws_container: &Path) -> Result<Self>`:
  - Generate a session id (`<UTC>-<rand4hex>`); container name = `outrig-<sid>`.
  - Detect SELinux enforcing (`getenforce` shells out, falls back to reading
    `/sys/fs/selinux/enforce`); if enforcing, append `,Z` to the volume mount option.
  - Run:
    ```
    podman run -d --rm \
      --name outrig-<sid> \
      -v <host_ws>:<ws>:rw[,Z] \
      --userns=keep-id \
      -w <ws> \
      --security-opt=no-new-privileges \
      --pull=never \
      <image> sleep infinity
    ```
  - Capture host UID/GID via `nix::unistd::getuid()`/`getgid()` and store on the struct.
- `Container::stop(self, grace: Duration) -> Result<()>` running `podman stop -t <s> <name>`
  then `podman rm -f <name>` (the `--rm` flag covers the second on success but be defensive).
- `Drop` impl: spawn a detached `podman rm -f <name>` (best-effort, never blocks). The Drop
  must not require a tokio runtime context.
- A `panic::set_hook` registered in the binary's main that runs synchronous `podman rm -f` on
  any tracked container name; container names are in a `Mutex<HashSet<String>>` global.
- A SIGINT cancellation token plumbed into the calling code (run subcommand): caller selects
  on `signal::ctrl_c()` vs the REPL future; on fire, drops the REPL future and awaits
  `stop()`.
- `tests/container_lifecycle.rs` (`#[cfg(feature = "e2e")]`):
  - Start `alpine:latest`, `podman ps` shows the container, stop, `podman ps -a --filter
    name=outrig-` shows nothing.
  - Drop a `Container` without calling `stop` -> the detached `rm -f` cleans up; verify with a
    short sleep then `podman ps -a`.

## Acceptance

- `cargo test --features e2e container_lifecycle` passes.
- Hitting Ctrl-C during a manual `outrig run` (once 0014 lands) leaves no orphan containers.
- `podman ps -a --filter name=outrig-` is empty after every test invocation.

## Dependencies

- 0006-process-wrappers

## Notes

- Don't drop into `--cap-drop=ALL` in v0; the docs explain why.
- The bootstrap step (matching user UID/GID inside the container) is task 0009. This task only
  starts and stops the container; the container is not yet useful for MCP.
