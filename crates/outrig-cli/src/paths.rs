//! CLI-owned repo, config, and cache path policy.

use std::fs::{self, File, Permissions};
use std::io::{ErrorKind, Write as _};
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};

use directories::{BaseDirs, ProjectDirs};
use nix::libc;
use tempfile::NamedTempFile;

use outrig::config::{Config, ConfigSource, MountAccess, Workspace};
use outrig::error::{IoPathExt, OutrigError, Result};

const REPO_CONFIG_REL: &str = ".agents/outrig/config.toml";
const IMAGES_REL: &str = ".agents/outrig/images";
const GLOBAL_CONFIG_FILE: &str = "config.toml";
const GLOBAL_HOME_DIR: &str = ".outrig";
const GLOBAL_XDG_DIR: &str = "outrig";

/// The process working directory, with a message that says what went wrong.
/// The bare `std::env::current_dir()` error is an unqualified ENOENT --
/// indistinguishable from a missing file -- and it fires whenever the
/// directory the user launched from has since been deleted or unmounted.
pub(crate) fn current_dir() -> Result<PathBuf> {
    std::env::current_dir().map_err(|source| {
        OutrigError::Configuration(format!(
            "cannot determine the current directory: {source}\n\
             help: the directory may have been deleted or unmounted -- `cd` somewhere else and retry"
        ))
    })
}

/// The nearest directory at or above `cwd` holding `.agents/outrig/config.toml`,
/// refused when that config is not the invoking user's own (see
/// [`refuse_foreign_repo_config`]).
pub(crate) fn find_repo_root_from(cwd: &Path) -> Result<PathBuf> {
    find_repo_root_with(cwd, nix::unistd::geteuid().as_raw())
}

/// [`find_repo_root_from`] for the user `uid`.
fn find_repo_root_with(cwd: &Path, uid: u32) -> Result<PathBuf> {
    let mut cur = cwd;
    loop {
        let config = cur.join(REPO_CONFIG_REL);
        match fs::metadata(&config) {
            Ok(meta) if meta.is_file() => {
                refuse_foreign_repo_config(cur, uid)?;
                return Ok(cur.to_path_buf());
            }
            // A `.agents/` or `.agents/outrig/` that is a file rather than a
            // directory, or a `config.toml` that is a directory: not the
            // config this is looking for, so the walk goes on, same as a
            // missing entry.
            Ok(_) => {}
            Err(e) if matches!(e.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory) => {}
            // Anything else -- a directory in the path the user cannot
            // search, say -- says nothing about whether the config is there,
            // so it is not treated as absent: `is_file()` collapsed it to
            // `false` and let the walk continue past a config it could not
            // examine, landing on one further up or none at all (#829).
            Err(e) => return Err(e).path_ctx("stat", config),
        }
        match cur.parent() {
            Some(parent) => cur = parent,
            None => return Err(OutrigError::NoRepoConfig),
        }
    }
}

/// Refuse the repo config under `root` unless `uid` owns it, the
/// `.agents/outrig/` directories it sits in, and `root` itself. Whoever owns
/// any of those could have put the file there, and the config decides what is
/// mounted, which commands run, and where API keys are sent, all as the user
/// running outrig -- so a directory others can write, such as `/tmp`, must not
/// hand its config to everything below it. git refuses another user's
/// repository for the same reason. Each entry is judged as itself rather than
/// through a symlink, so a link someone else planted is refused even when it
/// points at a config of your own. `--config` names a file outright and is not
/// asked.
fn refuse_foreign_repo_config(root: &Path, uid: u32) -> Result<()> {
    // A walk from a relative `cwd` can end at the empty path, which is the
    // current directory to `join` but nothing at all to `stat`.
    let root = if root.as_os_str().is_empty() {
        Path::new(".")
    } else {
        root
    };
    let config = repo_config_path(root);
    // `config.toml`, `outrig/`, `.agents/`, and then `root`.
    for path in config.ancestors().take(4) {
        let owner = std::fs::symlink_metadata(path)
            .path_ctx("stat", path)?
            .uid();
        if owner != uid {
            let entry = if path == config {
                "it".to_string()
            } else {
                path.display().to_string()
            };
            // Not "pass --config": with a `--config` of their own, the walk
            // only picks the root, and `image add` does not read the flag.
            return Err(OutrigError::Configuration(format!(
                "refusing {config}: {entry} is owned by uid {owner}, not by you (uid {uid}), \
                 so another user could have written it\n\
                 help: run `outrig init` where you are to start a config of your own; \
                 outrig reads a config you do not own only when --config names it",
                config = config.display(),
            )));
        }
    }
    Ok(())
}

pub(crate) fn repo_config_path(root: &Path) -> PathBuf {
    root.join(REPO_CONFIG_REL)
}

pub(crate) fn image_dir(root: &Path, name: &str) -> PathBuf {
    root.join(IMAGES_REL).join(name)
}

pub(crate) fn image_dir_rel(name: &str) -> PathBuf {
    Path::new(IMAGES_REL).join(name)
}

pub(crate) fn write_atomic(path: &Path, contents: &str) -> Result<()> {
    write_atomic_all(&[(path, contents)])
}

/// Writes each file atomically, in order, after staging every one in a temp
/// file beside its target: creating, writing, or syncing any of them fails
/// before a single target changes. Only a failed rename can leave the
/// earlier files written and the later ones not.
pub(crate) fn write_atomic_all(files: &[(&Path, &str)]) -> Result<()> {
    let staged = files
        .iter()
        .map(|&(path, contents)| stage(path, contents).map(|tmp| (tmp, path)))
        .collect::<Result<Vec<_>>>()?;
    for (tmp, path) in staged {
        tmp.persist(path).map_err(OutrigError::from)?;
    }
    Ok(())
}

/// `contents` in a synced temp file in `path`'s directory, created if
/// missing. Dropped instead of persisted, the temp file is removed.
///
/// The rename keeps the temp file's mode, so it is set here before any
/// content is written. A new target gets `0666` under the umask, as `open(2)`
/// would create it; a bare `NamedTempFile` is `0600`, which would hide a
/// checked-in `Dockerfile` from the group. An existing target keeps its own
/// mode and access ACL, so a forced rewrite resets nothing the user chose --
/// and the temp file starts `0600`, never broader than the target, so nobody
/// can open it in the window before it takes the target's mode and keep a
/// descriptor to what is written next.
fn stage(path: &Path, contents: &str) -> Result<NamedTempFile> {
    let parent = path.parent().ok_or_else(|| {
        OutrigError::Configuration(format!("path has no parent: {}", path.display()))
    })?;
    std::fs::create_dir_all(parent).path_ctx("create directory", parent)?;
    let mut tmp = match std::fs::metadata(path) {
        Ok(existing) => {
            let tmp = NamedTempFile::new_in(parent)?;
            // A new file takes its directory's group, which can be a broader one
            // than the target's: the group is part of who the mode and ACL grant
            // to, so it is restored first, and a rewrite that cannot is refused.
            if existing.gid() != tmp.as_file().metadata()?.gid() {
                nix::unistd::fchown(tmp.as_file(), None, Some(existing.gid().into()))
                    .map_err(std::io::Error::from)?;
            }
            copy_access_acl(path, tmp.as_file())?;
            tmp.as_file().set_permissions(existing.permissions())?;
            tmp
        }
        Err(_) => tempfile::Builder::new()
            .permissions(Permissions::from_mode(0o666))
            .tempfile_in(parent)?,
    };
    tmp.write_all(contents.as_bytes())?;
    tmp.as_file().sync_all()?;
    Ok(tmp)
}

/// Give `dst` the access ACL of `src`, or none when `src` has none. A file
/// created under a directory with a default ACL inherits its named entries,
/// which would grant readers `src` never had once a `chmod` unmasks them.
/// Filesystems without ACLs have nothing to copy.
fn copy_access_acl(src: &Path, dst: &File) -> std::io::Result<()> {
    use std::ffi::CString;
    use std::io::{Error, ErrorKind};
    use std::os::fd::AsRawFd as _;
    use std::os::unix::ffi::OsStrExt as _;

    const ACL: &std::ffi::CStr = c"system.posix_acl_access";
    let no_acl = |e: &Error| matches!(e.raw_os_error(), Some(libc::ENODATA | libc::ENOTSUP));
    let src = CString::new(src.as_os_str().as_bytes())
        .map_err(|e| Error::new(ErrorKind::InvalidInput, e))?;
    // The ACL can change size between the query and the read, so a read that
    // finds it grown asks again.
    let acl = loop {
        // SAFETY: `src` and `ACL` are NUL-terminated; a null buffer of length 0
        // asks for the size.
        let size = unsafe { libc::getxattr(src.as_ptr(), ACL.as_ptr(), std::ptr::null_mut(), 0) };
        if size < 0 {
            break Err(Error::last_os_error());
        }
        let mut acl = vec![0u8; size as usize];
        // SAFETY: as above, and `acl` is writable for the length passed.
        let len = unsafe {
            libc::getxattr(src.as_ptr(), ACL.as_ptr(), acl.as_mut_ptr().cast(), acl.len())
        };
        if len >= 0 {
            acl.truncate(len as usize);
            break Ok(acl);
        }
        let err = Error::last_os_error();
        if err.raw_os_error() != Some(libc::ERANGE) {
            break Err(err);
        }
    };
    let fd = dst.as_raw_fd();
    let err = match acl {
        Ok(acl) => {
            // SAFETY: `fd` is open, `ACL` is NUL-terminated, and `acl` is
            // readable for the length passed.
            let rc = unsafe {
                libc::fsetxattr(fd, ACL.as_ptr(), acl.as_ptr().cast(), acl.len(), 0)
            };
            return if rc == 0 { Ok(()) } else { Err(Error::last_os_error()) };
        }
        Err(err) => err,
    };
    if !no_acl(&err) {
        return Err(err);
    }
    // SAFETY: `fd` is open and `ACL` is NUL-terminated.
    if unsafe { libc::fremovexattr(fd, ACL.as_ptr()) } != 0 {
        let err = Error::last_os_error();
        if !no_acl(&err) {
            return Err(err);
        }
    }
    Ok(())
}

/// The repo config a command reads, and the repo it runs against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoConfig {
    /// The repo: the default workspace and the session's working directory.
    pub root: PathBuf,
    /// A `--config` file outside `.agents/outrig/`, read in place of the
    /// repo's own. `None` reads `<root>/.agents/outrig/config.toml`, which a
    /// config-less `outrig run` or `outrig mcp` does not have.
    pub file: Option<PathBuf>,
}

impl RepoConfig {
    /// The repo at `root`, reading its own `.agents/outrig/config.toml`.
    pub fn at_root(root: PathBuf) -> Self {
        Self { root, file: None }
    }

    /// The file this reads.
    pub(crate) fn config_path(&self) -> PathBuf {
        self.file
            .clone()
            .unwrap_or_else(|| repo_config_path(&self.root))
    }

    /// `global` is the global config as [`resolve_global_config`] found it:
    /// `None` is a run without one, which is what a missing file amounts to.
    pub(crate) fn load(&self, global: Option<&Path>) -> Result<Config> {
        let cfg = self.load_unvalidated(global)?;
        cfg.validate(Some(&self.root))?;
        Ok(cfg)
    }

    pub(crate) fn load_for_run(
        &self,
        global: Option<&Path>,
        agent_flag: Option<&str>,
        model_override: Option<&str>,
    ) -> Result<Config> {
        let cfg = self.load_unvalidated(global)?;
        cfg.validate_for_run(Some(&self.root), agent_flag, model_override)?;
        Ok(cfg)
    }

    pub(crate) fn load_for_build(&self, global: Option<&Path>) -> Result<Config> {
        let cfg = self.load_unvalidated(global)?;
        cfg.validate_for_build(Some(&self.root))?;
        Ok(cfg)
    }

    /// Both files read and merged, with whatever either set aside reported
    /// before the caller validates: a key set aside can be why a rule then
    /// fails, and its warning is what explains that error.
    fn load_unvalidated(&self, global: Option<&Path>) -> Result<Config> {
        let cfg = match &self.file {
            Some(file) => Config::load_file_unvalidated(file, global),
            None => Config::load_unvalidated(&self.root, global),
        }?;
        report_config_warnings(&cfg);
        Ok(cfg)
    }
}

/// One `[outrig] warning:` line on stderr for each key the load of `cfg` set
/// aside. stderr because `outrig mcp` speaks JSON-RPC on stdout.
pub(crate) fn report_config_warnings(cfg: &Config) {
    for warning in cfg.warnings() {
        eprintln!("[outrig] warning: {warning}");
    }
}

/// The repo config for `build`: `--config` when given (see
/// [`explicit_repo_config`]), else the walk up from `cwd`, where finding
/// nothing is [`OutrigError::NoRepoConfig`].
pub(crate) fn resolve_repo_config(override_path: Option<&Path>, cwd: &Path) -> Result<RepoConfig> {
    match override_path {
        Some(p) => explicit_repo_config(p, cwd),
        None => find_repo_root_from(cwd).map(RepoConfig::at_root),
    }
}

/// Like [`resolve_repo_config`] but finding no repo config is not an error.
/// `outrig run`/`outrig mcp` may run in a directory with no
/// `.agents/outrig/config.toml`. With no `--config` and nothing found up the
/// tree, `cwd` is the root, and `Config::load` treats its missing file as an
/// empty config merged over the global config.
pub(crate) fn resolve_repo_config_optional(
    override_path: Option<&Path>,
    cwd: &Path,
) -> Result<RepoConfig> {
    match override_path {
        Some(p) => explicit_repo_config(p, cwd),
        None => Ok(RepoConfig::at_root(repo_root_or_cwd(cwd)?)),
    }
}

/// `--config <path>`. The file has to exist: the flag names it outright, so
/// standing in an empty config would run a session it does not describe.
/// One at `<repo>/.agents/outrig/config.toml` is that repo's own, and means
/// what running from `<repo>` means. Any other is read on its own, for the
/// repo found from `cwd` as if no flag were given: where the file sits never
/// picks the directory mounted as the workspace.
///
/// The path is taken against `cwd` first, so the root is absolute. Taken as
/// written, `--config .agents/outrig/config.toml` has the empty path three
/// levels up, and the empty path is not the current directory to what
/// receives it: `Path::new("").exists()` is false, and a path joined to it
/// stays relative to wherever it is later opened from.
fn explicit_repo_config(path: &Path, cwd: &Path) -> Result<RepoConfig> {
    let file: PathBuf = cwd.join(path).components().collect();
    match fs::metadata(&file) {
        Ok(meta) if meta.is_file() => {}
        Ok(_) => return Err(not_an_existing_file(path)),
        Err(e) if matches!(e.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory) => {
            return Err(not_an_existing_file(path));
        }
        // A `stat` that failed for another reason (a directory it may not
        // search, say) says nothing about whether the file is there.
        Err(e) => {
            return Err(OutrigError::Configuration(format!(
                "--config {} cannot be read: {e}",
                path.display()
            )));
        }
    }
    if file.ends_with(REPO_CONFIG_REL) {
        let root = file.ancestors().nth(3).expect("an absolute path ending in \
            .agents/outrig/config.toml has a third ancestor");
        return Ok(RepoConfig::at_root(root.to_path_buf()));
    }
    Ok(RepoConfig {
        root: repo_root_or_cwd(cwd)?,
        file: Some(file),
    })
}

fn not_an_existing_file(path: &Path) -> OutrigError {
    OutrigError::Configuration(format!(
        "--config {} is not an existing file",
        path.display()
    ))
}

/// The repo found from `cwd`, else `cwd` when there is none. A config the walk
/// refused is an error, not a config-less run.
fn repo_root_or_cwd(cwd: &Path) -> Result<PathBuf> {
    match find_repo_root_from(cwd) {
        Err(OutrigError::NoRepoConfig) => Ok(cwd.to_path_buf()),
        found => found,
    }
}

/// Refuse a workspace no config declared when it is the invoking user's home
/// directory or a directory above it. Such a workspace is outrig's own pick --
/// the repo root, which a run from `~`, an MCP client started in `/`, or a
/// stray `~/.agents/outrig/config.toml` can make that directory -- and it
/// would put `~/.ssh` and `~/.gnupg` in the sandbox. A declared
/// `[workspace] host-path` is the user's pick, and the caller does not ask.
pub(crate) fn refuse_home_workspace(workspace: &Path, repo_root: &Path) -> Result<()> {
    match BaseDirs::new() {
        Some(dirs) => refuse_home_workspace_with(workspace, dirs.home_dir(), repo_root),
        None => Ok(()),
    }
}

/// [`refuse_home_workspace`] against an explicit `home`. Both paths are
/// compared canonicalized, so a symlink or a `..` cannot spell its way past;
/// one that will not canonicalize is compared as written.
fn refuse_home_workspace_with(workspace: &Path, home: &Path, repo_root: &Path) -> Result<()> {
    let canonical = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    let (workspace, home) = (canonical(workspace), canonical(home));
    if !home.starts_with(&workspace) {
        return Ok(());
    }
    let what = if workspace == home {
        "your home directory".to_string()
    } else {
        format!("above your home directory ({})", home.display())
    };
    let why = implicit_workspace_origin(repo_root);
    Err(OutrigError::Configuration(format!(
        "refusing to mount {} as the workspace: it is {what}, and {why}\n\
         help: run outrig from a project directory, or declare [workspace] host-path \
         to mount it on purpose",
        workspace.display()
    )))
}

/// Refuse a workspace whose *resolved* host path contains `:`, podman's bind-mount
/// field separator.
///
/// Config validation refuses a `:` in a `host-path` as written, which is a
/// config error wherever the config is read. This is the other half: a path
/// that spells none can still resolve through a directory whose name has one
/// -- the repo root for the built-in `.`, or the declaring file's directory
/// for a relative value -- and the resolved path is what becomes the `-v`.
///
/// It lives here, and not in validation, because it is a fact about this
/// launch rather than about the config: `outrig mcp --attach` borrows a
/// running container and serializes no mount at all, so a session started
/// from such a directory is none of its business. Every launch that does
/// mount asks, before the image is acquired, so the diagnostic arrives in
/// place of podman's -- which would come after the build and name neither the
/// workspace nor the directory the colon is in.
///
/// Why that path is the workspace is read off `workspace`, not guessed: a
/// declared `host-path` is the config's pick and is named with the file that
/// declared it, since telling its author to run from somewhere else would be
/// advice that cannot work -- a file-relative value resolves against that
/// file's directory wherever outrig is invoked from. Only an undeclared
/// workspace is outrig's own pick, and only it gets the
/// [`implicit_workspace_origin`] explanation and the cwd advice that follows
/// from it.
pub(crate) fn refuse_workspace_field_separator(
    resolved: &Path,
    workspace: &Workspace,
    repo_root: &Path,
) -> Result<()> {
    if !resolved.as_os_str().as_encoded_bytes().contains(&b':') {
        return Ok(());
    }
    let (origin, help) = match workspace.declared_host_path() {
        // Declared. Validation already refused a `:` this value spelled, so
        // the colon is in what it resolved *through*: the directory of the
        // file that declared it, or the repo root when nothing did.
        Some(declared) => (
            format!(
                "[workspace] host-path {}{} resolves there",
                declared.display(),
                declared_in_clause(workspace.config_source())
            ),
            "help: podman offers no way to escape one, so point [workspace] host-path at a \
             directory whose path has none",
        ),
        // Undeclared, so the path is outrig's own pick and the cwd is one of
        // the two things that can have made it.
        None => (
            implicit_workspace_origin(repo_root),
            "help: podman offers no way to escape one, so run outrig from a directory whose \
             path has no `:`, or declare [workspace] host-path pointing at one",
        ),
    };
    Err(OutrigError::Configuration(format!(
        "refusing to mount {} as the workspace: its path contains `:`, which podman reads \
         as the field separator of a `-v <host>:<container>[:<options>]` value, so the mount \
         would be refused at launch; {origin}\n{help}",
        resolved.display()
    )))
}

/// ` declared in <file>` for a diagnostic, or nothing when no config file
/// declared the value: a `--volume`, or one a library caller built.
fn declared_in_clause(source: Option<&ConfigSource>) -> String {
    source.map_or_else(String::new, |source| {
        format!(" declared in {}", source.config_path().display())
    })
}

/// What made `repo_root` the workspace when no config declared one, for a
/// refusal to name: the repo config at the root, or no repo config at all, which
/// leaves the current directory.
fn implicit_workspace_origin(repo_root: &Path) -> String {
    let repo_cfg = repo_config_path(repo_root);
    if repo_cfg.is_file() {
        format!("{} makes it the repo root", repo_cfg.display())
    } else {
        "no repo config was found, so it is the current directory".to_string()
    }
}

/// Canonicalize `p` even when it does not exist. Components are resolved left
/// to right, so a symlink is followed before any `..` after it is applied; a
/// component that does not exist is kept as written, and a later `..` pops it.
fn canonical_or_normalized(p: &Path) -> PathBuf {
    resolve_lenient(p).0
}

/// [`canonical_or_normalized`], plus the directory each name in `p` is looked
/// up in.
fn resolve_lenient(p: &Path) -> (PathBuf, Vec<PathBuf>) {
    use std::path::Component;
    let absolute = if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir().map_or_else(|_| p.to_path_buf(), |cwd| cwd.join(p))
    };
    let mut resolved = PathBuf::new();
    let mut lookups = Vec::new();
    for c in absolute.components() {
        match c {
            Component::ParentDir => {
                resolved.pop();
            }
            Component::CurDir => {}
            Component::Normal(name) => {
                lookups.push(resolved.clone());
                resolved.push(name);
                if let Ok(real) = std::fs::canonicalize(&resolved) {
                    resolved = real;
                }
            }
            c => resolved.push(c),
        }
    }
    (resolved, lookups)
}

/// Where a read-write mount lets an agent rewrite the global config, from the
/// path as spelled. The next session reads that spelling through the kernel,
/// not through [`canonical_or_normalized`], so it can land elsewhere than the
/// guard looked.
struct GlobalConfigReach {
    spelled: PathBuf,
    resolved: PathBuf,
    /// The directory each name in the path is looked up in. An agent that can
    /// write one can replace that name with a symlink, a missing one (which a
    /// `..` after it pops without the agent in the way) and `config.toml`
    /// itself included. A link followed to another link counts as one hop, so
    /// only the directory holding the first is listed.
    lookups: Vec<PathBuf>,
}

impl GlobalConfigReach {
    fn of(spelled: &Path) -> Self {
        let (resolved, lookups) = resolve_lenient(spelled);
        Self {
            spelled: spelled.to_path_buf(),
            resolved,
            lookups,
        }
    }

    /// Why an agent with `host` mounted read-write could rewrite the global
    /// config, if it could.
    fn rewritable_through(&self, host: &Path) -> Option<String> {
        if self.resolved.starts_with(host) {
            return Some(format!(
                "it is read-write and covers the global config ({})",
                self.resolved.display()
            ));
        }
        let dir = self.lookups.iter().find(|d| d.starts_with(host))?;
        Some(format!(
            "it is read-write and covers {}, which the global config path ({}) is looked up \
             through, so an agent could redirect it by replacing a component",
            dir.display(),
            self.spelled.display()
        ))
    }
}

/// Refuse a workspace no config declared when it is read-write over the global
/// config: its file, its directory, or a directory its path is looked up
/// through (see [`GlobalConfigReach`]). A file that is not there yet counts,
/// since an agent could create it. Like [`refuse_home_workspace`], this is about
/// outrig's own pick -- the repo root, or the current directory -- so the caller
/// asks only when `[workspace] host-path` is declared nowhere: the global
/// config's own is the operator's choice, and a repo's is judged by
/// [`pin_repo_declared_mounts`].
pub(crate) fn refuse_workspace_covering_global_config(
    workspace: &Path,
    repo_root: &Path,
    global_cfg: Option<&Path>,
) -> Result<()> {
    let Some(global) = global_cfg.map(GlobalConfigReach::of) else {
        return Ok(());
    };
    let workspace = canonical_or_normalized(workspace);
    let Some(reason) = global.rewritable_through(&workspace) else {
        return Ok(());
    };
    Err(OutrigError::Configuration(format!(
        "refusing to mount {} as the workspace: {reason}, and {}\n\
         help: run outrig from a project directory that does not hold the global config, \
         or declare [workspace] host-path in the global config to mount it on purpose",
        workspace.display(),
        implicit_workspace_origin(repo_root)
    )))
}

/// Refuse an extra mount whose *resolved* host path contains `:`.
///
/// The companion to [`refuse_workspace_field_separator`], and there for the
/// same reason: config validation refuses a `:` an entry spelled, but a
/// relative `host-path` resolves against the directory of the file that
/// declared it (or the repo root), and it is the resolved path that becomes
/// the `-v`. Both the primary workspace's extra mounts and every sidecar's
/// are checked, since one builder renders them all.
///
/// Named as resolved, which is where the colon is, with the entry as written
/// beside it so the line to edit is identifiable.
pub(crate) fn refuse_mount_field_separators(cfg: &Config, repo_root: &Path) -> Result<()> {
    let workspace = cfg.workspace.mounts.iter().map(|m| ("mount".to_string(), m));
    let sidecars = cfg.sidecars.iter().flat_map(|(name, sc)| {
        sc.mounts
            .iter()
            .map(move |m| (format!("sidecar {name:?} mount"), m))
    });
    for (what, mount) in workspace.chain(sidecars) {
        let resolved = mount.resolved_host_path(repo_root);
        if !resolved.as_os_str().as_encoded_bytes().contains(&b':') {
            continue;
        }
        // A `--volume` carries no file. Its own parser splits on `:`, so the
        // colon is in what the path resolved through.
        let declared = declared_in_clause(mount.config_source());
        return Err(OutrigError::Configuration(format!(
            "refusing {what} host-path {}{declared}: it resolves to {}, which contains `:` \
             -- podman reads that as the field separator of a `-v \
             <host>:<container>[:<options>]` value, so the mount would be refused at launch\n\
             help: podman offers no way to escape one; point it at a path with no `:` in it",
            mount.host_path().display(),
            resolved.display()
        )));
    }
    Ok(())
}

/// A repo-declared mount [`pin_repo_declared_mounts`] validated: the
/// canonical host path the guard saw, which is what the launch now binds.
#[derive(Debug, Clone)]
pub(crate) struct PinnedMount {
    what: String,
    host: PathBuf,
    source: PathBuf,
}

/// Refuse a repo-declared host path that reaches the operator's own files. The
/// repo config sits in the read-write workspace, so an agent can edit it for
/// the next session; a repo `[workspace] host-path`, `[[workspace.mounts]]` or
/// sidecar mount covering the home directory (or one above it), or read-write
/// covering the global config file, its directory, or a directory its path is
/// looked up through (see [`GlobalConfigReach`]), would let that agent
/// rewrite the global config the repo is not trusted to carry. A
/// global-declared entry is the operator's own choice, and a hand-built one
/// (`--volume`) carries no source. Neither does an undeclared workspace, which is
/// outrig's own pick: [`refuse_workspace_covering_global_config`] asks about it.
///
/// What passes is *pinned*: each checked entry's host path is replaced by the
/// canonical path the check saw, so a symlink the agent swaps afterwards
/// cannot redirect the bind (#604). A component that is replaced outright
/// is what [`recheck_pinned_mounts`] catches, just before the container starts.
///
/// The pin changes where a directory is mounted from, never where the
/// container shows it. A mount whose config left out its container path
/// appears at its host path as written, so that path is declared before the
/// host path is pinned. So is the workspace's when the pin moves it, the
/// config has a binding, and `starts_bindings` says the launch starts the
/// config's bindings, which put the workspace at its host path. The harness
/// accepts a container path that resolves on the host to the directory
/// mounted there, so a binding handed the path as written names files the
/// agent can open.
pub(crate) fn pin_repo_declared_mounts(
    cfg: &mut Config,
    repo_root: &Path,
    global_cfg: Option<&Path>,
    starts_bindings: bool,
) -> Result<Vec<PinnedMount>> {
    let home = BaseDirs::new().map(|d| d.home_dir().to_path_buf());
    pin_repo_declared_mounts_with(cfg, repo_root, home.as_deref(), global_cfg, starts_bindings)
}

/// [`pin_repo_declared_mounts`] against an explicit `home` (`None` skips the
/// home rule). Paths are compared canonicalized, as in
/// [`refuse_home_workspace_with`].
fn pin_repo_declared_mounts_with(
    cfg: &mut Config,
    repo_root: &Path,
    home: Option<&Path>,
    global_cfg: Option<&Path>,
    starts_bindings: bool,
) -> Result<Vec<PinnedMount>> {
    let home = home.map(canonical_or_normalized);
    let global = global_cfg.map(GlobalConfigReach::of);
    let mut pinned = Vec::new();
    let mut check = |what: &str,
                     source: Option<&ConfigSource>,
                     host: PathBuf,
                     access: MountAccess|
     -> Result<Option<PathBuf>> {
        let Some(source) = source.filter(|s| !matches!(s, ConfigSource::Global { .. })) else {
            return Ok(None);
        };
        let host = canonical_or_normalized(&host);
        let reason = if let Some(why) =
            reaches_the_operators_files(&host, access, home.as_deref(), global.as_ref())
        {
            why
        } else if host.to_str().is_none() {
            // The bind is serialized through `display()`, which would name a
            // different directory than the one checked.
            "it resolves to a path that is not valid UTF-8, which cannot be bound faithfully"
                .to_string()
        } else {
            pinned.push(PinnedMount {
                what: what.to_string(),
                host: host.clone(),
                source: source.config_path(),
            });
            return Ok(Some(host));
        };
        Err(repo_declared_refusal(what, &host, source, &reason))
    };
    let bound = starts_bindings && !cfg.bindings.is_empty();
    let workspace = &mut cfg.workspace;
    if let Some(host) = check(
        "workspace host-path",
        workspace.config_source(),
        workspace.resolved_host_path(repo_root),
        MountAccess::ReadWrite,
    )? {
        // Only a moved path: with every binding refused, the harness leaves
        // the workspace at `/workspace`, and one the pin did not move stays.
        let shown = lexical(&workspace.resolved_host_path(repo_root));
        if bound && workspace.declared_container_path().is_none() && shown != host {
            workspace.set_container_path(shown);
        }
        workspace.set_host_path(host);
    }
    for m in &mut workspace.mounts {
        if let Some(host) = check(
            "mount",
            m.config_source(),
            m.resolved_host_path(repo_root),
            m.access(),
        )? {
            m.set_container_path(m.resolved_container_path(repo_root));
            m.set_host_path(host);
        }
    }
    for (name, sc) in &mut cfg.sidecars {
        let what = format!("sidecar {name:?} mount");
        for m in &mut sc.mounts {
            if let Some(host) = check(
                &what,
                m.config_source(),
                m.resolved_host_path(repo_root),
                m.access(),
            )? {
                m.set_container_path(m.resolved_container_path(repo_root));
                m.set_host_path(host);
            }
        }
    }
    Ok(pinned)
}

/// Why a repo-declared host path may not be mounted: it reaches the
/// operator's own files. `host` and `home` are both canonical, as the home
/// rule compares them; only a read-write mount is asked about the global
/// config, since a read-only one cannot rewrite it.
fn reaches_the_operators_files(
    host: &Path,
    access: MountAccess,
    home: Option<&Path>,
    global: Option<&GlobalConfigReach>,
) -> Option<String> {
    if home.is_some_and(|home| home.starts_with(host)) {
        return Some("it is your home directory or above it".to_string());
    }
    if access != MountAccess::ReadWrite {
        return None;
    }
    global.and_then(|global| global.rewritable_through(host))
}

/// The refusal of `what` at `host`, which `source` declared, for `reason`:
/// one wording for every launch that holds a repo config to these rules.
fn repo_declared_refusal(
    what: &str,
    host: &Path,
    source: &ConfigSource,
    reason: &str,
) -> OutrigError {
    OutrigError::Configuration(format!(
        "refusing {what} {} declared in {}: {reason}, so an agent could edit files \
         that govern the next session\n\
         help: a repo config may not do this; declare it in the global config if you \
         mean it, or pass --volume",
        host.display(),
        source.config_path().display()
    ))
}

/// Refuse a repo-declared host path that reaches the operator's own files,
/// for a launch that mounts none of them itself: `outrig run-new
/// <script.py>`, whose sessions are the host program's and mount what it asks
/// for, never the configuration's workspace or its mounts.
///
/// The program is shown both, as `outrig.config()`, and mounting what the
/// configuration names is one thing a program is written to do -- so a repo
/// config that names the home directory, or the global config read-write,
/// reaches the next session's agent through it. What a repository declares is
/// held to [`pin_repo_declared_mounts`]' rules, with its wording, before the
/// program starts (#646).
///
/// Nothing is pinned, and a path that is not UTF-8 is not refused: outrig
/// binds none of these paths, so there is no bind to hold to the path checked
/// nor one to render it. A program that mounts a path it was shown names it
/// itself, and the session it asks for checks what it named.
pub(crate) fn refuse_repo_declared_mounts(
    cfg: &Config,
    repo_root: &Path,
    global_cfg: Option<&Path>,
) -> Result<()> {
    let home = BaseDirs::new().map(|d| d.home_dir().to_path_buf());
    refuse_repo_declared_mounts_with(cfg, repo_root, home.as_deref(), global_cfg)
}

/// [`refuse_repo_declared_mounts`] against an explicit `home` (`None` skips
/// the home rule), as [`pin_repo_declared_mounts_with`] is.
fn refuse_repo_declared_mounts_with(
    cfg: &Config,
    repo_root: &Path,
    home: Option<&Path>,
    global_cfg: Option<&Path>,
) -> Result<()> {
    let home = home.map(canonical_or_normalized);
    let global = global_cfg.map(GlobalConfigReach::of);
    for (what, source, host, access) in declared_host_paths(cfg, repo_root) {
        let Some(source) = source.filter(|s| !matches!(s, ConfigSource::Global { .. })) else {
            continue;
        };
        let host = canonical_or_normalized(&host);
        if let Some(reason) =
            reaches_the_operators_files(&host, access, home.as_deref(), global.as_ref())
        {
            return Err(repo_declared_refusal(&what, &host, source, &reason));
        }
    }
    Ok(())
}

/// Every host path `cfg` declares for a bind mount -- the workspace's, each
/// `[[workspace.mounts]]` entry's, and each sidecar mount's -- as what a
/// diagnostic calls it, what declared it, where it resolves, and how it would
/// be mounted. The workspace is always read-write.
fn declared_host_paths<'a>(
    cfg: &'a Config,
    repo_root: &'a Path,
) -> impl Iterator<Item = (String, Option<&'a ConfigSource>, PathBuf, MountAccess)> + 'a {
    let workspace = std::iter::once((
        "workspace host-path".to_string(),
        cfg.workspace.config_source(),
        cfg.workspace.resolved_host_path(repo_root),
        MountAccess::ReadWrite,
    ));
    let mounts = cfg.workspace.mounts.iter().map(move |m| {
        (
            "mount".to_string(),
            m.config_source(),
            m.resolved_host_path(repo_root),
            m.access(),
        )
    });
    let sidecars = cfg.sidecars.iter().flat_map(move |(name, sidecar)| {
        sidecar.mounts.iter().map(move |m| {
            (
                format!("sidecar {name:?} mount"),
                m.config_source(),
                m.resolved_host_path(repo_root),
                m.access(),
            )
        })
    });
    workspace.chain(mounts).chain(sidecars)
}

/// `path` with `.` and `..` resolved textually, as the harness places a
/// directory in the container.
fn lexical(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut resolved = PathBuf::new();
    for c in path.components() {
        match c {
            Component::ParentDir => {
                resolved.pop();
            }
            Component::CurDir => {}
            c => resolved.push(c),
        }
    }
    resolved
}

/// Refuse a launch whose pinned mount no longer resolves to the path the guard
/// validated. Run right before the container starts, after the image work that
/// can take minutes: a symlink swap is already defeated by the pin, but an
/// agent replacing a parent directory is seen here.
pub(crate) fn recheck_pinned_mounts(pinned: &[PinnedMount]) -> Result<()> {
    for p in pinned {
        let now = canonical_or_normalized(&p.host);
        if now != p.host {
            return Err(OutrigError::Configuration(format!(
                "refusing {} {} declared in {}: it now resolves to {}, so a component of it \
                 was replaced after it was checked",
                p.what,
                p.host.display(),
                p.source.display(),
                now.display()
            )));
        }
    }
    Ok(())
}

/// Build context for one of outrig's built-in images, under the user cache
/// directory. A subdirectory per image: the content hash covers the whole
/// context, so two built-ins sharing a directory would bust each other's tag.
pub(crate) fn builtin_image_dir(name: &str) -> PathBuf {
    if let Some(dirs) = ProjectDirs::from("", "", "outrig") {
        return dirs.cache_dir().join("builtin-images").join(name);
    }
    std::env::temp_dir()
        .join("outrig-builtin-images")
        .join(name)
}

pub(crate) fn default_session_root() -> PathBuf {
    if let Some(dirs) = ProjectDirs::from("", "", "outrig") {
        return dirs.data_dir().join("sessions");
    }
    std::env::temp_dir().join("outrig-sessions")
}

/// Where the global config is, unchecked: `init` and `config init` write it
/// there, so with nowhere to put it they stop. A command that reads it goes
/// through [`resolve_global_config`].
pub(crate) fn global_config_path(override_path: Option<&Path>) -> Result<PathBuf> {
    locate_global_config(override_path).ok_or_else(|| {
        OutrigError::Configuration(
            "nowhere to put the global config: there is no absolute XDG_CONFIG_HOME and no \
             absolute home directory\n\
             help: pass --global-config <path>, or set HOME to an absolute path"
                .to_string(),
        )
    })
}

/// [`global_config_path`] for a command that reads the file. `--global-config`
/// names a file outright, so one that does not exist is an error, not an empty
/// config for a typo to run on; only the default may be missing. Anything that
/// does exist is left to the read, which reports a directory or a file it
/// cannot open, and reads `/dev/null` as empty. Call it where the file is about
/// to be read: a command that never reads it never checks the flag.
///
/// `None` is no global config at all: with no flag, no absolute
/// `XDG_CONFIG_HOME`, and no absolute home directory there is no default
/// location, and the command runs as it does when the default file is missing.
pub(crate) fn resolve_global_config(override_path: Option<&Path>) -> Result<Option<PathBuf>> {
    if let Some(path) = override_path
        && matches!(path.try_exists(), Ok(false))
    {
        return Err(OutrigError::Configuration(format!(
            "--global-config {} does not exist",
            path.display()
        )));
    }
    Ok(locate_global_config(override_path))
}

/// The global config at `path` alone, as [`Config::load_global`] reads it, or
/// an empty one when there is no global config.
pub(crate) fn load_global_or_empty(path: Option<&Path>) -> Result<Config> {
    path.map_or_else(|| Ok(Config::default()), Config::load_global)
}

/// What [`global_config_path`] and [`resolve_global_config`] share: where the
/// global config is -- the flag, else what the environment names -- or `None`
/// when nothing names a place.
fn locate_global_config(override_path: Option<&Path>) -> Option<PathBuf> {
    let xdg = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from);
    let home = BaseDirs::new().map(|b| b.home_dir().to_path_buf());
    global_config_path_with(override_path, xdg.as_deref(), home.as_deref())
}

/// [`locate_global_config`] over explicit values: `--global-config`, else
/// `<xdg_config_home>/outrig/config.toml`, else `<home>/.outrig/config.toml`.
///
/// The flag is taken as given, a relative one included, as `--config` is. An
/// `XDG_CONFIG_HOME` or home that is not absolute is no location: joined onto
/// the working directory, it would make the global config -- the file that may
/// declare what a repo config may not, such as a provider's `api-key` --
/// whichever file the directory holds. The XDG Base Directory Specification has
/// an implementation ignore a relative `XDG_CONFIG_HOME` and read an empty one
/// as unset.
fn global_config_path_with(
    override_path: Option<&Path>,
    xdg_config_home: Option<&Path>,
    home: Option<&Path>,
) -> Option<PathBuf> {
    if let Some(p) = override_path {
        return Some(p.to_path_buf());
    }
    if let Some(xdg) = xdg_config_home.filter(|p| p.is_absolute()) {
        return Some(xdg.join(GLOBAL_XDG_DIR).join(GLOBAL_CONFIG_FILE));
    }
    home.filter(|p| p.is_absolute())
        .map(|home| home.join(GLOBAL_HOME_DIR).join(GLOBAL_CONFIG_FILE))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    use tempfile::tempdir;

    fn write_repo_config(root: &Path) -> PathBuf {
        let agents = root.join(".agents").join("outrig");
        fs::create_dir_all(&agents).unwrap();
        let config = agents.join("config.toml");
        fs::write(&config, b"# fixture\n").unwrap();
        config
    }

    /// A file that can't be staged fails the write before any file changes,
    /// the ones listed ahead of it included, and leaves no temp file behind.
    #[test]
    fn write_atomic_all_changes_nothing_when_a_file_cannot_be_staged() {
        let tmp = tempdir().unwrap();
        let first = tmp.path().join("first");
        fs::write(&first, "old").unwrap();
        // A regular file where the second target's directory would go.
        let blocker = tmp.path().join("blocker");
        fs::write(&blocker, "").unwrap();

        write_atomic_all(&[(&first, "new"), (&blocker.join("second"), "new")])
            .expect_err("the second file's directory can't be created");

        assert_eq!(fs::read_to_string(&first).unwrap(), "old");
        let mut left: Vec<_> = fs::read_dir(tmp.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        left.sort();
        assert_eq!(left, ["blocker", "first"]);
    }

    /// What [`write_atomic_follows_the_umask_and_keeps_an_existing_mode`]
    /// runs in a child process with its own umask: write `fresh` (new) and
    /// `existing` (a `0640` file) under `$OUTRIG_UMASK_DIR`. Does nothing
    /// otherwise.
    #[test]
    fn umask_child_writes_files() {
        let Some(dir) = std::env::var_os("OUTRIG_UMASK_DIR").map(PathBuf::from) else {
            return;
        };
        write_atomic_all(&[(&dir.join("fresh"), "new"), (&dir.join("existing"), "new")]).unwrap();
    }

    /// #405: a written file follows the umask like any other the user
    /// creates, and a forced rewrite keeps the mode the file already had. The
    /// writes run in a child whose umask is its own: the umask is
    /// process-wide, and other tests in this binary create files (and set it)
    /// concurrently.
    #[test]
    fn write_atomic_follows_the_umask_and_keeps_an_existing_mode() {
        use std::os::unix::process::CommandExt as _;
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        for (umask, fresh) in [(0o077, 0o600), (0o022, 0o644), (0o002, 0o664)] {
            let tmp = tempdir().unwrap();
            let existing = tmp.path().join("existing");
            fs::write(&existing, "old").unwrap();
            fs::set_permissions(&existing, Permissions::from_mode(0o640)).unwrap();

            let mut child = std::process::Command::new(std::env::current_exe().unwrap());
            child
                .args(["--exact", "paths::tests::umask_child_writes_files"])
                .env("OUTRIG_UMASK_DIR", tmp.path());
            // SAFETY: `umask` is async-signal-safe, as `pre_exec` requires.
            unsafe {
                child.pre_exec(move || {
                    nix::sys::stat::umask(nix::sys::stat::Mode::from_bits_truncate(umask));
                    Ok(())
                });
            }
            assert!(child.status().unwrap().success());

            assert_eq!(mode(&tmp.path().join("fresh")), fresh, "umask {umask:o}");
            assert_eq!(mode(&existing), 0o640, "umask {umask:o}");
            assert_eq!(fs::read_to_string(&existing).unwrap(), "new");
        }
    }

    /// An existing file's access ACL is what the rewrite has, not what the
    /// directory's default ACL would hand a new file: no named entry appears
    /// that the target never had.
    #[test]
    fn a_rewrite_does_not_inherit_the_directory_default_acl() {
        let tmp = tempdir().unwrap();
        let existing = tmp.path().join("existing");
        fs::write(&existing, "old").unwrap();
        fs::set_permissions(&existing, Permissions::from_mode(0o640)).unwrap();
        let set = std::process::Command::new("setfacl")
            .args(["-d", "-m", "u:1:r"])
            .arg(tmp.path())
            .status();
        if !set.is_ok_and(|s| s.success()) {
            return; // no setfacl, or a filesystem without ACLs
        }

        write_atomic(&existing, "new").unwrap();

        let acl = std::process::Command::new("getfacl")
            .arg("-nc")
            .arg(&existing)
            .output()
            .unwrap();
        assert!(acl.status.success(), "getfacl failed");
        let acl = String::from_utf8(acl.stdout).unwrap();
        assert!(!acl.contains("user:1:"), "{acl}");
        assert_eq!(fs::metadata(&existing).unwrap().permissions().mode() & 0o777, 0o640);
    }

    /// An ACL past the old fixed buffer (600 named users is over 4096 bytes)
    /// is copied whole, not a reason to refuse the rewrite.
    #[test]
    fn a_rewrite_copies_an_acl_larger_than_4096_bytes() {
        let tmp = tempdir().unwrap();
        let existing = tmp.path().join("existing");
        fs::write(&existing, "old").unwrap();
        let users = (1000..1600).map(|u| format!("u:{u}:r")).collect::<Vec<_>>();
        let set = std::process::Command::new("setfacl")
            .args(["-m", &users.join(",")])
            .arg(&existing)
            .status();
        if !set.is_ok_and(|s| s.success()) {
            return; // no setfacl, or a filesystem without ACLs
        }
        let getfacl = |p: &Path| {
            let out = std::process::Command::new("getfacl")
                .arg("-nc")
                .arg(p)
                .output()
                .unwrap();
            assert!(out.status.success(), "getfacl failed");
            out.stdout
        };
        let before = getfacl(&existing);

        write_atomic(&existing, "new").unwrap();

        assert_eq!(getfacl(&existing), before);
    }

    /// The group is part of who a rewrite's mode grants to: the temp file
    /// would take the directory's, so the target's own is put back.
    #[test]
    fn a_rewrite_keeps_the_existing_files_group() {
        let own = nix::unistd::getegid();
        let Some(other) = nix::unistd::getgroups()
            .unwrap()
            .into_iter()
            .find(|g| *g != own)
        else {
            return; // no supplementary group to give the file
        };
        let tmp = tempdir().unwrap();
        let existing = tmp.path().join("existing");
        fs::write(&existing, "old").unwrap();
        std::os::unix::fs::chown(&existing, None, Some(other.as_raw())).unwrap();

        write_atomic(&existing, "new").unwrap();

        assert_eq!(fs::metadata(&existing).unwrap().gid(), other.as_raw());
    }

    #[test]
    fn find_repo_root_missing_returns_documented_error() {
        let tmp = tempdir().unwrap();
        let err = find_repo_root_from(tmp.path()).unwrap_err();
        assert!(matches!(err, OutrigError::NoRepoConfig));
        assert_eq!(
            err.to_string(),
            "no .agents/outrig/config.toml found in current directory or any parent\n\
             help: run `outrig init` to initialize",
        );
    }

    #[test]
    fn find_repo_root_from_nested_cwd_finds_parent_config() {
        let tmp = tempdir().unwrap();
        write_repo_config(tmp.path());
        let nested = tmp.path().join("a/b/c");
        fs::create_dir_all(&nested).unwrap();

        let root = find_repo_root_from(&nested).unwrap();
        assert_eq!(root, tmp.path());
    }

    #[test]
    fn find_repo_root_from_root_dir_finds_config() {
        let tmp = tempdir().unwrap();
        write_repo_config(tmp.path());

        let root = find_repo_root_from(tmp.path()).unwrap();
        assert_eq!(root, tmp.path());
        assert_eq!(
            repo_config_path(&root),
            tmp.path().join(".agents/outrig/config.toml"),
        );
    }

    #[test]
    fn find_repo_root_skips_agents_dir_without_outrig_config() {
        let tmp = tempdir().unwrap();
        write_repo_config(tmp.path());

        let bare = tmp.path().join("a");
        fs::create_dir_all(bare.join(".agents")).unwrap();
        let nested = bare.join("b/c");
        fs::create_dir_all(&nested).unwrap();

        let root = find_repo_root_from(&nested).unwrap();
        assert_eq!(root, tmp.path());
    }

    /// #829: `.agents/outrig/` the user cannot search must stop the walk
    /// rather than be read as absent -- a config further up, or none, picked
    /// silently in place of one the walk never examined.
    #[test]
    fn find_repo_root_reports_a_config_it_cannot_examine() {
        let tmp = tempdir().unwrap();
        write_repo_config(tmp.path()); // a config further up, of the caller's own
        let proj = tmp.path().join("proj");
        let agents = proj.join(".agents").join("outrig");
        fs::create_dir_all(&agents).unwrap();
        fs::write(agents.join("config.toml"), b"# fixture
").unwrap();
        let nested = proj.join("sub");
        fs::create_dir_all(&nested).unwrap();
        fs::set_permissions(&agents, Permissions::from_mode(0o000)).unwrap();
        // A superuser searches any directory, so there is nothing to see.
        let searchable = fs::metadata(agents.join("config.toml")).is_ok();

        let result = find_repo_root_from(&nested);

        fs::set_permissions(&agents, Permissions::from_mode(0o755)).unwrap();
        if searchable {
            return;
        }
        let err = result.unwrap_err();
        assert!(
            matches!(&err, OutrigError::Path { op: "stat", path, .. } if *path == agents.join("config.toml")),
            "{err:?}"
        );
        assert!(err.to_string().contains("Permission denied"), "{err}");
    }

    /// A regular file named `.agents` is not the directory the config lives
    /// in, but it is not absence either: the walk goes on past it rather
    /// than erroring or stopping short.
    #[test]
    fn find_repo_root_skips_a_agents_entry_that_is_a_regular_file() {
        let tmp = tempdir().unwrap();
        write_repo_config(tmp.path());

        let bare = tmp.path().join("a");
        fs::create_dir_all(&bare).unwrap();
        fs::write(bare.join(".agents"), b"not a directory
").unwrap();
        let nested = bare.join("b/c");
        fs::create_dir_all(&nested).unwrap();

        let root = find_repo_root_from(&nested).unwrap();
        assert_eq!(root, tmp.path());
    }

    /// #329: a config the walk finds is taken only from its owner. To anyone
    /// else -- the victim below a config planted in `/tmp` -- it is an error
    /// naming the file, not a config to run on and not a missing one to run
    /// config-less past.
    #[test]
    fn find_repo_root_refuses_a_config_another_user_owns() {
        let tmp = tempdir().unwrap();
        let config = write_repo_config(tmp.path());
        let nested = tmp.path().join("victim/scratch");
        fs::create_dir_all(&nested).unwrap();
        let owner = fs::metadata(&config).unwrap().uid();

        assert_eq!(find_repo_root_with(&nested, owner).unwrap(), tmp.path());

        let other = owner + 1;
        let err = find_repo_root_with(&nested, other).unwrap_err();
        assert!(
            matches!(err, OutrigError::Configuration(_)),
            "a refusal, not a missing config: {err:?}"
        );
        assert_eq!(
            err.to_string(),
            format!(
                "configuration: refusing {config}: it is owned by uid {owner}, not by you \
                 (uid {other}), so another user could have written it\n\
                 help: run `outrig init` where you are to start a config of your own; \
                 outrig reads a config you do not own only when --config names it",
                config = config.display(),
            ),
        );
    }

    /// `--config <repo>/.agents/outrig/config.toml` is that repo, wherever the
    /// command runs from: the documented way to point an MCP client at a repo.
    #[test]
    fn explicit_repo_config_is_its_own_repo() {
        let repo = tempdir().unwrap();
        let config = write_repo_config(repo.path());
        let elsewhere = tempdir().unwrap();

        for resolve in [resolve_repo_config, resolve_repo_config_optional] {
            let resolved = resolve(Some(&config), elsewhere.path()).unwrap();
            assert_eq!(resolved, RepoConfig::at_root(repo.path().to_path_buf()));
            assert_eq!(resolved.config_path(), config);
        }
    }

    /// Any other `--config` file is read in place of the repo's own, and the
    /// repo is the one found from the working directory -- not the directory
    /// three levels above the file, which is where #323's `$HOME` came from.
    #[test]
    fn explicit_other_file_is_read_for_the_repo_found_from_cwd() {
        let repo = tempdir().unwrap();
        write_repo_config(repo.path());
        let nested = repo.path().join("a/b");
        fs::create_dir_all(&nested).unwrap();
        let outside = tempdir().unwrap();
        let file = outside.path().join("deep/ci/outrig.toml");
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(&file, b"# fixture\n").unwrap();
        // Where the old derivation looked: three levels above the file.
        write_repo_config(outside.path());

        for resolve in [resolve_repo_config, resolve_repo_config_optional] {
            let resolved = resolve(Some(&file), &nested).unwrap();
            assert_eq!(
                resolved,
                RepoConfig {
                    root: repo.path().to_path_buf(),
                    file: Some(file.clone()),
                },
            );
            assert_eq!(resolved.config_path(), file);
        }
    }

    /// Outside any repo, an out-of-tree `--config` runs against the working
    /// directory, as a config-less run would.
    #[test]
    fn explicit_other_file_outside_a_repo_runs_against_cwd() {
        let cwd = tempdir().unwrap();
        let outside = tempdir().unwrap();
        let file = outside.path().join("outrig.toml");
        fs::write(&file, b"# fixture\n").unwrap();

        for resolve in [resolve_repo_config, resolve_repo_config_optional] {
            let resolved = resolve(Some(&file), cwd.path()).unwrap();
            assert_eq!(resolved.root, cwd.path());
            assert_eq!(resolved.file.as_deref(), Some(file.as_path()));
        }
    }

    /// An explicit `--config` names a file outright, so one that is not there
    /// -- a typo, or a directory -- stops the command and says which, in
    /// either shape and even with a repo config to fall back to.
    #[test]
    fn explicit_config_that_is_not_a_file_is_refused() {
        let repo = tempdir().unwrap();
        write_repo_config(repo.path());
        for missing in [
            repo.path().join(".agents/outrig/confg.toml"),
            repo.path().join("sub/.agents/outrig/config.toml"),
            repo.path().join("ci/outrig.toml"),
            repo.path().join(".agents/outrig"),
        ] {
            for resolve in [resolve_repo_config, resolve_repo_config_optional] {
                let err = resolve(Some(&missing), repo.path()).unwrap_err();
                assert_eq!(
                    err.to_string(),
                    format!(
                        "configuration: --config {} is not an existing file",
                        missing.display()
                    ),
                );
            }
        }
    }

    /// A `--config` whose `stat` fails for a reason other than absence is not
    /// "not an existing file": that sends the user looking for a file that is
    /// there. The error names the path and the cause instead.
    #[test]
    fn explicit_config_that_cannot_be_examined_reports_why() {
        let repo = tempdir().unwrap();
        write_repo_config(repo.path());
        let locked = repo.path().join("locked");
        fs::create_dir(&locked).unwrap();
        let file = locked.join("outrig.toml");
        fs::write(&file, b"# fixture\n").unwrap();
        fs::set_permissions(&locked, Permissions::from_mode(0o000)).unwrap();
        // A superuser searches any directory, so there is nothing to see.
        let searchable = fs::metadata(&file).is_ok();

        let errs: Vec<_> = [resolve_repo_config, resolve_repo_config_optional]
            .map(|resolve| resolve(Some(&file), repo.path()))
            .into();
        fs::set_permissions(&locked, Permissions::from_mode(0o755)).unwrap();
        if searchable {
            return;
        }
        for err in errs {
            let msg = err.unwrap_err().to_string();
            assert!(
                msg.starts_with(&format!(
                    "configuration: --config {} cannot be read: ",
                    file.display()
                )) && msg.contains("Permission denied"),
                "{msg}"
            );
        }
    }

    /// Without the flag, nothing changes: the walk up decides, and a run or
    /// mcp session with nothing to find is config-less in the working
    /// directory.
    #[test]
    fn no_flag_walks_up_or_falls_back_to_cwd() {
        let repo = tempdir().unwrap();
        write_repo_config(repo.path());
        let nested = repo.path().join("a");
        fs::create_dir_all(&nested).unwrap();
        let bare = tempdir().unwrap();

        let expected = RepoConfig::at_root(repo.path().to_path_buf());
        assert_eq!(resolve_repo_config(None, &nested).unwrap(), expected);
        assert_eq!(resolve_repo_config_optional(None, &nested).unwrap(), expected);
        assert!(matches!(
            resolve_repo_config(None, bare.path()),
            Err(OutrigError::NoRepoConfig)
        ));
        assert_eq!(
            resolve_repo_config_optional(None, bare.path()).unwrap(),
            RepoConfig::at_root(bare.path().to_path_buf()),
        );
    }

    /// A relative `--config` is taken against the working directory, so the
    /// root is absolute in every shape: never the empty path three levels
    /// above `.agents/outrig/config.toml`, which would leave every path
    /// resolved against it relative.
    #[test]
    fn relative_explicit_config_resolves_against_cwd() {
        let repo = tempdir().unwrap();
        write_repo_config(repo.path());
        fs::create_dir_all(repo.path().join("ci")).unwrap();
        fs::write(repo.path().join("ci/outrig.toml"), b"# fixture\n").unwrap();

        for canonical in [".agents/outrig/config.toml", "./.agents/outrig/config.toml"] {
            let resolved = resolve_repo_config(Some(Path::new(canonical)), repo.path()).unwrap();
            assert_eq!(resolved.root, repo.path(), "--config {canonical}");
            assert_eq!(resolved.root.as_os_str(), repo.path().as_os_str());
        }
        let resolved = resolve_repo_config(Some(Path::new("ci/outrig.toml")), repo.path()).unwrap();
        assert_eq!(resolved.file, Some(repo.path().join("ci/outrig.toml")));
    }

    /// The home directory and every directory above it are refused as a
    /// default workspace, however they are spelled; anything below or beside
    /// it is not.
    #[test]
    fn home_and_its_ancestors_are_refused_as_a_default_workspace() {
        let tmp = tempdir().unwrap();
        let home = tmp.path().join("home/u");
        let proj = home.join("proj");
        fs::create_dir_all(&proj).unwrap();
        let elsewhere = tmp.path().join("srv");
        fs::create_dir_all(&elsewhere).unwrap();
        let link = tmp.path().join("link-to-home");
        std::os::unix::fs::symlink(&home, &link).unwrap();

        for refused in [
            home.clone(),
            home.join("."),
            proj.join(".."),
            link,
            tmp.path().join("home"),
            PathBuf::from("/"),
        ] {
            let err = refuse_home_workspace_with(&refused, &home, &refused).unwrap_err();
            assert!(
                err.to_string().contains("refusing to mount"),
                "{} must be refused: {err}",
                refused.display(),
            );
        }
        for allowed in [&proj, &elsewhere] {
            refuse_home_workspace_with(allowed, &home, allowed)
                .unwrap_or_else(|e| panic!("{} must be allowed: {e}", allowed.display()));
        }
    }

    /// An absent path compares by its normalized spelling, so a `..` through
    /// a sibling directory does not make it look like it is inside one.
    #[test]
    fn an_absent_path_is_normalized_before_comparing() {
        let tmp = tempdir().unwrap();
        let root = fs::canonicalize(tmp.path()).unwrap();
        fs::create_dir_all(root.join("cache")).unwrap();
        let absent = root.join("cache/../conf/config.toml");
        let got = canonical_or_normalized(&absent);
        assert_eq!(got, root.join("conf/config.toml"));
        assert!(!got.starts_with(root.join("cache")));
    }

    /// A `..` after a missing component is applied to the symlink-resolved
    /// prefix, not folded across it.
    #[test]
    fn parent_traversal_through_a_missing_component_follows_earlier_symlinks() {
        let tmp = tempdir().unwrap();
        let root = fs::canonicalize(tmp.path()).unwrap();
        fs::create_dir_all(root.join("cache")).unwrap();
        fs::create_dir_all(root.join("private/sub")).unwrap();
        std::os::unix::fs::symlink(root.join("private/sub"), root.join("cache/link")).unwrap();
        let spelled = root.join("cache/link/absent/../../conf/outrig/config.toml");
        let got = canonical_or_normalized(&spelled);
        assert_eq!(got, root.join("private/conf/outrig/config.toml"));
        assert!(!got.starts_with(root.join("cache")));
    }

    /// `pin_repo_declared_mounts_with` for a repo config, under `root`,
    /// declaring one mount of `host`, against a `global` config path and no
    /// home rule. The refusal's text, if there is one.
    fn refusal(root: &Path, host: &Path, access: &str, global: &Path) -> Option<String> {
        let repo = root.join("repo");
        let file = repo_config_path(&repo);
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(
            &file,
            format!(
                "[[workspace.mounts]]\nhost-path = \"{}\"\ncontainer-path = \"/g\"\n\
                 access = \"{access}\"\n",
                host.display()
            ),
        )
        .unwrap();
        let mut cfg = Config::load_unvalidated(&repo, None).unwrap();
        pin_repo_declared_mounts_with(&mut cfg, &repo, None, Some(global), false)
            .err()
            .map(|e| e.to_string())
    }

    /// A repo config declaring one read-write mount of `host`, loaded under
    /// `root`, and the pin of it against a global config path.
    fn pin(root: &Path, host: &Path, global: &Path) -> (Config, Vec<PinnedMount>) {
        let repo = root.join("repo");
        let file = repo_config_path(&repo);
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(
            &file,
            format!(
                "[[workspace.mounts]]\nhost-path = \"{}\"\ncontainer-path = \"/g\"\n",
                host.display()
            ),
        )
        .unwrap();
        let mut cfg = Config::load_unvalidated(&repo, None).unwrap();
        let pinned =
            pin_repo_declared_mounts_with(&mut cfg, &repo, None, Some(global), false).unwrap();
        (cfg, pinned)
    }

    /// Pinning installs the canonical bytes, and the bind renders them through
    /// `display()`: a target whose name is not UTF-8 would be mounted as its
    /// U+FFFD spelling, which a repo can make a link to home. Refused.
    #[test]
    fn a_repo_mount_resolving_to_a_non_utf8_path_is_refused() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;
        let tmp = tempdir().unwrap();
        let root = fs::canonicalize(tmp.path()).unwrap();
        let odd = root.join(OsStr::from_bytes(b"odd\xff"));
        fs::create_dir_all(&odd).unwrap();
        let link = root.join("link");
        std::os::unix::fs::symlink(&odd, &link).unwrap();
        let global = root.join("conf/outrig/config.toml");

        let err = refusal(&root, &link, "read-only", &global).expect("non-UTF-8 target");
        assert!(err.contains("not valid UTF-8"), "{err}");
    }

    /// The path the guard validated is the path that is mounted: a symlink
    /// swapped after the check cannot redirect it, and a replaced component
    /// is refused at start (#604).
    #[test]
    fn a_pinned_repo_mount_survives_a_symlink_swap_and_refuses_a_replaced_parent() {
        let tmp = tempdir().unwrap();
        let root = fs::canonicalize(tmp.path()).unwrap();
        let (safe, other) = (root.join("safe"), root.join("other"));
        fs::create_dir_all(&safe).unwrap();
        fs::create_dir_all(&other).unwrap();
        let link = root.join("link");
        std::os::unix::fs::symlink(&safe, &link).unwrap();
        let global = root.join("conf/outrig/config.toml");

        let (cfg, pinned) = pin(&root, &link, &global);
        let repo = root.join("repo");
        assert_eq!(cfg.workspace.mounts[0].resolved_host_path(&repo), safe);
        recheck_pinned_mounts(&pinned).unwrap();

        // The symlink now points elsewhere; the pin still names `safe`.
        fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink(&other, &link).unwrap();
        assert_eq!(cfg.workspace.mounts[0].resolved_host_path(&repo), safe);
        recheck_pinned_mounts(&pinned).unwrap();

        // The pinned directory itself is replaced by a link elsewhere.
        fs::remove_dir(&safe).unwrap();
        std::os::unix::fs::symlink(&other, &safe).unwrap();
        let err = recheck_pinned_mounts(&pinned).unwrap_err().to_string();
        assert!(err.contains(&other.display().to_string()), "{err}");
    }

    /// The pin moves where a directory is mounted from, never where the
    /// container shows it: a mount stays at the container path its config gave
    /// or defaulted to, and so, in a launch that starts bindings, does a
    /// workspace the pin moved.
    #[test]
    fn a_pinned_mount_appears_where_the_config_put_it() {
        let tmp = tempdir().unwrap();
        let root = fs::canonicalize(tmp.path()).unwrap();
        let [ws, data, more] = ["ws", "data", "more"].map(|name| root.join(name));
        let [ws_link, data_link, more_link] =
            ["ws-link", "data-link", "more-link"].map(|name| root.join(name));
        for (real, link) in [(&ws, &ws_link), (&data, &data_link), (&more, &more_link)] {
            fs::create_dir_all(real).unwrap();
            std::os::unix::fs::symlink(real, link).unwrap();
        }
        let repo = root.join("repo");
        let file = repo_config_path(&repo);
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        let global = root.join("conf/outrig/config.toml");
        let binding = "[bindings.x]\ndescription = \"d\"\nfactory = \"m:f\"\n";
        let pinned = |workspace: &Path, binding: &str, starts_bindings: bool| {
            fs::write(
                &file,
                format!(
                    "[workspace]\nhost-path = \"{0}\"\n\
                     [[workspace.mounts]]\nhost-path = \"{1}\"\ncontainer-path = \"{1}\"\n\
                     [[workspace.mounts]]\nhost-path = \"{2}\"\n{binding}",
                    workspace.display(),
                    data_link.display(),
                    more_link.display()
                ),
            )
            .unwrap();
            let mut cfg = Config::load_unvalidated(&repo, None).unwrap();
            pin_repo_declared_mounts_with(&mut cfg, &repo, None, Some(&global), starts_bindings)
                .unwrap();
            cfg.workspace
        };

        // The workspace is at `/workspace` in a launch that ignores bindings
        // or with no binding to start; one the pin did not move is left to
        // the harness.
        for (written, binding, starts_bindings, shown) in [
            (&ws_link, binding, true, Some(ws_link.as_path())),
            (&ws_link, binding, false, None),
            (&ws_link, "", true, None),
            (&ws, binding, true, None),
        ] {
            let workspace = pinned(written, binding, starts_bindings);
            assert_eq!(workspace.resolved_host_path(&repo), ws);
            assert_eq!(workspace.declared_container_path(), shown);
            let mounts = [(&data, &data_link), (&more, &more_link)];
            for (mount, (real, link)) in workspace.mounts.iter().zip(mounts) {
                assert_eq!(mount.resolved_host_path(&repo), *real);
                assert_eq!(mount.declared_container_path(), Some(link.as_path()));
            }
        }
    }

    /// A host program's launch mounts nothing the configuration names, but is
    /// shown all of it, so a repo-declared path is held to the same rules,
    /// refused in the same words, and never pinned (#646). A path that is not
    /// UTF-8 is the pin's own rule: nothing renders a bind of it here.
    #[test]
    fn the_program_guard_refuses_what_a_repo_may_not_declare_and_pins_nothing() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt as _;

        let tmp = tempdir().unwrap();
        let root = fs::canonicalize(tmp.path()).unwrap();
        let home = root.join("home/u");
        let conf = root.join("conf");
        let data = root.join("data");
        for dir in [&home, &conf, &data] {
            fs::create_dir_all(dir).unwrap();
        }
        let global = conf.join("config.toml");
        let odd = root.join(OsStr::from_bytes(b"odd\xff"));
        fs::create_dir_all(&odd).unwrap();
        let [odd_link, data_link] = [("link", &odd), ("data-link", &data)].map(|(name, real)| {
            let link = root.join(name);
            std::os::unix::fs::symlink(real, &link).unwrap();
            link
        });
        let repo = root.join("repo");
        let file = repo_config_path(&repo);
        fs::create_dir_all(file.parent().unwrap()).unwrap();

        // The refusals of `repo_body`, merged over a `global_body`: the host
        // program's, then the interactive launch's pin, and the config the
        // program's guard left behind.
        let refusals = |repo_body: &str, global_body: &str| {
            fs::write(&file, repo_body).unwrap();
            fs::write(&global, global_body).unwrap();
            let cfg = Config::load_unvalidated(&repo, Some(&global)).unwrap();
            let program = refuse_repo_declared_mounts_with(&cfg, &repo, Some(&home), Some(&global))
                .err()
                .map(|e| e.to_string());
            let mut pinning = cfg.clone();
            let pinned = pin_repo_declared_mounts_with(
                &mut pinning,
                &repo,
                Some(&home),
                Some(&global),
                false,
            )
            .err()
            .map(|e| e.to_string());
            (program, pinned, cfg)
        };
        let mount = |host: &Path, access: &str| {
            format!(
                "[[workspace.mounts]]\nhost-path = \"{}\"\ncontainer-path = \"/g\"\n\
                 access = \"{access}\"\n",
                host.display()
            )
        };

        // The home rule and the global-config rule, in the pin's own words.
        let workspace = format!("[workspace]\nhost-path = \"{}\"\n", home.display());
        let (program, pinned, _) = refusals(&workspace, "");
        assert_eq!(program, pinned);
        let err = program.expect("the home directory");
        assert!(err.contains("it is your home directory or above it"), "{err}");
        assert!(err.contains(&format!("declared in {}", file.display())), "{err}");

        let (program, pinned, _) = refusals(&mount(&conf, "read-write"), "");
        assert_eq!(program, pinned);
        assert!(
            program.expect("the global config").contains("covers the global config"),
            "the global config's directory read-write",
        );
        // Read-only, it rewrites nothing; and the global config declares for
        // the operator, who may name what a repository may not.
        let (program, pinned, _) = refusals(&mount(&conf, "read-only"), "");
        assert_eq!((program, pinned), (None, None));
        let (program, pinned, _) = refusals("", &mount(&home, "read-write"));
        assert_eq!((program, pinned), (None, None));

        // The pin's own rule, about a bind this launch does not make.
        let (program, pinned, _) = refusals(&mount(&odd_link, "read-only"), "");
        assert_eq!(program, None);
        assert!(pinned.expect("non-UTF-8").contains("not valid UTF-8"), "the pin refuses it");

        // Nothing is pinned: the mount still resolves as the config spells it.
        let (program, _, cfg) = refusals(&mount(&data_link, "read-only"), "");
        assert_eq!(program, None);
        assert_eq!(cfg.workspace.mounts[0].resolved_host_path(&repo), data_link);
    }

    /// How a refusal names a directory the global config path is looked up
    /// through.
    fn looked_up_through(dir: &Path) -> String {
        format!("covers {}, which the global config path", dir.display())
    }

    /// A `..` after a missing component is popped without the agent in the
    /// way, yet the next session resolves the path through the kernel: an agent
    /// that makes the component a symlink picks where `config.toml` is read. A
    /// read-write mount of the directory it would be created in is refused
    /// (#605).
    #[test]
    fn a_repo_mount_covering_where_a_missing_component_is_created_is_refused() {
        let tmp = tempdir().unwrap();
        let root = fs::canonicalize(tmp.path()).unwrap();
        let shared = root.join("shared");
        fs::create_dir_all(shared.join("inner")).unwrap();
        let global = shared.join("absent/../../conf/outrig/config.toml");

        let err = refusal(&root, &shared, "read-write", &global).expect("the creation point");
        assert!(err.contains(&looked_up_through(&shared)), "{err}");
        assert!(err.contains(&global.display().to_string()), "{err}");

        assert_eq!(refusal(&root, &shared, "read-only", &global), None);
        // From below the creation point the agent cannot make `absent`.
        assert_eq!(refusal(&root, &shared.join("inner"), "read-write", &global), None);
    }

    /// The same swap works on a symlink that is already there: the agent
    /// replaces it in the directory that holds it, though the path resolves
    /// somewhere else today.
    #[test]
    fn a_repo_mount_covering_a_symlink_in_the_global_config_path_is_refused() {
        let tmp = tempdir().unwrap();
        let root = fs::canonicalize(tmp.path()).unwrap();
        let (links, real) = (root.join("links"), root.join("real"));
        fs::create_dir_all(real.join("outrig")).unwrap();
        fs::write(real.join("outrig/config.toml"), "").unwrap();
        fs::create_dir_all(&links).unwrap();
        std::os::unix::fs::symlink(&real, links.join("cfg")).unwrap();
        let global = links.join("cfg/outrig/config.toml");

        let err = refusal(&root, &links, "read-write", &global).expect("the symlink's directory");
        assert!(err.contains(&looked_up_through(&links)), "{err}");

        // Where the file is today keeps its own reason.
        let err = refusal(&root, &real, "read-write", &global).expect("the resolved file");
        assert!(err.contains("covers the global config ("), "{err}");
    }

    /// A `config.toml` that is itself a symlink is replaced by writing the
    /// directory that holds it, though the file it points at is elsewhere.
    #[test]
    fn a_repo_mount_covering_a_symlinked_global_config_is_refused() {
        let tmp = tempdir().unwrap();
        let root = fs::canonicalize(tmp.path()).unwrap();
        let (links, real) = (root.join("links"), root.join("real"));
        fs::create_dir_all(&real).unwrap();
        fs::write(real.join("config.toml"), "").unwrap();
        fs::create_dir_all(&links).unwrap();
        std::os::unix::fs::symlink(real.join("config.toml"), links.join("config.toml")).unwrap();

        let err = refusal(&root, &links, "read-write", &links.join("config.toml"))
            .expect("the symlink's directory");
        assert!(err.contains(&looked_up_through(&links)), "{err}");
    }

    /// A workspace resolving through a directory whose name carries `:` is
    /// refused for a launch that mounts: podman would read it as the field
    /// separator of the `-v` value and refuse the container after the image
    /// was acquired. Undeclared, the path is outrig's own pick, so the
    /// refusal says which default chose it and what to do about that (#365).
    #[test]
    fn an_undeclared_workspace_resolving_through_a_colon_is_refused() {
        let tmp = tempdir().unwrap();
        let root = tmp.path().join("a:b");
        fs::create_dir_all(&root).unwrap();

        let implicit = Config::load_from_str("").unwrap().workspace;
        let err = refuse_workspace_field_separator(&root, &implicit, &root)
            .expect_err("the colon");
        let err = err.to_string();
        assert!(err.contains(&root.display().to_string()), "{err}");
        assert!(err.contains("`:`"), "{err}");
        assert!(err.contains("no repo config was found"), "names the default: {err}");
        assert!(err.contains("run outrig from a directory"), "advises the cwd: {err}");

        let clean = tmp.path().join("clean");
        fs::create_dir_all(&clean).unwrap();
        refuse_workspace_field_separator(&clean, &implicit, &clean)
            .expect("no colon, nothing to refuse");
    }

    /// A *declared* `host-path` is the config's pick, not outrig's, and a
    /// relative one resolves against the directory of the file that declared
    /// it -- so where outrig was invoked from has nothing to do with it. The
    /// refusal names that file and the key, and advises an edit rather than a
    /// `cd` that could not help.
    #[test]
    fn a_declared_workspace_resolving_through_a_colon_names_its_file() {
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("a:b").join("repo");
        let file = repo_config_path(&repo);
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::create_dir_all(repo.join("proj")).unwrap();
        fs::write(&file, "[workspace]\nhost-path = \"proj\"\n").unwrap();
        let cfg = Config::load_unvalidated(&repo, None).unwrap();
        // Nothing the file spelled holds a `:`, so validation passes it.
        cfg.validate(Some(&repo)).expect("the entry itself is clean");
        let resolved = cfg.workspace.resolved_host_path(&repo);

        let err = refuse_workspace_field_separator(&resolved, &cfg.workspace, &repo)
            .expect_err("the resolved colon");
        let err = err.to_string();
        assert!(err.contains(&resolved.display().to_string()), "{err}");
        assert!(err.contains("host-path proj"), "names the key as written: {err}");
        assert!(err.contains(&file.display().to_string()), "names the file: {err}");
        assert!(
            !err.contains("no repo config was found") && !err.contains("run outrig from"),
            "neither claims the cwd chose it nor advises a cd: {err}"
        );
    }

    /// The same question for the extra mounts, which resolve against the
    /// directory of the file that declared them. The refusal names the entry
    /// as written, the path it resolved to, and that file.
    #[test]
    fn a_mount_resolving_through_a_colon_is_refused() {
        let tmp = tempdir().unwrap();
        // The colon is in the repo root, which the entry resolves against.
        let repo = tmp.path().join("a:b").join("repo");
        let file = repo_config_path(&repo);
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::create_dir_all(repo.join("docs")).unwrap();
        fs::write(
            &file,
            "[[workspace.mounts]]\nhost-path = \"docs\"\ncontainer-path = \"/docs\"\n",
        )
        .unwrap();
        let cfg = Config::load_unvalidated(&repo, None).unwrap();
        // The entry itself spells no colon, so config validation passes it.
        cfg.validate(Some(&repo)).expect("nothing a config file said is malformed");

        let err = refuse_mount_field_separators(&cfg, &repo).expect_err("the resolved colon");
        let err = err.to_string();
        assert!(err.contains("\"docs\"") || err.contains("docs"), "names the entry: {err}");
        assert!(
            err.contains(&repo.join("docs").display().to_string()),
            "names what it resolved to: {err}"
        );
        assert!(err.contains(&file.display().to_string()), "names the file: {err}");
        assert!(err.contains("`:`"), "{err}");
    }

    /// Only the resolved path is this rule's business, so a mount that
    /// resolves cleanly passes however the repo root is spelled.
    #[test]
    fn a_mount_resolving_clean_is_not_refused() {
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let file = repo_config_path(&repo);
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::create_dir_all(repo.join("docs")).unwrap();
        fs::write(
            &file,
            "[[workspace.mounts]]\nhost-path = \"docs\"\ncontainer-path = \"/docs\"\n",
        )
        .unwrap();
        let cfg = Config::load_unvalidated(&repo, None).unwrap();
        refuse_mount_field_separators(&cfg, &repo).expect("no colon anywhere");
    }

    /// The refusal says which default put the workspace there: a repo config
    /// at the root, or no repo config at all.
    #[test]
    fn home_refusal_names_the_default_that_chose_it() {
        let tmp = tempdir().unwrap();
        let home = tmp.path().join("home/u");
        fs::create_dir_all(&home).unwrap();

        let err = refuse_home_workspace_with(&home, &home, &home).unwrap_err();
        assert_eq!(
            err.to_string(),
            format!(
                "configuration: refusing to mount {} as the workspace: it is your home \
                 directory, and no repo config was found, so it is the current directory\n\
                 help: run outrig from a project directory, or declare [workspace] host-path \
                 to mount it on purpose",
                home.display()
            ),
        );

        let config = write_repo_config(&home);
        let parent = tmp.path().join("home");
        let err = refuse_home_workspace_with(&home, &home, &home).unwrap_err();
        assert!(
            err.to_string()
                .contains(&format!("{} makes it the repo root", config.display())),
            "{err}",
        );
        let err = refuse_home_workspace_with(&parent, &home, &parent).unwrap_err();
        assert!(
            err.to_string()
                .contains(&format!("it is above your home directory ({})", home.display())),
            "{err}",
        );
    }

    /// [`refuse_workspace_covering_global_config`] for `workspace` against
    /// `global`, the repo root being `workspace` too, as it is for an undeclared
    /// workspace. The refusal's text, if there is one.
    fn workspace_refusal(workspace: &Path, global: Option<&Path>) -> Option<String> {
        refuse_workspace_covering_global_config(workspace, workspace, global)
            .err()
            .map(|e| e.to_string())
    }

    /// A workspace no config declared is read-write, so one that contains the
    /// global config -- its file, or a directory above it -- is refused, as a
    /// repo mount of it is (#609). A file that is not there yet counts: an agent
    /// could create it. A directory beside the config or below it does not.
    #[test]
    fn an_implicit_workspace_containing_the_global_config_is_refused() {
        let tmp = tempdir().unwrap();
        let root = fs::canonicalize(tmp.path()).unwrap();
        let conf = root.join("proj/conf");
        fs::create_dir_all(conf.join("work")).unwrap();
        fs::create_dir_all(root.join("proj/src")).unwrap();
        let global = conf.join("config.toml");

        for present in [false, true] {
            if present {
                fs::write(&global, "").unwrap();
            }
            for workspace in [conf.clone(), root.join("proj"), root.clone()] {
                let err = workspace_refusal(&workspace, Some(&global))
                    .unwrap_or_else(|| panic!("{} must be refused", workspace.display()));
                assert_eq!(
                    err,
                    format!(
                        "configuration: refusing to mount {} as the workspace: it is read-write \
                         and covers the global config ({}), and no repo config was found, so it \
                         is the current directory\n\
                         help: run outrig from a project directory that does not hold the \
                         global config, or declare [workspace] host-path in the global config \
                         to mount it on purpose",
                        workspace.display(),
                        global.display()
                    ),
                );
            }
            for workspace in [conf.join("work"), root.join("proj/src"), root.join("other")] {
                assert_eq!(
                    workspace_refusal(&workspace, Some(&global)),
                    None,
                    "{} must be allowed",
                    workspace.display()
                );
            }
        }
        assert_eq!(workspace_refusal(&conf, None), None);
        assert_eq!(workspace_refusal(&conf, Some(Path::new("/dev/null"))), None);
    }

    /// The refusal says which default put the workspace there, as the home
    /// refusal does: a repo config at the root, or no repo config at all.
    #[test]
    fn global_config_refusal_names_the_default_that_chose_it() {
        let tmp = tempdir().unwrap();
        let root = fs::canonicalize(tmp.path()).unwrap();
        let global = root.join("config.toml");

        let err = workspace_refusal(&root, Some(&global)).unwrap();
        assert!(
            err.contains(", and no repo config was found, so it is the current directory\n"),
            "{err}",
        );

        let config = write_repo_config(&root);
        let err = workspace_refusal(&root, Some(&global)).unwrap();
        assert!(
            err.contains(&format!(
                ", and {} makes it the repo root\n",
                config.display()
            )),
            "{err}",
        );
    }

    /// A directory the global config path is looked up through is as good as
    /// the file: an agent that can write it replaces a symlink there, or
    /// creates a missing component that a `..` after it had popped (#605).
    #[test]
    fn an_implicit_workspace_where_the_global_config_path_is_looked_up_is_refused() {
        let tmp = tempdir().unwrap();
        let root = fs::canonicalize(tmp.path()).unwrap();
        let (proj, real) = (root.join("proj"), root.join("real"));
        fs::create_dir_all(&proj).unwrap();
        fs::create_dir_all(real.join("outrig")).unwrap();
        fs::write(real.join("outrig/config.toml"), "").unwrap();
        std::os::unix::fs::symlink(&real, proj.join("cfg")).unwrap();
        let global = proj.join("cfg/outrig/config.toml");

        let err = workspace_refusal(&proj, Some(&global)).expect("the symlink's directory");
        assert!(err.contains(&looked_up_through(&proj)), "{err}");
        assert!(err.contains(&global.display().to_string()), "{err}");
        // Where the file is today keeps its own reason.
        let err = workspace_refusal(&real, Some(&global)).expect("the resolved file");
        assert!(err.contains("covers the global config ("), "{err}");

        let shared = root.join("shared");
        fs::create_dir_all(&shared).unwrap();
        let global = shared.join("absent/../../conf/outrig/config.toml");
        let err = workspace_refusal(&shared, Some(&global)).expect("the creation point");
        assert!(err.contains(&looked_up_through(&shared)), "{err}");
    }

    /// The workspace is compared by where it resolves, so a symlink to the
    /// global config's directory is refused like the directory, and named as
    /// the directory.
    #[test]
    fn a_symlinked_spelling_of_the_workspace_is_compared_by_where_it_resolves() {
        let tmp = tempdir().unwrap();
        let root = fs::canonicalize(tmp.path()).unwrap();
        let conf = root.join("conf");
        fs::create_dir_all(&conf).unwrap();
        let link = root.join("link-to-conf");
        std::os::unix::fs::symlink(&conf, &link).unwrap();

        let err = workspace_refusal(&link, Some(&conf.join("config.toml"))).expect("resolves");
        assert!(
            err.contains(&format!(
                "refusing to mount {} as the workspace",
                conf.display()
            )),
            "{err}",
        );
    }

    #[test]
    fn global_config_path_xdg_overrides_home_default() {
        let xdg = Path::new("/xdg/conf");
        let home = Path::new("/home/alice");
        let p = global_config_path_with(None, Some(xdg), Some(home)).unwrap();
        assert_eq!(p, Path::new("/xdg/conf/outrig/config.toml"));
    }

    #[test]
    fn global_config_path_flag_overrides_xdg() {
        let flag = Path::new("/explicit/global.toml");
        let xdg = Path::new("/xdg/conf");
        let home = Path::new("/home/alice");
        let p = global_config_path_with(Some(flag), Some(xdg), Some(home)).unwrap();
        assert_eq!(p, flag);
    }

    #[test]
    fn global_config_path_home_default_when_xdg_unset() {
        let home = Path::new("/home/alice");
        let p = global_config_path_with(None, None, Some(home)).unwrap();
        assert_eq!(p, Path::new("/home/alice/.outrig/config.toml"));
    }

    /// #368: an `XDG_CONFIG_HOME` that is empty or relative is unset, so the
    /// global config is the home default and not a file under the working
    /// directory, as the XDG Base Directory Specification says.
    #[test]
    fn global_config_path_ignores_an_xdg_config_home_that_is_not_absolute() {
        let home = Path::new("/home/alice");
        for xdg in ["", "conf", "./conf", "~/.config"] {
            let p = global_config_path_with(None, Some(Path::new(xdg)), Some(home));
            assert_eq!(
                p.as_deref(),
                Some(Path::new("/home/alice/.outrig/config.toml")),
                "XDG_CONFIG_HOME={xdg:?}",
            );
        }
    }

    /// #368: a home that is missing, empty, or relative names no place, and
    /// with no absolute `XDG_CONFIG_HOME` either there is no location -- not
    /// one resolved against the working directory. An absolute
    /// `XDG_CONFIG_HOME` needs no home, and the flag needs neither.
    #[test]
    fn global_config_path_needs_an_absolute_xdg_config_home_or_home() {
        let flag = Path::new("global.toml");
        for home in [None, Some(Path::new("")), Some(Path::new("alice"))] {
            for xdg in [None, Some(Path::new("")), Some(Path::new("conf"))] {
                assert_eq!(
                    global_config_path_with(None, xdg, home),
                    None,
                    "XDG_CONFIG_HOME={xdg:?}, home={home:?}",
                );
            }
            let p = global_config_path_with(None, Some(Path::new("/xdg")), home);
            assert_eq!(p.as_deref(), Some(Path::new("/xdg/outrig/config.toml")));
            let p = global_config_path_with(Some(flag), None, home);
            assert_eq!(p.as_deref(), Some(flag));
        }
    }

    /// With no global config there is nothing to read, and what comes back is
    /// an empty config, as it is for a file that is not there.
    #[test]
    fn no_global_config_loads_as_an_empty_one() {
        let tmp = tempdir().unwrap();
        let missing = tmp.path().join("config.toml");

        assert_eq!(load_global_or_empty(None).unwrap(), Config::default());
        assert_eq!(
            load_global_or_empty(Some(&missing)).unwrap(),
            Config::default()
        );
    }

    /// #342: `--global-config` names a file outright, so one that is not there
    /// -- a typo, a directory that is missing, a dangling link -- is refused,
    /// naming the path. Whatever is there goes on to the read, `/dev/null`
    /// included, and the default is never checked.
    #[test]
    fn explicit_global_config_that_does_not_exist_is_refused() {
        let tmp = tempdir().unwrap();
        let file = tmp.path().join("config.toml");
        fs::write(&file, b"").unwrap();
        let link = tmp.path().join("link.toml");
        std::os::unix::fs::symlink(&file, &link).unwrap();
        let dangling = tmp.path().join("dangling.toml");
        std::os::unix::fs::symlink(tmp.path().join("gone.toml"), &dangling).unwrap();

        for missing in [
            tmp.path().join("confg.toml"),
            tmp.path().join("no-such-dir/config.toml"),
            dangling,
        ] {
            let err = resolve_global_config(Some(&missing)).unwrap_err();
            assert_eq!(
                err.to_string(),
                format!(
                    "configuration: --global-config {} does not exist",
                    missing.display()
                ),
            );
        }
        for present in [
            file,
            link,
            tmp.path().to_path_buf(),
            PathBuf::from("/dev/null"),
        ] {
            assert_eq!(
                resolve_global_config(Some(&present)).unwrap(),
                Some(present)
            );
        }
        assert_eq!(
            resolve_global_config(None).unwrap(),
            global_config_path(None).ok()
        );
    }
}
