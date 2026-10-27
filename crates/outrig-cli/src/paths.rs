//! CLI-owned repo, config, and cache path policy.

use std::io::Write as _;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};

use directories::{BaseDirs, ProjectDirs};
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
        if cur.join(REPO_CONFIG_REL).is_file() {
            refuse_foreign_repo_config(cur, uid)?;
            return Ok(cur.to_path_buf());
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
fn stage(path: &Path, contents: &str) -> Result<NamedTempFile> {
    let parent = path.parent().ok_or_else(|| {
        OutrigError::Configuration(format!("path has no parent: {}", path.display()))
    })?;
    std::fs::create_dir_all(parent).path_ctx("create directory", parent)?;
    let mut tmp = NamedTempFile::new_in(parent)?;
    tmp.write_all(contents.as_bytes())?;
    tmp.as_file().sync_all()?;
    Ok(tmp)
}

/// The repo config a command reads, and the repo it runs against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoConfig {
    /// The repo: the default workspace, the session's working directory, and
    /// what `model-path` resolves against.
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
/// receives it: `Path::new("").exists()` is false, and mistralrs-core reads an
/// empty model directory as a Hugging Face repo ID.
fn explicit_repo_config(path: &Path, cwd: &Path) -> Result<RepoConfig> {
    let file: PathBuf = cwd.join(path).components().collect();
    if !file.is_file() {
        return Err(OutrigError::Configuration(format!(
            "--config {} is not an existing file",
            path.display()
        )));
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
pub(crate) fn pin_repo_declared_mounts(
    cfg: &mut Config,
    repo_root: &Path,
    global_cfg: Option<&Path>,
) -> Result<Vec<PinnedMount>> {
    let home = BaseDirs::new().map(|d| d.home_dir().to_path_buf());
    pin_repo_declared_mounts_with(cfg, repo_root, home.as_deref(), global_cfg)
}

/// [`pin_repo_declared_mounts`] against an explicit `home` (`None` skips the
/// home rule). Paths are compared canonicalized, as in
/// [`refuse_home_workspace_with`].
fn pin_repo_declared_mounts_with(
    cfg: &mut Config,
    repo_root: &Path,
    home: Option<&Path>,
    global_cfg: Option<&Path>,
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
        let reason = if home.as_ref().is_some_and(|h| h.starts_with(&host)) {
            "it is your home directory or above it".to_string()
        } else if access == MountAccess::ReadWrite
            && let Some(why) = global.as_ref().and_then(|g| g.rewritable_through(&host))
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
        Err(OutrigError::Configuration(format!(
            "refusing {what} {} declared in {}: {reason}, so an agent could edit files \
             that govern the next session\n\
             help: a repo config may not do this; declare it in the global config if you \
             mean it, or pass --volume",
            host.display(),
            source.config_path().display()
        )))
    };
    let workspace = &mut cfg.workspace;
    if let Some(host) = check(
        "workspace host-path",
        workspace.config_source(),
        workspace.resolved_host_path(repo_root),
        MountAccess::ReadWrite,
    )? {
        workspace.set_host_path(host);
    }
    for m in &mut workspace.mounts {
        if let Some(host) = check(
            "mount",
            m.config_source(),
            m.resolved_host_path(repo_root),
            m.access(),
        )? {
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
                m.set_host_path(host);
            }
        }
    }
    drop(check);
    Ok(pinned)
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

pub(crate) fn model_cache_root(from_config: Option<&Path>) -> PathBuf {
    if let Some(p) = from_config {
        return p.to_path_buf();
    }
    if let Some(dirs) = ProjectDirs::from("", "", "outrig") {
        return dirs.cache_dir().join("models");
    }
    std::env::temp_dir().join("outrig-models")
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
    /// above `.agents/outrig/config.toml`, which mistralrs-core would read as
    /// a Hugging Face repo ID for a bare `model-path`.
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
        pin_repo_declared_mounts_with(&mut cfg, &repo, None, Some(global))
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
        let pinned = pin_repo_declared_mounts_with(&mut cfg, &repo, None, Some(global)).unwrap();
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
