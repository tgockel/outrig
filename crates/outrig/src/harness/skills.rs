//! Skills: know-how a session's agents can use, each a `SKILL.md` of instructions and, optionally,
//! a `skill.py` the agent imports as a module (`plan/phase/0003-python/skills.md`).
//!
//! Every skill comes from a [`SkillSource`], given to the session in precedence order: the CLI's
//! are the project's `.agents/skills` and then the user's `~/.agents/skills`, and an embedder may
//! add its own. The first source that holds a name serves it, and every later copy is reported as
//! shadowed.
//!
//! **Discovery runs no Python.** As the session starts, each skill's `SKILL.md` frontmatter is
//! read for its name and description, and its `skill.py`'s `# /// script` block (PEP 723) for
//! what it declares it needs, on the host and with nothing else read. A skill whose metadata is
//! wrong is left out and reported, and the result is the `skill.discovered` event.
//!
//! **A skill's files reach the container over the interpreter's pipe, as they are used.** No
//! skill directory is mounted. The interpreter asks for a skill's listing when its package is
//! imported, and for each file when it is imported or opened. A file a skill has changed since its
//! package was imported is refused as `skill-changed`, and so is what was read while the skill's
//! files changed, so one import never mixes two versions.
//!
//! Every source operation, discovery's and serving's alike, runs on a thread of its own in one of
//! [`SOURCE_OPERATIONS_MAX`] places a session has, and is given up on after [`SOURCE_TIMEOUT`];
//! the runtime never waits for the thread, so a source that never returns holds no exit up.
//!
//! A diagnostic's message escapes every control character but a line break or a tab: what it
//! quotes of a skill, such as the line a parser stopped at, is shown on terminals.
//!
//! The sources read the host with the user's authority, which is what makes one rule necessary:
//! a [`DirectorySource`] serves nothing that is not inside a skill's own directory, and follows
//! no link to get there.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::File;
use std::io::{self, Read as _};
use std::os::fd::{AsFd, AsRawFd as _, OwnedFd};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex, OnceLock, PoisonError};
use std::time::Duration;

use base64::Engine as _;
use futures_util::future::join_all;
use nix::dir::Dir;
use nix::errno::Errno;
use nix::fcntl::{AtFlags, OFlag};
use nix::sys::stat::{FileStat, Mode, SFlag};
use regex::Regex;
use saphyr_parser::{Event, Parser, ScalarStyle};
use serde_json::{Value, json};
use tokio::sync::Semaphore;

use super::event::{DiscoveredSkill, Payload, SkillDiagnostic};
use crate::events::Events;
use crate::python::host::Interpreter;

/// Where a session's skills come from: the skills it holds, the files of each, and what one file
/// holds. Given to [`SessionBuilder::skill_source`](super::SessionBuilder::skill_source).
///
/// Each method is called on a thread where blocking is fine, and each call must answer within 30
/// seconds, or the session gives up on it: discovery leaves the skill out, and an import in
/// progress fails with `source-error`.
///
/// A skill is named by its directory, in the Agent Skills layout: `SKILL.md` and, optionally,
/// `skill.py` beside whatever else it holds. A path is relative to the skill's directory, its
/// components separated by `/`.
pub trait SkillSource: Send + Sync {
    /// The names of the skills this source holds. The session checks each against its
    /// `SKILL.md`.
    fn skills(&self) -> io::Result<Vec<String>>;

    /// Every file of `skill`, each with a version that changes whenever the file does: the
    /// session decides that a skill changed by comparing them, without reading the files.
    fn files(&self, skill: &str) -> io::Result<Vec<SkillFile>>;

    /// What `path`, one of [`SkillSource::files`]'s, holds.
    fn read(&self, skill: &str, path: &str) -> io::Result<Vec<u8>>;
}

/// One file of a skill, as its source lists it: its path, and a version that changes whenever
/// the file does -- a revision, a hash, or the size, modification time and inode
/// [`DirectorySource`] uses.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct SkillFile {
    pub path: String,
    pub version: String,
}

impl SkillFile {
    pub fn new(path: impl Into<String>, version: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            version: version.into(),
        }
    }
}

/// How long one source operation may take, waiting for its place included: listing a source,
/// reading one skill's metadata, or answering one request.
pub(crate) const SOURCE_TIMEOUT: Duration = Duration::from_secs(30);

/// How many source operations a session runs at once. One that never returns keeps its place,
/// so a source that hangs slows the session's skills rather than taking more of its threads.
const SOURCE_OPERATIONS_MAX: usize = 8;

/// The most entries a skill's directory may hold, its directories included, and the most a
/// source's directory may hold.
const ENTRIES_MAX: usize = 1024;

/// How many directories deep a skill's files may be.
const DEPTH_MAX: usize = 16;

/// The largest file a skill may have served.
const FILE_MAX: u64 = 8 << 20;

/// The most YAML collections a `SKILL.md`'s frontmatter may open one inside another.
const NESTING_MAX: usize = 32;

/// The longest `description` the Agent Skills standard allows, in characters.
const DESCRIPTION_MAX: usize = 1024;

/// Skills held in directories: a root holding one directory per skill.
///
/// Nothing below the root is reached through a link: a skill's directory that is a symbolic link
/// is refused, a link inside one is listed and refused when it is read, and so is anything that is
/// not a regular file. A path naming `.` or `..` is refused, names beginning `.` and
/// `__pycache__` are not served, and a skill may hold at most 1,024 entries, 16 directories deep,
/// each file at most 8 MiB. The root is opened when it is first listed and held open, so
/// replacing it later changes nothing; a root that does not exist holds no skills.
pub struct DirectorySource {
    root: PathBuf,
    /// The directory the root must resolve inside, when there is one.
    base: Option<PathBuf>,
    /// The root, as its first use found it: open, absent, or refused.
    opened: OnceLock<io::Result<Option<OwnedFd>>>,
}

impl DirectorySource {
    /// The skills in `root`, which is followed wherever it leads: a path the operator chose, as
    /// the user's `~/.agents/skills` is.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            base: None,
            opened: OnceLock::new(),
        }
    }

    /// The skills in `base`'s `path`, which must resolve inside `base`: a repository's
    /// `.agents/skills`, which may be a link to another directory of the repository but never to
    /// one outside it.
    pub fn within(base: impl Into<PathBuf>, path: impl AsRef<Path>) -> Self {
        let base = base.into();
        Self {
            root: base.join(path),
            base: Some(base),
            opened: OnceLock::new(),
        }
    }

    /// The root, opened the first time it is asked for; `None` when it does not exist.
    fn root(&self) -> io::Result<Option<&OwnedFd>> {
        match self
            .opened
            .get_or_init(|| open_root(&self.root, self.base.as_deref()))
        {
            Ok(root) => Ok(root.as_ref()),
            Err(e) => Err(io::Error::new(e.kind(), e.to_string())),
        }
    }

    /// `skill`'s directory, opened beneath the root.
    fn skill_dir(&self, skill: &str) -> io::Result<OwnedFd> {
        let root = self.root()?.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("{} does not exist", self.root.display()),
            )
        })?;
        open_at(root, component(skill)?)
    }
}

impl SkillSource for DirectorySource {
    fn skills(&self) -> io::Result<Vec<String>> {
        let Some(root) = self.root()? else {
            return Ok(Vec::new());
        };
        Ok(entries(root, &mut 0)?
            .into_iter()
            .filter(|(_, stat)| matches!(file_type(stat), SFlag::S_IFDIR | SFlag::S_IFLNK))
            .map(|(name, _)| name)
            .collect())
    }

    fn files(&self, skill: &str) -> io::Result<Vec<SkillFile>> {
        let mut files = Vec::new();
        walk(&self.skill_dir(skill)?, "", 0, &mut files, &mut 0)?;
        files.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(files)
    }

    fn read(&self, skill: &str, path: &str) -> io::Result<Vec<u8>> {
        let parts: Vec<&str> = path.split('/').map(component).collect::<io::Result<_>>()?;
        let mut file = self.skill_dir(skill)?;
        for part in parts {
            file = open_at(&file, part)?;
        }
        if file_type(&nix::sys::stat::fstat(&file)?) != SFlag::S_IFREG {
            return Err(io::Error::other(format!(
                "{path:?} is not a regular file, which is all this source serves"
            )));
        }
        let mut data = Vec::new();
        File::from(file).take(FILE_MAX + 1).read_to_end(&mut data)?;
        if data.len() as u64 > FILE_MAX {
            return Err(too_large(path));
        }
        Ok(data)
    }
}

/// The directory `root` names, or `None` when there is none; inside `base`, when given one.
fn open_root(root: &Path, base: Option<&Path>) -> io::Result<Option<OwnedFd>> {
    let flags = OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC;
    let fd = match nix::fcntl::open(root, flags, Mode::empty()) {
        Ok(fd) => fd,
        Err(Errno::ENOENT) => return Ok(None),
        Err(e) => {
            let why = format!("{} could not be opened: {e}", root.display());
            return Err(io::Error::other(why));
        }
    };
    if let Some(base) = base {
        // Where the directory that was opened is, rather than where the path led a moment
        // earlier: nothing can swap it between the check and the use.
        let unread = |e: io::Error| {
            io::Error::other(format!(
                "where {} resolves could not be read: {e}",
                root.display()
            ))
        };
        let real =
            std::fs::read_link(format!("/proc/self/fd/{}", fd.as_raw_fd())).map_err(unread)?;
        let base = std::fs::canonicalize(base).map_err(unread)?;
        if !real.starts_with(&base) {
            return Err(io::Error::other(format!(
                "{} resolves to {}, outside {}",
                root.display(),
                real.display(),
                base.display()
            )));
        }
    }
    Ok(Some(fd))
}

/// `name`, if it can be one component of a path: not empty, `.`, `..`, hidden or a bytecode
/// cache, and holding no `/` or NUL.
fn component(name: &str) -> io::Result<&str> {
    if name.is_empty() || name == "." || name == ".." || name.contains(['/', '\0']) || skipped(name)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{name:?} cannot name a skill or one of its files"),
        ));
    }
    Ok(name)
}

/// Whether a directory entry is left out of what a source serves: hidden ones, such as a
/// checkout's `.git`, and Python's bytecode caches, which a run of the skill on the host writes.
fn skipped(name: &str) -> bool {
    name.starts_with('.') || name == "__pycache__"
}

/// `name` beneath `dir`, opened without following a link, blocking on a FIFO, or taking a
/// terminal. Opened one component at a time, a link fails as `ELOOP`, and is said to be one.
fn open_at(dir: &impl AsFd, name: &str) -> io::Result<OwnedFd> {
    let flags = OFlag::O_RDONLY
        | OFlag::O_NOFOLLOW
        | OFlag::O_NONBLOCK
        | OFlag::O_NOCTTY
        | OFlag::O_CLOEXEC;
    nix::fcntl::openat(dir, name, flags, Mode::empty()).map_err(|e| match e {
        Errno::ELOOP => io::Error::other(format!(
            "{name:?} is a symbolic link, which this source does not follow"
        )),
        e => io::Error::other(format!("{name:?} could not be opened: {e}")),
    })
}

fn too_large(path: &str) -> io::Error {
    io::Error::other(format!(
        "{path:?} is larger than the {} MiB a skill's file may be",
        FILE_MAX >> 20
    ))
}

fn file_type(stat: &FileStat) -> SFlag {
    SFlag::from_bits_truncate(stat.st_mode) & SFlag::S_IFMT
}

/// The entries of the directory `dir` a source may serve, in name order, each as it is -- a link
/// as a link -- read through a description of the directory of their own. Each counts toward
/// `count`, which may not pass [`ENTRIES_MAX`].
fn entries(dir: &impl AsFd, count: &mut usize) -> io::Result<Vec<(String, FileStat)>> {
    let flags = OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC;
    let mut listing = Dir::openat(dir, ".", flags, Mode::empty())?;
    let mut found = Vec::new();
    for entry in listing.iter() {
        let entry = entry?;
        // A name that is not UTF-8 cannot be asked for, so it is never served.
        let Ok(name) = entry.file_name().to_str() else {
            continue;
        };
        if name == "." || name == ".." || skipped(name) {
            continue;
        }
        *count += 1;
        if *count > ENTRIES_MAX {
            return Err(io::Error::other(format!(
                "it holds more than the {ENTRIES_MAX} entries this source serves"
            )));
        }
        match nix::sys::stat::fstatat(dir, name, AtFlags::AT_SYMLINK_NOFOLLOW) {
            Ok(stat) => found.push((name.to_string(), stat)),
            // Gone since it was listed.
            Err(Errno::ENOENT) => {}
            Err(e) => return Err(e.into()),
        }
    }
    found.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(found)
}

/// Every file beneath `dir`, whose path is `prefix`, `depth` directories below the skill's.
fn walk(
    dir: &OwnedFd,
    prefix: &str,
    depth: usize,
    files: &mut Vec<SkillFile>,
    count: &mut usize,
) -> io::Result<()> {
    for (name, stat) in entries(dir, count)? {
        let path = format!("{prefix}{name}");
        if file_type(&stat) != SFlag::S_IFDIR {
            files.push(SkillFile::new(path, version(&stat)));
            continue;
        }
        if depth == DEPTH_MAX {
            return Err(io::Error::other(format!(
                "{path:?} is deeper than the {DEPTH_MAX} directories this source serves"
            )));
        }
        walk(
            &open_at(dir, &name)?,
            &format!("{path}/"),
            depth + 1,
            files,
            count,
        )?;
    }
    Ok(())
}

/// A file's version: its size, modification time and inode, which a write changes -- the inode
/// whenever the file is replaced rather than rewritten.
fn version(stat: &FileStat) -> String {
    format!(
        "{}:{}.{:09}:{}",
        stat.st_size, stat.st_mtime, stat.st_mtime_nsec, stat.st_ino
    )
}

// ---------------------------------------------------------------------------- discovery

/// Why a skill cannot be served, or a request about one answered: a code from `skills.md`'s
/// list, and what happened.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Problem {
    code: &'static str,
    message: String,
}

impl Problem {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    fn invalid(message: impl Into<String>) -> Self {
        Self::new("metadata-invalid", message)
    }

    fn source(e: impl std::fmt::Display) -> Self {
        Self::new("source-error", e.to_string())
    }
}

/// What a skill's files said when they were last read: their digest, and what its `skill.py`
/// declares.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Meta {
    digest: String,
    declared: Declared,
}

/// What a `skill.py`'s `# /// script` block declares: what the skill needs, which the
/// interpreter checks as its package is imported, and what `outrig.skills.invoke` reads of
/// `[tool.outrig.skill]`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Declared {
    requires_python: Option<String>,
    dependencies: Vec<String>,
    /// `entry`, the entry function's name, when the block names one.
    entry: Option<String>,
    /// `bind`: each entry parameter a binding fills, to the binding's name.
    bind: BTreeMap<String, String>,
    /// `[tool.outrig.skill.parameters]`: each entry parameter's help.
    parameters: BTreeMap<String, String>,
}

/// What reading one skill's metadata found.
#[derive(Debug)]
struct Found {
    meta: Meta,
    description: String,
    executable: bool,
    warnings: Vec<String>,
}

/// A skill the session serves.
struct Served {
    name: String,
    /// The name of the source serving it, and the source.
    source: String,
    from: Arc<dyn SkillSource>,
    description: String,
    executable: bool,
    meta: Mutex<Meta>,
}

/// The session's skills, in precedence order, and the places its source operations take.
pub(crate) struct Catalog {
    /// Whether the session was given any source.
    given: bool,
    skills: Vec<Served>,
    /// How long one source operation may take.
    bound: Duration,
    gate: Arc<Semaphore>,
}

/// What the main agent's preamble says of one skill.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SkillBrief {
    pub(crate) name: String,
    /// Its description, on one line.
    pub(crate) description: String,
    /// The package it imports as, when it has code.
    pub(crate) package: Option<String>,
}

/// Whether `name` follows the Agent Skills standard's rule: 1 to 64 lowercase ASCII letters,
/// digits and hyphens, with no hyphen first, last, or beside another.
fn is_skill_name(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !name.starts_with('-')
        && !name.ends_with('-')
        && !name.contains("--")
}

/// Check the sources given to a session: each needs a name, and no two the same.
pub(crate) fn check_sources(sources: &[(String, Arc<dyn SkillSource>)]) -> Result<(), String> {
    let mut seen = HashSet::new();
    for (name, _) in sources {
        if name.is_empty() {
            return Err("a skill source needs a name".to_string());
        }
        if !seen.insert(name.as_str()) {
            return Err(format!("two skill sources are named {name:?}"));
        }
    }
    Ok(())
}

/// Find the skills `sources` serve, in precedence order, reading each one's metadata and nothing
/// else, each source operation bounded by `bound`; publish what was found as `skill.discovered`
/// on `events`, when there is any source at all.
///
/// Every source is listed at once, and then every skill read at once, within the session's
/// places: a source that never answers costs the start its bound once, not once a skill.
pub(crate) async fn discover(
    sources: Vec<(String, Arc<dyn SkillSource>)>,
    events: &Events,
    bound: Duration,
) -> Arc<Catalog> {
    let gate = Arc::new(Semaphore::new(SOURCE_OPERATIONS_MAX));
    let listings = join_all(sources.iter().map(|(_, source)| {
        let source = Arc::clone(source);
        bounded(&gate, bound, move || {
            source.skills().map_err(Problem::source)
        })
    }))
    .await;
    let mut diagnostics = Vec::new();
    let mut taken: HashMap<String, &str> = HashMap::new();
    let mut wanted = Vec::new();
    for ((source, from), listing) in sources.iter().zip(listings) {
        let names = match listing {
            Ok(names) => names,
            Err(problem) => {
                let message = format!(
                    "the {source:?} skill source could not be listed: {}",
                    problem.message
                );
                diagnostics.push(SkillDiagnostic::new(None, source, problem.code, message));
                continue;
            }
        };
        for name in names {
            if let Some(winner) = taken.get(&name) {
                let message = format!(
                    "skill {name:?} from the {source:?} source is shadowed: the {winner:?} source \
                     holds one of that name, which is the one served"
                );
                diagnostics.push(SkillDiagnostic::new(
                    Some(&name),
                    source,
                    "shadowed",
                    message,
                ));
                continue;
            }
            taken.insert(name.clone(), source);
            if is_skill_name(&name) {
                wanted.push((source, from, name));
            } else {
                let problem = Problem::invalid(
                    "its directory's name is not a skill name: 1 to 64 lowercase letters, digits \
                     and hyphens, with no hyphen first, last or beside another",
                );
                diagnostics.push(left_out(&name, source, &problem));
            }
        }
    }
    let read = join_all(wanted.iter().map(|(_, from, name)| {
        let (from, name) = (Arc::clone(from), name.clone());
        bounded(&gate, bound, move || {
            let (files, digest) = listing(&*from, &name)?;
            read_skill(&*from, &name, &files, digest)
        })
    }))
    .await;
    let mut skills = Vec::new();
    for ((source, from, name), found) in wanted.into_iter().zip(read) {
        match found {
            Ok(found) => {
                for warning in &found.warnings {
                    let message = format!("skill {name:?} from the {source:?} source: {warning}");
                    let diagnostic =
                        SkillDiagnostic::new(Some(&name), source, "unknown-key", message);
                    diagnostics.push(diagnostic);
                }
                skills.push(Served {
                    name,
                    source: source.clone(),
                    from: Arc::clone(from),
                    description: found.description,
                    executable: found.executable,
                    meta: Mutex::new(found.meta),
                });
            }
            Err(problem) => diagnostics.push(left_out(&name, source, &problem)),
        }
    }
    let given = !sources.is_empty();
    if given {
        let discovered = skills
            .iter()
            .map(|served| DiscoveredSkill {
                name: served.name.clone(),
                source: served.source.clone(),
                description: served.description.clone(),
                digest: lock(&served.meta).digest.clone(),
                executable: served.executable,
            })
            .collect();
        events.emit(Payload::SkillDiscovered {
            skills: discovered,
            diagnostics,
        });
    }
    Arc::new(Catalog {
        given,
        skills,
        bound,
        gate,
    })
}

/// That skill `name` from `source` is left out, for `problem`.
fn left_out(name: &str, source: &str, problem: &Problem) -> SkillDiagnostic {
    let message = format!(
        "skill {name:?} from the {source:?} source is left out ({}): {}",
        problem.code, problem.message
    );
    SkillDiagnostic::new(Some(name), source, problem.code, message)
}

/// `work` on a thread of its own, once one of the session's places for a source operation is
/// free, given up on as `source-error` after `bound`, the wait for the place included. The place
/// goes with the work, so a source that never returns keeps it; and the thread is not the
/// runtime's, so such a source holds up neither the runtime's shutdown nor the process's exit.
async fn bounded<T: Send + 'static>(
    gate: &Arc<Semaphore>,
    bound: Duration,
    work: impl FnOnce() -> Result<T, Problem> + Send + 'static,
) -> Result<T, Problem> {
    let gate = Arc::clone(gate);
    let run = async move {
        let place = gate
            .acquire_owned()
            .await
            .expect("the gate is never closed");
        let (done, answer) = tokio::sync::oneshot::channel();
        std::thread::Builder::new()
            .name("skill-source".to_string())
            .spawn(move || {
                let _place = place;
                let _ = done.send(work());
            })
            .map_err(|e| Problem::source(format!("no thread could be started to read it: {e}")))?;
        answer
            .await
            .unwrap_or_else(|_| Err(Problem::source("reading it failed: its source panicked")))
    };
    tokio::time::timeout(bound, run).await.unwrap_or_else(|_| {
        let seconds = bound.as_secs_f64();
        Err(Problem::source(format!(
            "its source did not answer within {seconds} s"
        )))
    })
}

/// `skill`'s files, as `source` lists them, each path checked, and their digest.
fn listing(source: &dyn SkillSource, skill: &str) -> Result<(Vec<SkillFile>, String), Problem> {
    let mut files = source.files(skill).map_err(Problem::source)?;
    if files.len() > ENTRIES_MAX {
        return Err(Problem::source(format!(
            "it lists more than the {ENTRIES_MAX} files a skill may hold"
        )));
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));
    for (i, file) in files.iter().enumerate() {
        if file.path.split('/').any(|part| component(part).is_err()) {
            return Err(Problem::source(format!(
                "it lists {:?}, which cannot name a skill's file",
                file.path
            )));
        }
        if i > 0 && files[i - 1].path == file.path {
            return Err(Problem::source(format!("it lists {:?} twice", file.path)));
        }
    }
    let digest = digest(&files);
    Ok((files, digest))
}

/// Read `skill`'s metadata from `source` -- its `SKILL.md` and its `skill.py`, and nothing else
/// -- its files being `files`, whose digest is `digest`.
fn read_skill(
    source: &dyn SkillSource,
    skill: &str,
    files: &[SkillFile],
    digest: String,
) -> Result<Found, Problem> {
    let listed = |path: &str| files.iter().any(|file| file.path == path);
    if !listed("SKILL.md") {
        return Err(Problem::invalid("it has no SKILL.md"));
    }
    let instructions = text(source, skill, "SKILL.md")?;
    let Some((front, _)) = split(&instructions) else {
        return Err(Problem::invalid(
            "its SKILL.md does not begin with frontmatter between `---` lines",
        ));
    };
    let (name, description) = frontmatter(front).map_err(Problem::invalid)?;
    match name {
        Some(name) if name == skill => {}
        Some(name) => {
            return Err(Problem::invalid(format!(
                "its SKILL.md's name, {name:?}, is not its directory's"
            )));
        }
        None => return Err(Problem::invalid("its SKILL.md's frontmatter has no name")),
    }
    let description = description
        .ok_or_else(|| Problem::invalid("its SKILL.md's frontmatter has no description"))?
        .trim()
        .to_string();
    let length = description.chars().count();
    if !(1..=DESCRIPTION_MAX).contains(&length) {
        return Err(Problem::invalid(format!(
            "its SKILL.md's description is {length} characters; the standard allows 1 to \
             {DESCRIPTION_MAX}"
        )));
    }
    let executable = listed("skill.py");
    let mut warnings = Vec::new();
    let declared = if executable {
        script(&text(source, skill, "skill.py")?, &mut warnings)?
    } else {
        Declared::default()
    };
    Ok(Found {
        meta: Meta { digest, declared },
        description,
        executable,
        warnings,
    })
}

/// `path` of `skill`, as text with any byte-order mark gone and every line ending a newline.
fn text(source: &dyn SkillSource, skill: &str, path: &str) -> Result<String, Problem> {
    let data = read_checked(source, skill, path)?;
    let text = String::from_utf8(data)
        .map_err(|_| Problem::invalid(format!("its {path} is not UTF-8")))?;
    let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
    Ok(text.replace("\r\n", "\n"))
}

/// What `path` of `skill` holds, refused past [`FILE_MAX`] whichever source read it.
fn read_checked(source: &dyn SkillSource, skill: &str, path: &str) -> Result<Vec<u8>, Problem> {
    let data = source.read(skill, path).map_err(Problem::source)?;
    if data.len() as u64 > FILE_MAX {
        return Err(Problem::source(too_large(path)));
    }
    Ok(data)
}

/// The digest of a skill's listing: every path and version, in path order.
fn digest(files: &[SkillFile]) -> String {
    let mut hasher = blake3::Hasher::new();
    for file in files {
        for part in [&file.path, &file.version] {
            hasher.update(&(part.len() as u64).to_le_bytes());
            hasher.update(part.as_bytes());
        }
    }
    hasher.finalize().to_hex().to_string()
}

/// `text`'s frontmatter and body: the lines between an opening `---`, its first line, and the
/// next `---` line, and what follows that.
fn split(text: &str) -> Option<(&str, &str)> {
    let mut lines = text.split_inclusive('\n');
    if lines.next()?.trim_end() != "---" {
        return None;
    }
    let start = text.find('\n')? + 1;
    let mut at = start;
    for line in lines {
        if line.trim_end() == "---" {
            return Some((&text[start..at], &text[at + line.len()..]));
        }
        at += line.len();
    }
    None
}

/// The `name` and `description` frontmatter gives, read from its YAML as events: a value that
/// is plain `~`, `null` or empty is no value. Refused: YAML that does not parse, more than one
/// document, a top level that is not a mapping, a key that is not a scalar or appears twice, an
/// anchor or an alias anywhere, collections nested past [`NESTING_MAX`], and a `name` or
/// `description` that is not a scalar. Everything else is read and ignored.
fn frontmatter(yaml: &str) -> Result<(Option<String>, Option<String>), String> {
    let mut name = None;
    let mut description = None;
    let mut keys = HashSet::new();
    // Collections open, the top-level mapping counting one.
    let mut depth = 0;
    let mut documents = 0;
    // At depth 1: the key whose value comes next, when a key has just been read.
    let mut key: Option<String> = None;
    for event in Parser::new_from_str(yaml) {
        let (event, span) = event.map_err(|e| format!("its frontmatter is not valid YAML: {e}"))?;
        let line = span.start.line();
        match event {
            Event::Alias(_) => {
                return Err(format!(
                    "its frontmatter uses an alias, at line {line}, which OutRig refuses"
                ));
            }
            Event::Scalar(_, _, anchor, _)
            | Event::SequenceStart(anchor, _)
            | Event::MappingStart(anchor, _)
                if anchor != 0 =>
            {
                return Err(format!(
                    "its frontmatter uses an anchor, at line {line}, which OutRig refuses"
                ));
            }
            Event::DocumentStart(_) => {
                documents += 1;
                if documents > 1 {
                    return Err("its frontmatter holds more than one YAML document".into());
                }
            }
            Event::Scalar(value, style, _, _) => match (depth, key.take()) {
                (0, _) => return Err("its frontmatter is not a mapping".into()),
                (1, None) => {
                    if !keys.insert(value.to_string()) {
                        return Err(format!("its frontmatter has the key {value:?} twice"));
                    }
                    key = Some(value.into_owned());
                }
                (1, Some(key)) => {
                    let null = style == ScalarStyle::Plain
                        && matches!(&*value, "" | "~" | "null" | "Null" | "NULL");
                    let value = (!null).then(|| value.into_owned());
                    match key.as_str() {
                        "name" => name = value,
                        "description" => description = value,
                        _ => {}
                    }
                }
                _ => {}
            },
            Event::SequenceStart(..) | Event::MappingStart(..) => {
                let mapping = matches!(event, Event::MappingStart(..));
                match (depth, key.take()) {
                    (0, _) if !mapping => return Err("its frontmatter is not a mapping".into()),
                    (1, None) => {
                        return Err(format!(
                            "its frontmatter has a key that is not a scalar, at line {line}"
                        ));
                    }
                    (1, Some(key)) if key == "name" || key == "description" => {
                        return Err(format!("its frontmatter's {key} is not a string"));
                    }
                    _ => {}
                }
                depth += 1;
                if depth > NESTING_MAX {
                    return Err(format!(
                        "its frontmatter nests collections deeper than {NESTING_MAX}"
                    ));
                }
            }
            Event::SequenceEnd | Event::MappingEnd => depth -= 1,
            Event::StreamStart | Event::StreamEnd | Event::DocumentEnd | Event::Nothing => {}
        }
    }
    Ok((name, description))
}

/// PEP 723's expression for a metadata block, from the specification.
static BLOCK: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?m)^# /// (?P<type>[a-zA-Z0-9-]+)$\s(?P<content>(^#(| .*)$\s)+)^# ///$")
        .expect("PEP 723's expression compiles")
});

/// What `skill.py`'s `# /// script` block declares, `source` being its text, with a warning in
/// `warnings` for each key OutRig does not know in `[tool.outrig]`.
///
/// Only the file's leading lines are searched -- those before its first line that is neither
/// blank nor a comment -- so a block after the first statement, or inside a docstring, is never
/// read, and finding one parses no Python. A block opened and never closed is no block, and the
/// skill takes the defaults.
fn script(source: &str, warnings: &mut Vec<String>) -> Result<Declared, Problem> {
    let mut found = None;
    for block in BLOCK.captures_iter(leading(source)) {
        if &block["type"] != "script" {
            continue;
        }
        if found.is_some() {
            return Err(Problem::new(
                "duplicate-metadata-block",
                "its skill.py has more than one `# /// script` block",
            ));
        }
        let content: String = block["content"]
            .split_inclusive('\n')
            .map(|line| {
                line.strip_prefix("# ")
                    .or_else(|| line.strip_prefix('#'))
                    .unwrap_or(line)
            })
            .collect();
        found = Some(content);
    }
    let Some(content) = found else {
        return Ok(Declared::default());
    };
    let table: toml::Table = content.parse().map_err(|e| {
        Problem::invalid(format!(
            "its skill.py's `# /// script` block is not valid TOML: {e}"
        ))
    })?;
    needs(&table, warnings).map_err(Problem::invalid)
}

/// The lines of `source` before its first line that is neither blank nor a comment.
fn leading(source: &str) -> &str {
    let mut end = 0;
    for line in source.split_inclusive('\n') {
        let line_start = line.trim_start();
        if !(line_start.is_empty() || line_start.starts_with('#')) {
            break;
        }
        end += line.len();
    }
    &source[..end]
}

/// What a `# /// script` block's table declares: `requires-python` and `dependencies`, its two
/// keys from the specification, and the OutRig keys of `[tool.outrig.skill]`.
fn needs(table: &toml::Table, warnings: &mut Vec<String>) -> Result<Declared, String> {
    let requires_python = match table.get("requires-python") {
        None => None,
        Some(value) => Some(
            value
                .as_str()
                .ok_or("its `requires-python` is not a string")?
                .to_string(),
        ),
    };
    let dependencies = match table.get("dependencies") {
        None => Vec::new(),
        Some(value) => value
            .as_array()
            .and_then(|items| {
                items
                    .iter()
                    .map(|item| item.as_str().map(str::to_string))
                    .collect()
            })
            .ok_or("its `dependencies` is not a list of strings")?,
    };
    let outrig = match table.get("tool") {
        None => None,
        Some(tool) => tool
            .as_table()
            .ok_or("its `tool` is not a table")?
            .get("outrig"),
    };
    let mut declared = Declared {
        requires_python,
        dependencies,
        ..Declared::default()
    };
    if let Some(outrig) = outrig {
        for (key, value) in outrig
            .as_table()
            .ok_or("its `[tool.outrig]` is not a table")?
        {
            if key == "skill" {
                let skill = value
                    .as_table()
                    .ok_or("its `[tool.outrig.skill]` is not a table")?;
                outrig_skill(skill, &mut declared, warnings)?;
            } else {
                warnings.push(format!(
                    "`[tool.outrig]` has the key {key:?}, which OutRig does not know and ignores"
                ));
            }
        }
    }
    Ok(declared)
}

/// Read `[tool.outrig.skill]` into `declared`: `entry`, the entry function's name; `bind`, a
/// table from an entry parameter to the binding it takes; and `parameters`, a table from an entry
/// parameter to its help. Anything else is a warning.
fn outrig_skill(
    skill: &toml::Table,
    declared: &mut Declared,
    warnings: &mut Vec<String>,
) -> Result<(), String> {
    for (key, value) in skill {
        match key.as_str() {
            "entry" => {
                let entry = value
                    .as_str()
                    .ok_or("its `[tool.outrig.skill]` entry is not a string")?;
                declared.entry = Some(entry.to_string());
            }
            "bind" => {
                declared.bind = strings(value)
                    .ok_or("its `[tool.outrig.skill]` bind is not a table of strings")?;
            }
            "parameters" => {
                declared.parameters = strings(value)
                    .ok_or("its `[tool.outrig.skill.parameters]` is not a table of strings")?;
            }
            key => warnings.push(format!(
                "`[tool.outrig.skill]` has the key {key:?}, which OutRig does not know and ignores"
            )),
        }
    }
    Ok(())
}

/// `value` as a table of strings, or `None` when it is not one.
fn strings(value: &toml::Value) -> Option<BTreeMap<String, String>> {
    value
        .as_table()?
        .iter()
        .map(|(key, value)| Some((key.clone(), value.as_str()?.to_string())))
        .collect()
}

// ---------------------------------------------------------------------------- serving

impl Catalog {
    /// What the main agent's preamble lists, in precedence order.
    pub(crate) fn briefs(&self) -> Vec<SkillBrief> {
        self.skills
            .iter()
            .map(|served| SkillBrief {
                name: served.name.clone(),
                description: one_line(&served.description),
                package: served
                    .executable
                    .then(|| format!("outrig_skills.{}", served.name.replace('-', "_"))),
            })
            .collect()
    }

    fn served(&self, name: &str) -> Result<&Served, Problem> {
        self.skills
            .iter()
            .find(|served| served.name == name)
            .ok_or_else(|| {
                Problem::new(
                    "unknown-skill",
                    format!("the session serves no skill named {name:?}"),
                )
            })
    }

    /// Answer one request the interpreter sent: within the session's places, and its bound.
    async fn answer(self: &Arc<Self>, line: &Value) -> Result<Value, Problem> {
        let text = |key: &str| line[key].as_str().map(str::to_string);
        let kind = text("t").unwrap_or_default();
        let skill = text("skill").ok_or_else(|| Problem::source("the request names no skill"))?;
        let (path, digest) = (text("path"), text("digest"));
        let catalog = Arc::clone(self);
        bounded(&self.gate, self.bound, move || {
            let digest = digest.as_deref();
            match (kind.as_str(), path) {
                ("skill.load", _) => catalog.load(&skill),
                ("skill.check", _) => catalog.check(&skill, digest),
                ("skill.read", Some(path)) => catalog.read(&skill, &path, digest),
                ("skill.read", None) => Err(Problem::source("the request names no path")),
                ("skill.instructions", _) => catalog
                    .instructions(&skill, digest)
                    .map(|body| json!({"body": body})),
                (kind, _) => Err(Problem::source(format!(
                    "{kind:?} is not a request about skills"
                ))),
            }
        })
        .await
    }

    /// What importing `name`'s package needs: its source, its files, their digest, and what it
    /// declares it needs, read again only when the digest has moved, and kept only when the files
    /// did not change while it was read.
    fn load(&self, name: &str) -> Result<Value, Problem> {
        let (served, files, digest) = self.current(name, None)?;
        let known = lock(&served.meta).clone();
        let meta = if known.digest == digest {
            known
        } else {
            let read = read_skill(&*served.from, name, &files, digest.clone());
            let meta = served.unchanged(&digest, read)?.meta;
            *lock(&served.meta) = meta.clone();
            meta
        };
        let paths: Vec<&str> = files.iter().map(|file| file.path.as_str()).collect();
        let declared = &meta.declared;
        Ok(json!({
            "source": served.source,
            "digest": meta.digest,
            "files": paths,
            "requires_python": declared.requires_python,
            "dependencies": declared.dependencies,
            "entry": declared.entry,
            "bind": declared.bind,
            "parameters": declared.parameters,
        }))
    }

    /// Nothing, when `name`'s files are still at `digest`, the one its package was imported at;
    /// `skill-changed` when they are not.
    fn check(&self, name: &str, digest: Option<&str>) -> Result<Value, Problem> {
        let digest = digest.ok_or_else(|| Problem::source("the request names no digest"))?;
        self.current(name, Some(digest))?;
        Ok(json!({}))
    }

    /// `name`, its files and their digest, refused as `skill-changed` when `digest`, the one its
    /// package was imported at, is no longer theirs.
    fn current(
        &self,
        name: &str,
        digest: Option<&str>,
    ) -> Result<(&Served, Vec<SkillFile>, String), Problem> {
        let served = self.served(name)?;
        let (files, now) = listing(&*served.from, name)?;
        if digest.is_some_and(|digest| digest != now) {
            return Err(changed(name, "after its package was imported"));
        }
        Ok((served, files, now))
    }

    /// What `path` of `name` holds, base64-encoded.
    fn read(&self, name: &str, path: &str, digest: Option<&str>) -> Result<Value, Problem> {
        let (served, files, digest) = self.current(name, digest)?;
        if !files.iter().any(|file| file.path == path) {
            return Err(Problem::source(format!(
                "skill {name:?} has no file {path:?}"
            )));
        }
        let read = read_checked(&*served.from, name, path);
        let data = served.unchanged(&digest, read)?;
        Ok(json!({"data": base64::engine::general_purpose::STANDARD.encode(data)}))
    }

    /// `name`'s instructions: its `SKILL.md` past the frontmatter.
    fn instructions(&self, name: &str, digest: Option<&str>) -> Result<String, Problem> {
        let (served, files, digest) = self.current(name, digest)?;
        if !files.iter().any(|file| file.path == "SKILL.md") {
            return Err(Problem::invalid(format!("skill {name:?} has no SKILL.md")));
        }
        let read = text(&*served.from, name, "SKILL.md");
        let text = served.unchanged(&digest, read)?;
        let (_, body) = split(&text).ok_or_else(|| {
            Problem::invalid(format!(
                "skill {name:?}'s SKILL.md no longer begins with frontmatter"
            ))
        })?;
        Ok(body.to_string())
    }

    /// The skill a directive names: the name of the source serving it, and whether it has a
    /// `skill.py`; `None` when the session serves no skill of that name.
    pub(crate) fn directed(&self, name: &str) -> Option<(String, bool)> {
        let served = self.served(name).ok()?;
        Some((served.source.clone(), served.executable))
    }

    /// `name`'s instructions, read through its source now, within the session's places and its
    /// bound: what a directive for a skill with no `skill.py` delivers. An error says why they
    /// could not be read, its code first.
    pub(crate) async fn read_instructions(self: &Arc<Self>, name: &str) -> Result<String, String> {
        let (catalog, name) = (Arc::clone(self), name.to_string());
        bounded(&self.gate, self.bound, move || {
            catalog.instructions(&name, None)
        })
        .await
        .map_err(|problem| format!("{}: {}", problem.code, problem.message))
    }

    /// Publish on `events` what the interpreter said of a skill's import.
    fn imported(&self, line: &Value, events: &Events) {
        let text = |value: &Value| value.as_str().map(str::to_string);
        let skill = text(&line["skill"]).unwrap_or_default();
        let source = self
            .served(&skill)
            .map(|served| served.source.clone())
            .unwrap_or_default();
        events.emit(Payload::SkillImported {
            skill,
            source,
            agent: text(&line["agent"]),
            digest: text(&line["digest"]),
            reload: line["reload"].as_bool().unwrap_or(false),
            code: text(&line["error"]["code"]),
            reason: line["error"]["message"].as_str().map(printable),
        });
    }

    /// The line that tells the interpreter the catalog, before any kernel opens.
    fn boot_line(&self) -> Value {
        let skills: Vec<Value> = self
            .skills
            .iter()
            .map(|served| {
                json!({
                    "name": served.name,
                    "description": served.description,
                    "source": served.source,
                    "executable": served.executable,
                })
            })
            .collect();
        json!({"t": "skills", "skills": skills})
    }
}

impl Served {
    /// `read`, what was just read of this skill's files at `digest`, when they are at it still;
    /// otherwise `skill-changed`, whatever was read: a file saved meanwhile can be read part old
    /// and part new, or fail to read at all.
    fn unchanged<T>(&self, digest: &str, read: Result<T, Problem>) -> Result<T, Problem> {
        if listing(&*self.from, &self.name)?.1 != digest {
            return Err(changed(&self.name, "while they were read"));
        }
        read
    }
}

/// That skill `name`'s files changed `when`, so a request about them cannot be answered.
fn changed(name: &str, when: &str) -> Problem {
    Problem::new(
        "skill-changed",
        format!(
            "skill {name:?}'s files changed {when}; `outrig.skills.reload({name:?})` imports it \
             again"
        ),
    )
}

/// `text` on one line: each run of whitespace or control characters one space.
fn one_line(text: &str) -> String {
    text.split(|c: char| c.is_whitespace() || c.is_control())
        .filter(|word| !word.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Serve `catalog` to `interpreter`: tell it the skills, before any kernel opens, and answer each
/// request it sends about one, publishing each import on `events`. Nothing, for a session given
/// no source.
pub(crate) fn serve(catalog: &Arc<Catalog>, interpreter: &Interpreter, events: &Events) {
    if !catalog.given {
        return;
    }
    if !catalog.skills.is_empty() {
        interpreter.declare_skills(&catalog.boot_line());
    }
    let (catalog, send, events) = (
        Arc::clone(catalog),
        interpreter.relay_line(),
        events.clone(),
    );
    // Called by the reader of the interpreter's output, which must never wait: a request is
    // answered from a task of its own.
    interpreter.attach_skills(Arc::new(move |line: Value| {
        if line["t"] == "skill.imported" {
            catalog.imported(&line, &events);
            return;
        }
        let Some(id) = line["id"].as_u64() else {
            tracing::warn!(
                "ignored a {} line from the interpreter, which has no id",
                line["t"]
            );
            return;
        };
        let (catalog, send) = (Arc::clone(&catalog), Arc::clone(&send));
        tokio::spawn(async move {
            let answer = match catalog.answer(&line).await {
                Ok(ok) => json!({"t": "skill", "id": id, "ok": ok}),
                Err(problem) => json!({
                    "t": "skill",
                    "id": id,
                    "error": {"code": problem.code, "message": problem.message},
                }),
            };
            send(answer);
        });
    }));
}

/// The answer to a request about skills that nothing serves: a session given no source.
pub(crate) fn unserved(line: &Value) -> Option<Value> {
    let id = line["id"].as_u64()?;
    Some(json!({
        "t": "skill",
        "id": id,
        "error": {"code": "unknown-skill", "message": "the session serves no skills"},
    }))
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl SkillDiagnostic {
    fn new(skill: Option<&str>, source: &str, code: &str, message: String) -> Self {
        Self {
            skill: skill.map(str::to_string),
            source: source.to_string(),
            code: code.to_string(),
            message: printable(&message),
        }
    }
}

/// `text` with each control character but a line break or a tab escaped, so what it quotes of a
/// skill -- the line a parser stopped at, say -- cannot drive the terminal it is shown on.
fn printable(text: &str) -> String {
    let mut shown = String::with_capacity(text.len());
    for c in text.chars() {
        if c.is_control() && c != '\n' && c != '\t' {
            shown.extend(c.escape_debug());
        } else {
            shown.push(c);
        }
    }
    shown
}

#[cfg(test)]
#[path = "skills_tests.rs"]
mod tests;
