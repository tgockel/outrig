# 0015 -- Session store

## Goal

Persist per-run session metadata + per-MCP stderr to disk under a session root. Support both
auto-allocated and explicit (user-supplied) session directories, with a symlink in the root for
explicit ones so `outrig ls` keeps working uniformly.

## Deliverables

- `src/session.rs::SessionId(pub String)` with `SessionId::new() -> Self` producing
  `<UTC-yyyymmddTHHMMSS>-<rand4hex>`.
- `src/session.rs::Session` mirroring `doc/usage/sessions.md`'s `session.json` shape:
  ```rust
  pub struct Session {
      pub id: SessionId,
      pub started_at: SystemTime,
      pub ended_at: Option<SystemTime>,
      pub container_name: String,
      pub image_tag: String,
      pub container_config_name: String,
      pub agent_name: String,
      pub working_dir: PathBuf,        // host workspace root
      pub session_dir: PathBuf,        // actual on-disk path (== link target if symlinked)
      pub exit_code: Option<i32>,
  }
  ```
- `src/session.rs::SessionStore { root: PathBuf }` with:
  - `pub fn new(root: PathBuf) -> Self`.
  - `pub fn create(&self, sid: &SessionId, explicit_dir: Option<&Path>, session: &Session) ->
    Result<PathBuf>` -- returns the actual session-content path:
    - If `explicit_dir` is `Some`: refuse if the path already contains a `session.json`. Else
      create the dir, write `session.json`, create `<root>/<sid> -> <explicit_dir>` symlink,
      return `explicit_dir`.
    - Else: create `<root>/<sid>/`, write `session.json`, return that path.
  - `pub fn finalize(&self, id: &SessionId, ended_at: SystemTime, exit_code: i32) ->
    Result<()>` -- read+update session.json atomically.
  - `pub fn list(&self) -> Result<Vec<Session>>` -- newest-first; follow symlinks; mark
    symlinked entries on the returned `Session` (add a `pub link_target: Option<PathBuf>`
    field).
  - `pub fn get_by_id(&self, id: &SessionId) -> Result<(PathBuf, Session)>`.
  - `pub fn get_by_path(&self, dir: &Path) -> Result<Session>`.
  - `pub fn remove_by_id(&self, id: &SessionId) -> Result<()>` -- resolves symlink, `rm -rf`
    target, removes the symlink in root if present.
  - `pub fn remove_by_path(&self, dir: &Path) -> Result<()>` -- `rm -rf` dir; if any symlink
    in root points at it, remove that too.
- session-root resolver: `fn resolve_session_root(flag: Option<&Path>, cfg: &Config,
  default: &Path) -> PathBuf` -- flag > cfg.session_root > default.
- Wire into `outrig run` (0014): pass `--session-dir` and resolved root to `SessionStore`,
  capture per-MCP stderr to `<session_dir>/logs/<server>.stderr` (already done in 0010, but
  the `log_dir` argument now points into the session dir).
- Atomic writes: `tempfile::NamedTempFile::persist` for session.json updates.
- `tests/session_store.rs` covering:
  - Auto path: create -> session.json exists at `<root>/<sid>/`.
  - Explicit path: create -> session.json at `<explicit>/`, symlink at `<root>/<sid>` points to
    it.
  - Explicit path with pre-existing session.json -> error.
  - List includes both auto and symlinked entries.
  - get_by_id / get_by_path round-trip.
  - remove_by_id removes target + symlink for symlinked entries; just the dir for auto.

## Acceptance

- `cargo test session_store` passes every case.
- After a normal `outrig run`, `<root>/<sid>/session.json` exists and parses;
  `<root>/<sid>/logs/<server>.stderr` is non-empty for any MCP that produced stderr.
- After `outrig run --session-dir /tmp/foo`, `/tmp/foo/session.json` exists and
  `<root>/<sid>` is a symlink to `/tmp/foo`.

## Dependencies

- 0005-config-merge-validate
- 0014-agent-loop

## Notes

- Use `std::os::unix::fs::symlink` for the symlink (fails on non-Unix; fine, we're Linux-only
  in v0).
- Don't follow symlinks recursively; one level of indirection is enough.
- Test cleanup: every test creates its own tempdir as the root.
