//! SessionStore: layout under `<session-root>/<sid>/`.
//!
//! Two materialization modes, both produce the same on-disk record:
//!
//! - **Auto** (`create(.., None, ..)`): `<root>/<sid>/session.json` plus
//!   `<root>/<sid>/logs/`.
//! - **Explicit** (`create(.., Some(dir), ..)`): writes to `<dir>/session.json`
//!   directly and creates `<root>/<sid>` as a symlink to `<dir>` so
//!   `outrig ls` finds it uniformly.

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::error::{OutrigError, Result};

const SESSION_JSON: &str = "session.json";

/// Stable, sortable session id. Format: `yyyymmddTHHMMSS-rrrr` (UTC, four
/// hex digits of randomness). Lexicographic order matches chronological
/// order modulo collisions, so listings can sort by string.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SessionId(pub String);

impl SessionId {
    pub fn new() -> Self {
        use jiff::Zoned;
        use rand::RngCore;

        let ts = Zoned::now()
            .with_time_zone(jiff::tz::TimeZone::UTC)
            .strftime("%Y%m%dT%H%M%S");
        let mut buf = [0u8; 2];
        rand::thread_rng().fill_bytes(&mut buf);
        Self(format!("{ts}-{:02x}{:02x}", buf[0], buf[1]))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for SessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<String> for SessionId {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl Default for SessionId {
    fn default() -> Self {
        Self::new()
    }
}

/// On-disk session record. Mirrors `doc/usage/sessions.md`'s `session.json`.
/// `link_target` is in-memory only -- populated by `list`/`get_by_id` when
/// the entry under `<root>/<sid>` is a symlink, never written to disk.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub struct Session {
    pub id: SessionId,
    #[serde(with = "iso_systime")]
    pub started_at: SystemTime,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "iso_systime_opt"
    )]
    pub ended_at: Option<SystemTime>,
    pub container_name: String,
    pub image_tag: String,
    pub container_config_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_name: Option<String>,
    pub working_dir: PathBuf,
    pub session_dir: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(skip)]
    pub link_target: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct SessionStore {
    root: PathBuf,
}

impl SessionStore {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    /// Path of the `<root>/<sid>` entry -- the symlink for explicitly-pathed
    /// sessions, the directory itself for auto-allocated ones. Exposed so CLI
    /// commands can name the path in user-facing messages without the store's
    /// `root` field leaking out.
    pub fn symlink_path(&self, id: &SessionId) -> PathBuf {
        self.root.join(&id.0)
    }

    /// Materialize a session on disk. `create` overwrites `session.session_dir`
    /// with the resolved path so the caller doesn't need to compute it twice
    /// and the in-memory `Session` matches what got persisted. Returns the
    /// same path for convenience.
    pub fn create(
        &self,
        sid: &SessionId,
        explicit_dir: Option<&Path>,
        session: &mut Session,
    ) -> Result<PathBuf> {
        fs::create_dir_all(&self.root)?;

        let actual_dir = match explicit_dir {
            Some(dir) => {
                let canon = fs::canonicalize(dir)?;
                let target_json = canon.join(SESSION_JSON);
                if target_json.exists() {
                    return Err(OutrigError::Configuration(format!(
                        "--session-dir {} already contains session.json",
                        canon.display()
                    )));
                }
                session.session_dir = canon.clone();
                write_session_json_atomic(&canon, session)?;
                let link = self.root.join(&sid.0);
                std::os::unix::fs::symlink(&canon, &link)?;
                canon
            }
            None => {
                let dir = self.root.join(&sid.0);
                session.session_dir = dir.clone();
                fs::create_dir_all(&dir)?;
                write_session_json_atomic(&dir, session)?;
                dir
            }
        };

        Ok(actual_dir)
    }

    /// Update `ended_at` + `exit_code` on a session that's already on disk.
    /// Read-modify-write; atomic on POSIX via `tempfile::persist`. Single-writer
    /// assumption -- if a future task adds heartbeating, revisit for lost-update.
    pub fn finalize(&self, id: &SessionId, ended_at: SystemTime, exit_code: i32) -> Result<()> {
        let (dir, mut session) = self.get_by_id(id)?;
        session.ended_at = Some(ended_at);
        session.exit_code = Some(exit_code);
        session.link_target = None; // never persist
        write_session_json_atomic(&dir, &session)
    }

    /// Newest-first. Symlinked entries get `link_target = Some(read_link_result)`.
    /// Entries without a `session.json` are skipped silently (foreign content under
    /// the root shouldn't crash listings).
    pub fn list(&self) -> Result<Vec<Session>> {
        let mut out = Vec::new();
        let entries = match fs::read_dir(&self.root) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
            Err(e) => return Err(e.into()),
        };
        for entry in entries {
            let entry = entry?;
            let Some((resolved, link_target)) = resolve_entry(&entry.path())? else {
                continue;
            };
            match read_session_json(&resolved.join(SESSION_JSON)) {
                Ok(mut session) => {
                    session.link_target = link_target;
                    out.push(session);
                }
                Err(OutrigError::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e),
            }
        }
        out.sort_by(|a, b| b.id.0.cmp(&a.id.0));
        Ok(out)
    }

    /// Resolve `<root>/<id>` (symlink-aware), return `(actual_dir, Session)`
    /// with `link_target` set when applicable.
    pub fn get_by_id(&self, id: &SessionId) -> Result<(PathBuf, Session)> {
        let entry = self.root.join(&id.0);
        let (resolved, link_target) = resolve_entry(&entry)?
            .ok_or_else(|| OutrigError::Configuration(format!("session {id} not found")))?;
        let mut session = read_session_json(&resolved.join(SESSION_JSON))?;
        session.link_target = link_target;
        Ok((resolved, session))
    }

    /// Read `<dir>/session.json`. `link_target` left `None`; the caller already
    /// has the directory in hand, so symlink-status is its concern.
    pub fn get_by_path(&self, dir: &Path) -> Result<Session> {
        read_session_json(&dir.join(SESSION_JSON))
    }

    /// Auto session: remove the dir.
    /// Symlinked: remove the link target's contents *and* the symlink.
    pub fn remove_by_id(&self, id: &SessionId) -> Result<()> {
        let entry = self.root.join(&id.0);
        let meta = fs::symlink_metadata(&entry)?;
        if meta.file_type().is_symlink() {
            let target = fs::read_link(&entry)?;
            if target.exists() {
                fs::remove_dir_all(&target)?;
            }
            fs::remove_file(&entry)?;
        } else {
            fs::remove_dir_all(&entry)?;
        }
        Ok(())
    }

    /// `rm -rf <dir>`, then sweep `<root>` for any symlink whose target was
    /// `<dir>` and remove it too. O(n) over root entries; fine for v0.
    pub fn remove_by_path(&self, dir: &Path) -> Result<()> {
        // Resolve once *before* removing so we can match dangling symlinks
        // even after the target is gone.
        let canon_dir = fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
        if dir.exists() {
            fs::remove_dir_all(dir)?;
        }
        let entries = match fs::read_dir(&self.root) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e.into()),
        };
        for entry in entries {
            let entry = entry?;
            let path = entry.path();
            let Ok(meta) = fs::symlink_metadata(&path) else {
                continue;
            };
            if !meta.file_type().is_symlink() {
                continue;
            }
            let Ok(target) = fs::read_link(&path) else {
                continue;
            };
            if target == canon_dir {
                fs::remove_file(&path)?;
            }
        }
        Ok(())
    }
}

/// Resolve a path under the session root. `Ok(None)` when the entry is
/// neither a symlink nor a directory (foreign content -- skipped by callers).
/// For a symlink, returns `(target, Some(target))`; for a dir,
/// `(path, None)`.
fn resolve_entry(path: &Path) -> Result<Option<(PathBuf, Option<PathBuf>)>> {
    let meta = match fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    if meta.file_type().is_symlink() {
        let tgt = fs::read_link(path)?;
        Ok(Some((tgt.clone(), Some(tgt))))
    } else if meta.is_dir() {
        Ok(Some((path.to_path_buf(), None)))
    } else {
        Ok(None)
    }
}

/// Cascade: CLI flag > config's `session-root` > caller-supplied default.
/// The default itself is computed once at the call site (see
/// `repo::default_session_root`); passing it in keeps this function pure
/// and unit-testable without env-var manipulation.
pub fn resolve_session_root(flag: Option<&Path>, cfg: &Config, default: &Path) -> PathBuf {
    if let Some(p) = flag {
        return p.to_path_buf();
    }
    if let Some(p) = cfg.session_root.as_deref() {
        return p.to_path_buf();
    }
    default.to_path_buf()
}

/// Same cascade as [`resolve_session_root`], but suitable for read-only session
/// commands (`ls`, `logs`, `discard`) that may run outside any repo. Skips the
/// validation+merge that [`Config::load`] performs (we only need the
/// `session-root` key) and degrades silently when the repo or global config
/// file is missing -- the user might have neither and just want the XDG
/// default.
pub fn resolve_session_root_for_cli(
    flag: Option<&Path>,
    repo_cfg_override: Option<&Path>,
    global_cfg_path: &Path,
    cwd: &Path,
) -> Result<PathBuf> {
    if let Some(p) = flag {
        return Ok(p.to_path_buf());
    }
    let repo_cfg_path = match crate::repo::resolve_repo_config(repo_cfg_override, cwd) {
        Ok(p) => Some(p),
        Err(OutrigError::NoRepoConfig) => None,
        Err(e) => return Err(e),
    };
    if let Some(p) = repo_cfg_path
        && let Some(root) = read_session_root(&p)?
    {
        return Ok(root);
    }
    if let Some(root) = read_session_root(global_cfg_path)? {
        return Ok(root);
    }
    Ok(crate::repo::default_session_root())
}

fn read_session_root(path: &Path) -> Result<Option<PathBuf>> {
    let text = match fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let cfg = Config::load_from_str(&text)?;
    Ok(cfg.session_root)
}

/// `2026-05-01 14:19:07` (UTC). Display-only; the on-disk format is
/// ISO 8601 via [`iso_systime`]. Returns `?` if the timestamp can't be
/// converted (only happens for SystemTimes outside jiff's representable
/// range -- effectively never in practice).
pub fn format_started_at(t: SystemTime) -> String {
    match jiff::Timestamp::try_from(t) {
        Ok(ts) => ts.strftime("%Y-%m-%d %H:%M:%S").to_string(),
        Err(_) => "?".to_string(),
    }
}

/// `Mm SSs` for sub-hour, `Hh MMm` for an hour or more. Matches the
/// example in `doc/usage/sessions.md` (`0m44s`, `2m18s`, `6m02s`,
/// `1h05m`).
pub fn format_duration(d: Duration) -> String {
    let total = d.as_secs();
    if total < 3600 {
        let m = total / 60;
        let s = total % 60;
        format!("{m}m{s:02}s")
    } else {
        let h = total / 3600;
        let m = (total % 3600) / 60;
        format!("{h}h{m:02}m")
    }
}

fn read_session_json(path: &Path) -> Result<Session> {
    let bytes = fs::read(path)?;
    serde_json::from_slice::<Session>(&bytes).map_err(|e| {
        OutrigError::Configuration(format!("session.json at {}: {}", path.display(), e))
    })
}

/// Atomic write: temp file in the same dir, fsync, rename. Skipping the
/// parent-dir fsync is a deliberate v0 punt; outrig is a developer tool, not
/// a database.
fn write_session_json_atomic(dir: &Path, session: &Session) -> Result<()> {
    let payload = serde_json::to_vec_pretty(session)
        .map_err(|e| OutrigError::Configuration(format!("encoding session.json: {e}")))?;
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    tmp.as_file_mut().write_all(&payload)?;
    tmp.as_file_mut().sync_all()?;
    tmp.persist(dir.join(SESSION_JSON))?;
    Ok(())
}

mod iso_systime {
    use std::time::SystemTime;

    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn to_iso(t: SystemTime) -> Result<String, jiff::Error> {
        Ok(jiff::Timestamp::try_from(t)?.to_string())
    }

    pub fn from_iso(s: &str) -> Result<SystemTime, jiff::Error> {
        Ok(SystemTime::from(s.parse::<jiff::Timestamp>()?))
    }

    pub fn serialize<S: Serializer>(t: &SystemTime, s: S) -> Result<S::Ok, S::Error> {
        to_iso(*t).map_err(serde::ser::Error::custom)?.serialize(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<SystemTime, D::Error> {
        from_iso(&String::deserialize(d)?).map_err(serde::de::Error::custom)
    }
}

mod iso_systime_opt {
    use std::time::SystemTime;

    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S: Serializer>(t: &Option<SystemTime>, s: S) -> Result<S::Ok, S::Error> {
        let v = match t {
            Some(t) => Some(super::iso_systime::to_iso(*t).map_err(serde::ser::Error::custom)?),
            None => None,
        };
        v.serialize(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<SystemTime>, D::Error> {
        match Option::<String>::deserialize(d)? {
            Some(s) => super::iso_systime::from_iso(&s)
                .map(Some)
                .map_err(serde::de::Error::custom),
            None => Ok(None),
        }
    }
}
