//! A session's application code: the package its owner supplies -- the task-bound subclass and
//! what it imports -- read on the host with no Python run, copied whole to a directory named for
//! its digest, mounted read-only and put on `sys.path` before the workspace, its requirement set
//! installed, and the modules its owner names imported as the session starts
//! (`plan/phase/0003-python/application-code.md`).
//!
//! The snapshot, not the owner's directory, is what every kernel imports: an edit made while the
//! session runs changes nothing until the next session, and the digest says which files the
//! session ran. Only pure Python can load in the interpreter, so a compiled file in the artifact
//! is refused before anything is copied, and the requirement set installs as a binding's does,
//! wheels only and each one checked pure.

use std::borrow::Cow;
use std::collections::HashSet;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::Duration;

use regex::Regex;

use super::bindings::{self, Places};
use super::event::Payload;
use super::skills;
use crate::container::ContainerMount;
use crate::error::{IoPathExt, OutrigError, Result};
use crate::events::Events;
use crate::python::host::{ImportFailure, Interpreter, InterpreterError};
use crate::python::install::{self, Install};
use crate::python::payload::{self, APPLICATION_MOUNT};
use crate::supervise;

/// What [`SessionBuilder::application`](super::SessionBuilder::application) takes: where a
/// session's application code is, what it requires, and which of its modules to import as the
/// session starts.
#[derive(Debug, Clone)]
pub struct ApplicationSpec {
    artifact: Artifact,
    requires: Vec<String>,
    imports: Vec<String>,
    label: Option<String>,
}

/// Where application code is: the two forms read to the same files.
#[derive(Clone)]
enum Artifact {
    Directory(PathBuf),
    Archive(Cow<'static, [u8]>),
}

impl fmt::Debug for Artifact {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Directory(path) => f.debug_tuple("Directory").field(path).finish(),
            Self::Archive(bytes) => write!(f, "Archive({} bytes)", bytes.len()),
        }
    }
}

impl ApplicationSpec {
    /// The code in the directory `path`, whose top-level entries are packages and modules, as a
    /// project's `src/` is.
    ///
    /// It is read as the session starts, with no Python run: every regular file beneath it but
    /// those whose names begin `.` and `__pycache__`, at most 1,024 entries, 16 directories deep,
    /// each file at most 8 MiB. Nothing below `path` is reached through a link: a link there, or
    /// anything that is not a regular file, fails the start, as does a compiled extension module
    /// (`.so`, `.pyd`), which the session's interpreter cannot load.
    pub fn directory(path: impl Into<PathBuf>) -> Self {
        Self::of(Artifact::Directory(path.into()))
    }

    /// The code in `archive`, an uncompressed tar laid out as [`ApplicationSpec::directory`]'s
    /// directory is -- `include_bytes!` of one an embedder built, which is not copied, say. It is
    /// unpacked into a directory of its own as the session starts and read from there as a
    /// directory is, held to the same rules; a member whose path leads out of the archive fails
    /// the start too. An archive of a directory's files has the directory's digest.
    pub fn archive(archive: impl Into<Cow<'static, [u8]>>) -> Self {
        Self::of(Artifact::Archive(archive.into()))
    }

    fn of(artifact: Artifact) -> Self {
        Self {
            artifact,
            requires: Vec::new(),
            imports: Vec::new(),
            label: None,
        }
    }

    /// The distributions the code imports beyond the standard library and `outrig`, as a
    /// `[bindings.<name>]` entry's `requires` lists them: a name, extras, a version specifier and
    /// markers, never a path or a URL. They are installed once into the cache a binding's are,
    /// pure-Python wheels only, and imported from beside the code.
    pub fn requires<I, S>(mut self, requires: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.requires = requires.into_iter().map(Into::into).collect();
        self
    }

    /// The modules imported as the session starts, in order, before any kernel opens: one that
    /// raises, is not found, or reaches compiled code fails the start, naming it, as does one
    /// whose import does not end within 30 seconds. A module not named here is imported when
    /// something first imports it.
    pub fn imports<I, S>(mut self, modules: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.imports = modules.into_iter().map(Into::into).collect();
        self
    }

    /// What the owner calls the code -- its release's version, say -- recorded beside the digest
    /// and compared by nothing.
    pub fn label(mut self, label: impl Into<String>) -> Self {
        self.label = Some(label.into());
        self
    }
}

/// Which application code a session runs: what
/// [`Session::application`](super::Session::application) answers.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ApplicationIdentity {
    /// The first 16 hex digits of a BLAKE3 digest of the code's files' paths and contents, the
    /// same for a directory and for an archive holding the same files.
    pub digest: String,
    /// What the owner calls it, which nothing compares.
    pub label: Option<String>,
    /// The requirement set installed for it, sorted.
    pub requires: Vec<String>,
}

/// A module name an application may be asked to import: dotted identifiers.
static MODULE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^[A-Za-z_][A-Za-z0-9_]*(\.[A-Za-z_][A-Za-z0-9_]*)*$").expect("a valid pattern")
});

/// Refuse what `spec` cannot start: a requirement that is not a specifier, or an import that is
/// not a module's name.
pub(crate) fn check(spec: Option<&ApplicationSpec>) -> Result<()> {
    let Some(spec) = spec else {
        return Ok(());
    };
    if let Some(problem) = install::requirement_problem(&spec.requires) {
        return Err(OutrigError::Configuration(format!(
            "the application's `requires`: {problem}"
        )));
    }
    if let Some(bad) = spec.imports.iter().find(|module| !MODULE.is_match(module)) {
        return Err(OutrigError::Configuration(format!(
            "the application's `imports` names {bad:?}, which is not a module's dotted name"
        )));
    }
    Ok(())
}

/// Application code, provisioned on the host: its snapshot and its requirement set's install,
/// ready to mount and to tell the interpreter of.
#[derive(Debug)]
pub(crate) struct Prepared {
    /// The directory the owner gave, or `None` for an archive.
    path: Option<PathBuf>,
    identity: ApplicationIdentity,
    /// The snapshot on the host.
    dir: PathBuf,
    /// The requirement set's install on the host, or `None` when it is empty.
    packages: Option<PathBuf>,
    imports: Vec<String>,
}

/// Read `spec`'s code, refuse it if any of it is compiled, copy it to the snapshot its digest
/// names unless one is there already, and install its requirement set under `places`.
pub(crate) async fn prepare(spec: ApplicationSpec, places: &Places) -> Result<Prepared> {
    let ApplicationSpec {
        artifact,
        requires,
        imports,
        label,
    } = spec;
    let requires = install::requirement_set(&requires)?;
    let path = match &artifact {
        Artifact::Directory(path) => Some(path.clone()),
        Artifact::Archive(_) => None,
    };
    // Beside the requirement sets' installs: `~/.cache/outrig/applications/` by default.
    let root = payload::cache_dir("application code")?.join("outrig/applications");
    let (digest, dir) = supervise::blocking(move |_| snapshot_of(&artifact, &root))
        .await
        .map_err(|e| OutrigError::Io(std::io::Error::other(e)))??;
    let packages = if requires.is_empty() {
        None
    } else {
        let python = payload::host_python().await?;
        let install = Install {
            python: &python,
            requires: &requires,
            env: &places.pip_env,
        };
        let dir = install::install(&places.packages_root, install)
            .await
            .map_err(|e| {
                OutrigError::Configuration(format!(
                    "the packages the application requires could not be installed: {e}"
                ))
            })?;
        Some(dir)
    };
    Ok(Prepared {
        path,
        identity: ApplicationIdentity {
            digest,
            label,
            requires,
        },
        dir,
        packages,
        imports,
    })
}

/// `artifact`'s digest, and its snapshot under `root`: copied whole, as the `outrig` package is
/// written, unless one is there already.
fn snapshot_of(artifact: &Artifact, root: &Path) -> Result<(String, PathBuf)> {
    let files = match artifact {
        Artifact::Directory(path) => skills::read_tree(path).map_err(|e| {
            OutrigError::Configuration(format!(
                "the application in {} cannot be read: {e}",
                path.display()
            ))
        })?,
        Artifact::Archive(archive) => read_archive(archive, root)?,
    };
    if let Some((path, _)) = files
        .iter()
        .find(|(path, _)| install::extension_module(Path::new(path)))
    {
        return Err(OutrigError::Configuration(format!(
            "the application is refused (not-pure-python): {path} is a compiled extension module, \
             and the session's interpreter loads no compiled code, so application code is pure \
             Python only"
        )));
    }
    let digest = payload::digest(&files);
    let dir = root.join(&digest);
    if !dir.is_dir() {
        payload::locked_once(&dir, || payload::write_package(&dir, &files))?;
    }
    Ok((digest, dir))
}

/// The files of the tar `archive`: unpacked into a directory of its own under `root`, where a
/// member that would land outside it is refused, as the payload's archive is unpacked, and read
/// from there as a directory of code is read. A member that reading would skip or refuse by its
/// name, depth, size or number is never written.
fn read_archive(archive: &[u8], root: &Path) -> Result<Vec<(String, Vec<u8>)>> {
    let refused = |e: &dyn fmt::Display| {
        OutrigError::Configuration(format!("the application's archive cannot be read: {e}"))
    };
    std::fs::create_dir_all(root).path_ctx("create", root)?;
    let stage = tempfile::Builder::new()
        .prefix(".unpack-")
        .tempdir_in(root)
        .path_ctx("create a directory in", root)?;
    let mut tar = tar::Archive::new(archive);
    let mut seen = HashSet::new();
    for entry in tar.entries().map_err(|e| refused(&e))? {
        let mut entry = entry.map_err(|e| refused(&e))?;
        let path = entry.path().map_err(|e| refused(&e))?.into_owned();
        // `git archive`'s first member, a comment `unpack_in` writes nothing of, is no entry.
        if entry.header().entry_type().is_pax_global_extensions()
            || !skills::admits(&path, entry.size(), &mut seen).map_err(|e| refused(&e))?
        {
            continue;
        }
        if !entry.unpack_in(stage.path()).map_err(|e| refused(&e))? {
            return Err(refused(&format!("{path:?} leads out of the archive")));
        }
    }
    skills::read_tree(stage.path()).map_err(|e| refused(&e))
}

impl Prepared {
    /// The snapshot, as the container binds it: read-only at `/outrig/app/<digest>`.
    pub(crate) fn snapshot(&self) -> ContainerMount {
        let mount = Path::new(APPLICATION_MOUNT).join(&self.identity.digest);
        ContainerMount::shared_read_only(&self.dir, mount)
    }

    /// The requirement set's install on the host, or `None` when it is empty.
    pub(crate) fn packages(&self) -> Option<&Path> {
        self.packages.as_deref()
    }

    /// Tell `interpreter` the code, as it sees it -- in the container, or on the host for an
    /// interpreter running there -- and import its modules, each within `bound`, recording each
    /// step to `events`. Before any kernel opens.
    pub(crate) async fn provision(
        self,
        interpreter: &Interpreter,
        events: &Events,
        bound: Duration,
        in_container: bool,
    ) -> std::result::Result<ApplicationIdentity, InterpreterError> {
        let (mount, packages) = if in_container {
            (
                self.snapshot().container,
                self.packages.as_deref().map(bindings::container_path),
            )
        } else {
            (self.dir.clone(), self.packages.clone())
        };
        let mount = mount.to_string_lossy().into_owned();
        let packages = packages.map(|dir| dir.to_string_lossy().into_owned());
        events.emit(Payload::ApplicationProvisioned {
            path: self.path,
            digest: self.identity.digest.clone(),
            label: self.identity.label.clone(),
            mount: mount.clone(),
            requires: self.identity.requires.clone(),
            packages: packages.clone(),
            imports: self.imports.clone(),
        });
        let paths: Vec<String> = std::iter::once(mount).chain(packages).collect();
        interpreter
            .provision(&paths, &self.imports, bound, |imported| {
                let error = imported.error.as_ref();
                events.emit(Payload::ApplicationImported {
                    module: imported.module.clone(),
                    code: error.map(|e| e.code.clone()),
                    reason: error.map(ImportFailure::reason),
                });
            })
            .await?;
        Ok(self.identity)
    }
}

#[cfg(test)]
#[path = "application_tests.rs"]
mod tests;
