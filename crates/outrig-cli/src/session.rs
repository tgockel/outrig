//! SessionStore: layout under `<session-root>/<sid>/`.
//!
//! Two materialization modes, both produce the same on-disk record:
//!
//! - **Auto** (`create(.., None, ..)`): `<root>/<sid>/session.json` plus
//!   `<root>/<sid>/logs/`.
//! - **Explicit** (`create(.., Some(dir), ..)`): writes to `<dir>/session.json`
//!   directly and creates `<root>/<sid>` as a symlink to `<dir>` so
//!   `outrig ls` finds it uniformly. `<dir>` has to be empty or absent: from
//!   then on it is the session's.
//!
//! Either way the record is `session.json`, `logs/`, and -- for a session with
//! a `view = "primary"` sidecar -- `outrig-enter`. Removal deletes those three
//! names and nothing else, then the directory if that empties it.

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

use crate::paths::{
    default_session_root, load_global_or_empty, report_config_warnings, resolve_global_config,
};
use outrig::config::Config;
use crate::error::{CliError, Result};
use outrig::error::{IoPathExt, OutrigError};

const SESSION_JSON: &str = "session.json";
/// The session's log directory. Everything outrig logs for a session -- MCP
/// stderr, `network.jsonl`, transcripts -- goes under it.
pub(crate) const LOGS_DIR: &str = "logs";

/// Stable, sortable session id. Format: `yyyymmddTHHMMSS-rrrr` (UTC, four
/// hex digits of randomness). Lexicographic order matches chronological
/// order modulo collisions, so listings can sort by string.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SessionId(pub String);

impl SessionId {
    pub fn new() -> Self {
        use jiff::Zoned;
        use rand::Rng;

        let ts = Zoned::now()
            .with_time_zone(jiff::tz::TimeZone::UTC)
            .strftime("%Y%m%dT%H%M%S");
        let mut buf = [0u8; 2];
        rand::rng().fill_bytes(&mut buf);
        Self(format!("{ts}-{:02x}{:02x}", buf[0], buf[1]))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The start time `name` encodes, when it has the shape [`SessionId::new`]
    /// gives: `None` for anything else, which is how a directory that is not
    /// a session's is told from one whose record is lost.
    pub fn started_at(name: &str) -> Option<SystemTime> {
        let (ts, rand) = name.split_once('-')?;
        if rand.len() != 4 || !rand.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        let civil = jiff::civil::DateTime::strptime("%Y%m%dT%H%M%S", ts).ok()?;
        // `strptime` accepts short fields (`14190` for a second), so only the
        // exact text `new` writes counts as a session id.
        if civil.strftime("%Y%m%dT%H%M%S").to_string() != ts {
            return None;
        }
        let zoned = civil.to_zoned(jiff::tz::TimeZone::UTC).ok()?;
        Some(SystemTime::from(zoned.timestamp()))
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
///
/// These fields are *data outrig itself wrote*, not user-authored config, so
/// they can't be migrated by asking the user to edit a file: every record
/// already on disk has to keep loading. Renaming a field needs a
/// `#[serde(alias = "<old name>")]`; dropping one needs `Option` +
/// `#[serde(default)]`. Skipping that is what made `outrig ls` fail outright
/// on every pre-2026-06-01 session (see `image_config_name` below).
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
    /// Sidecar container names owned by this session, in start order. Empty
    /// (and absent from disk) for single-container sessions, so pre-sidecar
    /// records parse and sidecar-less records serialize unchanged.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sidecar_container_names: Vec<String>,
    pub image_tag: String,
    /// Absent on records written before the 2026-06-01 `container` -> `image`
    /// rename, which stored the same value under `container_config_name`; the
    /// alias recovers it, and any read-modify-write (`finalize`,
    /// `set_sidecar_containers`) rewrites the record under the current name.
    #[serde(
        default,
        alias = "container_config_name",
        skip_serializing_if = "Option::is_none"
    )]
    pub image_config_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_name: Option<String>,
    pub working_dir: PathBuf,
    pub session_dir: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(skip)]
    pub link_target: Option<PathBuf>,
}

/// A `<root>/<sid>` entry that exists but could not be read as a session --
/// a truncated write, or a record whose schema predates a field rename. Kept
/// out-of-band by [`SessionStore::list`] so one bad record can't take down a
/// whole listing, while still leaving the CLI something to report.
#[derive(Debug)]
pub struct SkippedSession {
    /// The `<sid>` directory name under the session root.
    pub entry: String,
    /// The directory holding the record -- the link's target for a link.
    pub dir: PathBuf,
    /// Whether `<root>/<entry>` is a symlink to `dir`.
    pub linked: bool,
    /// The `container_name` text found in the damaged record, when it got that
    /// far. An `outrig mcp --attach` session records the borrowed container's
    /// name, not `outrig-<sid>`, so only this says what to check for running.
    pub recorded_container: Option<String>,
    /// Why the record could not be read: a [`CliError::SessionRecord`] for a
    /// record that does not parse, an I/O error for one that could not be read.
    pub error: CliError,
}

impl SkippedSession {
    pub fn id(&self) -> SessionId {
        SessionId(self.entry.clone())
    }

    /// The container the session would have run in. The record that would name
    /// it is unreadable, so this is the name `outrig run` gives every session
    /// it creates.
    pub fn container_name(&self) -> String {
        self.recorded_container
            .clone()
            .unwrap_or_else(|| format!("outrig-{}", self.entry))
    }

    /// Whether [`SkippedSession::container_name`] is what the record said,
    /// rather than the name `outrig run` would have given. When it is not, an
    /// attach session's borrowed container cannot be ruled out.
    pub fn container_known(&self) -> bool {
        self.recorded_container.is_some()
    }

    /// Why the record is unreadable, without the `session record <path> is
    /// unreadable:` framing that the path-naming errors carry.
    pub fn reason(&self) -> String {
        match &self.error {
            CliError::SessionRecord { source, .. } => source.to_string(),
            other => other.to_string(),
        }
    }
}

/// A `<root>/<sid>` symlink, left by `--session-dir`, whose target directory
/// is gone. The link is all that remains of the session: there is no record
/// to read, so it is kept out of [`SessionListing::sessions`] and
/// [`SessionListing::skipped`] alike.
#[derive(Debug, Clone)]
pub struct DanglingLink {
    /// The `<sid>` name under the session root.
    pub entry: String,
    /// The directory the link names, which no longer exists.
    pub target: PathBuf,
    /// When the link was last modified -- written once, at session start, so
    /// it stands in for the start time the missing record would have given.
    pub modified: SystemTime,
}

impl DanglingLink {
    pub fn id(&self) -> SessionId {
        SessionId(self.entry.clone())
    }

    /// The container the session would have run in. A dangling link has no
    /// record to name it, so this is the name `outrig run` gives every
    /// session it creates.
    pub fn container_name(&self) -> String {
        format!("outrig-{}", self.entry)
    }
}

/// The result of [`SessionStore::list`]: the records that parsed, plus the
/// entries that didn't.
#[derive(Debug, Default)]
pub struct SessionListing {
    pub sessions: Vec<Session>,
    pub skipped: Vec<SkippedSession>,
    pub dangling: Vec<DanglingLink>,
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
    ///
    /// An explicit directory is created if absent and refused unless empty, so
    /// that nothing in it predates the session. Writing the record claims the
    /// directory: of two launches given one at once, the second to write is
    /// refused.
    pub fn create(
        &self,
        sid: &SessionId,
        explicit_dir: Option<&Path>,
        session: &mut Session,
    ) -> Result<PathBuf> {
        // Before the root is created, so a root nested inside the explicit
        // directory is not taken for something already there. These checks are
        // the policy and the messages for a directory that is plainly taken;
        // the claim below holds when two launches pass them together.
        if let Some(dir) = explicit_dir {
            if dir.exists() && !dir.is_dir() {
                return Err(OutrigError::Configuration(format!(
                    "--session-dir {} exists and is not a directory",
                    dir.display()
                )).into());
            }
            fs::create_dir_all(dir).path_ctx("create directory", dir)?;
            if dir.join(SESSION_JSON).exists() {
                return Err(OutrigError::Configuration(format!(
                    "--session-dir {} holds an earlier session's record; discard that \
                     session first, or name a new or empty directory",
                    dir.display()
                )).into());
            }
            if let Some(first) = dir_names(dir)?.first() {
                return Err(OutrigError::Configuration(format!(
                    "--session-dir {} is not empty (it holds {first}); name a new or \
                     empty directory",
                    dir.display()
                )).into());
            }
        }
        fs::create_dir_all(&self.root).path_ctx("create directory", &self.root)?;

        let actual_dir = match explicit_dir {
            Some(dir) => {
                let canon = fs::canonicalize(dir).path_ctx("resolve", dir)?;
                session.session_dir = canon.clone();
                claim_session_json(&canon, session)?;
                let link = self.root.join(&sid.0);
                std::os::unix::fs::symlink(&canon, &link).path_ctx("symlink", &link)?;
                canon
            }
            None => {
                // Replacing, not claiming, until #382 makes the `create_dir`
                // of a fresh id the claim.
                let dir = self.root.join(&sid.0);
                session.session_dir = dir.clone();
                fs::create_dir_all(&dir).path_ctx("create directory", &dir)?;
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

    /// Record the session's sidecar container names. Separate from `create`
    /// because sidecars start after the session row exists; read-modify-write
    /// like [`SessionStore::finalize`].
    pub fn set_sidecar_containers(&self, id: &SessionId, names: &[String]) -> Result<()> {
        let (dir, mut session) = self.get_by_id(id)?;
        session.sidecar_container_names = names.to_vec();
        session.link_target = None; // never persist
        write_session_json_atomic(&dir, &session)
    }

    /// Newest-first. Symlinked entries get `link_target = Some(read_link_result)`.
    /// Entries without a `session.json` are skipped silently (foreign content under
    /// the root shouldn't crash listings), except a symlink whose target directory
    /// is gone, which is reported via [`SessionListing::dangling`]. Entries that have one but can't be parsed
    /// are skipped too, but reported via [`SessionListing::skipped`] -- a single
    /// corrupt or schema-drifted record shouldn't cost the user every other session.
    /// Faults reading the root itself are still hard errors.
    pub fn list(&self) -> Result<SessionListing> {
        let mut out = SessionListing::default();
        let entries = match fs::read_dir(&self.root) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
            Err(e) => return Err(e).path_ctx("read directory", &self.root).map_err(CliError::from),
        };
        for entry in entries {
            let entry = entry?;
            let Some((resolved, link_target)) = resolve_entry(&entry.path())? else {
                continue;
            };
            match read_session_json(&resolved.join(SESSION_JSON)) {
                Ok(mut session) => {
                    session.link_target = link_target;
                    out.sessions.push(session);
                }
                Err(CliError::Outrig(OutrigError::Path { source, .. }))
                    if source.kind() == std::io::ErrorKind::NotFound =>
                {
                    if link_target.is_some() && !resolved.exists() {
                        let modified = fs::symlink_metadata(entry.path())
                            .and_then(|m| m.modified())
                            .unwrap_or(SystemTime::UNIX_EPOCH);
                        out.dangling.push(DanglingLink {
                            entry: entry.file_name().to_string_lossy().into_owned(),
                            target: resolved,
                            modified,
                        });
                    }
                    continue;
                }
                Err(error) => out.skipped.push(SkippedSession {
                    entry: entry.file_name().to_string_lossy().into_owned(),
                    linked: link_target.is_some(),
                    recorded_container: fs::read(resolved.join(SESSION_JSON))
                        .ok()
                        .and_then(|bytes| salvage_container_name(&bytes)),
                    dir: resolved,
                    error,
                }),
            }
        }
        out.sessions.sort_by(|a, b| b.id.0.cmp(&a.id.0));
        out.skipped.sort_by(|a, b| b.entry.cmp(&a.entry));
        out.dangling.sort_by(|a, b| b.entry.cmp(&a.entry));
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

    /// Remove a session's record from its directory -- the link target for a
    /// symlinked session -- and the directory too if that empties it. A
    /// symlink goes either way. Returns what the directory still holds,
    /// sorted; empty when it is gone.
    pub fn remove_by_id(&self, id: &SessionId) -> Result<Vec<String>> {
        let entry = self.root.join(&id.0);
        let meta = fs::symlink_metadata(&entry).path_ctx("stat", &entry)?;
        if meta.file_type().is_symlink() {
            // Relative to the link's directory, as `resolve_entry` reads it.
            let target = self
                .root
                .join(fs::read_link(&entry).path_ctx("read symlink", &entry)?);
            let left = if target.exists() {
                remove_record(&target)?
            } else {
                Vec::new()
            };
            fs::remove_file(&entry).path_ctx("remove", &entry)?;
            Ok(left)
        } else {
            remove_record(&entry)
        }
    }

    /// Unlink a dangling `<root>/<entry>` and nothing else. The removal for a
    /// link that was dangling when it was selected: it never follows the
    /// link, so a session that recreated the directory since is untouched.
    /// Returns `false`, leaving the entry, when it is no longer a symlink to
    /// `target` or `target` exists again.
    pub fn unlink_dangling(&self, link: &DanglingLink) -> Result<bool> {
        let entry = self.root.join(&link.entry);
        let Ok(meta) = fs::symlink_metadata(&entry) else {
            return Ok(false);
        };
        if !meta.file_type().is_symlink() {
            return Ok(false);
        }
        let current = fs::read_link(&entry).path_ctx("read symlink", &entry)?;
        if self.root.join(current) != link.target || link.target.exists() {
            return Ok(false);
        }
        fs::remove_file(&entry).path_ctx("remove", &entry)?;
        Ok(true)
    }

    /// [`SessionStore::remove_by_id`] for a directory named outright, then
    /// sweep `<root>` for any symlink whose target was `<dir>` and remove it
    /// too. O(n) over root entries; fine for v0.
    pub fn remove_by_path(&self, dir: &Path) -> Result<Vec<String>> {
        // Resolve once *before* removing so we can match dangling symlinks
        // even after the target is gone. The removal works on this path too:
        // given a symlink, it is the directory the link names that holds the
        // record.
        let canon_dir = fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
        let left = if canon_dir.exists() {
            remove_record(&canon_dir)?
        } else {
            Vec::new()
        };
        let entries = match fs::read_dir(&self.root) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(left),
            Err(e) => return Err(e).path_ctx("read directory", &self.root).map_err(CliError::from),
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
            if self.root.join(target) == canon_dir {
                fs::remove_file(&path).path_ctx("remove", &path)?;
            }
        }
        Ok(left)
    }
}

/// Remove the record in `dir` -- `outrig-enter`, `logs/`, then `session.json`
/// -- and `dir` itself if that empties it. Nothing else is touched: a
/// directory given to `--session-dir` before 0.2.2 may hold the user's own
/// files, and under these names too: `logs` goes only as a real directory and
/// the other two only as anything else. `session.json` goes last, so a removal
/// that fails partway leaves a record that can still be found and removed
/// again.
///
/// Returns what `dir` still holds, sorted; empty when `dir` is gone.
fn remove_record(dir: &Path) -> Result<Vec<String>> {
    // `outrig-enter` is the launcher `outrig::container::enter::materialize`
    // writes, under that name.
    for name in ["outrig-enter", LOGS_DIR, SESSION_JSON] {
        let path = dir.join(name);
        // `logs/` goes only as a real directory, not a symlink to one; the
        // others only as a non-directory, a symlink unlinked, not followed.
        // A name of any other type stays, for the listing below to report.
        let removed = match fs::symlink_metadata(&path) {
            Ok(meta) if name == LOGS_DIR && meta.is_dir() => fs::remove_dir_all(&path),
            Ok(meta) if name != LOGS_DIR && !meta.is_dir() => fs::remove_file(&path),
            Ok(_) => Ok(()),
            Err(e) => Err(e),
        };
        if let Err(e) = removed
            && e.kind() != std::io::ErrorKind::NotFound
        {
            return Err(e).path_ctx("remove", &path).map_err(CliError::from);
        }
    }
    // `remove_dir` is the emptiness check: it fails rather than remove
    // anything still there, including a file that arrived just now.
    match fs::remove_dir(dir) {
        Ok(()) => Ok(Vec::new()),
        Err(e) => {
            let left = dir_names(dir)?;
            if left.is_empty() {
                Err(e).path_ctx("remove", dir).map_err(CliError::from)
            } else {
                Ok(left)
            }
        }
    }
}

/// The names in `dir`, sorted, so what a message names does not depend on
/// directory order.
fn dir_names(dir: &Path) -> Result<Vec<String>> {
    let mut names = Vec::new();
    for entry in fs::read_dir(dir).path_ctx("read directory", dir)? {
        let entry = entry.path_ctx("read directory", dir)?;
        names.push(entry.file_name().to_string_lossy().into_owned());
    }
    names.sort();
    Ok(names)
}

/// Resolve a path under the session root. `Ok(None)` when the entry is
/// neither a symlink nor a directory (foreign content -- skipped by callers).
/// For a symlink, returns `(target, Some(target))`; for a dir,
/// `(path, None)`.
fn resolve_entry(path: &Path) -> Result<Option<(PathBuf, Option<PathBuf>)>> {
    let meta = match fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).path_ctx("stat", path).map_err(CliError::from),
    };
    if meta.file_type().is_symlink() {
        let tgt = fs::read_link(path).path_ctx("read symlink", path)?;
        // A relative target is relative to the link's directory, not to the
        // cwd; `link_target` keeps the text as written.
        let resolved = path.parent().map_or_else(|| tgt.clone(), |p| p.join(&tgt));
        Ok(Some((resolved, Some(tgt))))
    } else if meta.is_dir() {
        Ok(Some((path.to_path_buf(), None)))
    } else {
        Ok(None)
    }
}

/// Cascade: CLI flag > config's `session-root` > caller-supplied default.
/// The default itself is computed once at the call site (see
/// `default_session_root`); passing it in keeps this function pure
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

/// Same cascade as [`resolve_session_root`], for the session commands (`ls`,
/// `logs`, `discard`, `clean`), which may run outside any repo. `session-root`
/// is global-only, so the global config is the only file consulted, and the
/// answer is the same from every directory. Read with
/// [`Config::load_global`], which resolves the key beside the file and skips
/// validation (only this key matters here); a missing default file, or no
/// default location to look in, falls through to the XDG default.
/// `global_override` is `--global-config` as given, resolved only without
/// `flag`: given the flag, the global config is neither read nor checked.
pub fn resolve_session_root_for_cli(
    flag: Option<&Path>,
    global_override: Option<&Path>,
) -> Result<PathBuf> {
    if let Some(p) = flag {
        return Ok(p.to_path_buf());
    }
    let global = load_global_or_empty(resolve_global_config(global_override)?.as_deref())?;
    report_config_warnings(&global);
    Ok(global.session_root.unwrap_or_else(default_session_root))
}

/// `2026-05-01 14:19:07` (UTC). Display-only; the on-disk format is
/// ISO 8601 via `iso_systime`. Returns `?` if the timestamp can't be
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

/// The string value of `"container_name"` in a record that no longer parses --
/// truncated after it, or broken somewhere else -- or `None` when the text
/// never gets that far or the value is not closed.
fn salvage_container_name(bytes: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(bytes);
    let rest = text.split_once("\"container_name\"")?.1;
    let rest = rest.trim_start().strip_prefix(':')?.trim_start();
    let body = rest.strip_prefix('"')?;
    let mut escaped = false;
    for (at, c) in body.char_indices() {
        match c {
            _ if escaped => escaped = false,
            '\\' => escaped = true,
            '"' => return serde_json::from_str(&rest[..at + 2]).ok(),
            _ => {}
        }
    }
    None
}

fn read_session_json(path: &Path) -> Result<Session> {
    let bytes = fs::read(path).path_ctx("read", path)?;
    serde_json::from_slice::<Session>(&bytes).map_err(|source| CliError::SessionRecord {
        path: path.to_path_buf(),
        source,
    })
}

/// The record written and fsynced to a temp file in `dir`, ready to be
/// renamed into place. Skipping the parent-dir fsync is a deliberate v0 punt;
/// outrig is a developer tool, not a database.
fn stage_session_json(dir: &Path, session: &Session) -> Result<tempfile::NamedTempFile> {
    let payload = serde_json::to_vec_pretty(session)
        .map_err(|e| OutrigError::Configuration(format!("encoding session.json: {e}")))?;
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    tmp.as_file_mut().write_all(&payload)?;
    tmp.as_file_mut().sync_all()?;
    Ok(tmp)
}

/// Atomic write: [`stage_session_json`], then rename over whatever
/// `session.json` is there.
fn write_session_json_atomic(dir: &Path, session: &Session) -> Result<()> {
    stage_session_json(dir, session)?
        .persist(dir.join(SESSION_JSON))
        .map_err(OutrigError::from)?;
    Ok(())
}

/// The first write of a record into an explicit directory, which is also the
/// claim on it: the rename fails rather than replace a `session.json` another
/// launch wrote first (#430). `create`'s checks have already turned away a
/// directory that is plainly taken, so a refusal here is two launches racing.
fn claim_session_json(dir: &Path, session: &Session) -> Result<()> {
    match stage_session_json(dir, session)?.persist_noclobber(dir.join(SESSION_JSON)) {
        Ok(_) => Ok(()),
        Err(e) if e.error.kind() == std::io::ErrorKind::AlreadyExists => {
            Err(OutrigError::Configuration(format!(
                "--session-dir {} was claimed by another session that started at the \
                 same moment; name a different directory",
                dir.display()
            ))
            .into())
        }
        Err(e) => Err(OutrigError::from(e).into()),
    }
}

pub(crate) mod iso_systime {
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
